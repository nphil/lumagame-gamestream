mod host;
mod http;
mod pair;
mod state;
mod stream;
mod wire;

use host::{ServerInfo, TIMEOUT};
use lumagame_core::{engine::{AuthMethod, Capabilities, Codec, Descriptor, EngineKind, Feature, Presentation, TargetKind, VideoCaps}, ids::{EngineId, ProviderGameId, SessionId, TargetId}, ipc::{self, AuthFail, AuthState, EndReason, ErrorCode, Event, FailReason, Hello, LibraryEntry, Link, Reach, Readiness, Request, SessionState, StreamInfo, Target, ToEngine}};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use state::{Cancel, Error, Host, Hosts, Identity, Kind, Result};
use std::{collections::{BTreeMap, BTreeSet, HashMap}, io::Read, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, mpsc::{self, Sender}}, thread, time::Duration};
use wire::Output;

pub enum Message { Request(ToEngine), Feedback(stream::Feedback), Found(String), Removed(String), Eof }
#[derive(Default)]
struct Shared {
    operation: Mutex<Option<Arc<Cancel>>>, pairing: Mutex<Option<Arc<Cancel>>>, session: Mutex<Option<(SessionId, Arc<Cancel>)>>,
    native_starting: AtomicBool, closing: AtomicBool, end_game: AtomicBool,
    stops: Mutex<BTreeMap<SessionId, bool>>,
}
impl Shared {
    fn begin_session(&self, id: SessionId, cancel: Arc<Cancel>) {
        self.end_game.store(false, Ordering::Release);
        *self.session.lock().unwrap() = Some((id, cancel.clone()));
        if let Some(end_game) = self.stops.lock().unwrap().remove(&id) {
            self.end_game.fetch_or(end_game, Ordering::AcqRel);
            cancel.stop();
        }
    }
    fn cancel(&self, request: Option<&Request>) {
        if let Some(Request::Stop { session, end_game }) = request {
            self.stops.lock().unwrap().entry(*session).and_modify(|old| *old |= *end_game).or_insert(*end_game);
        }
        let stop_session = match request { Some(Request::Stop { session, .. }) => Some(*session), None => self.session.lock().unwrap().as_ref().map(|s| s.0), _ => None };
        if let Some(id) = stop_session {
            if let Some((session, cancel)) = self.session.lock().unwrap().as_ref() {
                if *session == id {
                    if matches!(request, Some(Request::Stop { end_game: true, .. })) { self.end_game.store(true, Ordering::Release); }
                    cancel.stop();
                    if self.native_starting.load(Ordering::Acquire) { stream::interrupt(); }
                }
            }
        }
        if request.is_none() || matches!(request, Some(Request::AuthCancel)) {
            if let Some(cancel) = self.pairing.lock().unwrap().as_ref() { cancel.stop(); }
        }
        if request.is_none() {
            self.closing.store(true, Ordering::Release);
            if let Some(cancel) = self.operation.lock().unwrap().as_ref() { cancel.stop(); }
        }
    }
}
struct Live { id: SessionId, target: TargetId, game: u32, native: stream::NativeSession, stream: StreamInfo, running: bool }
struct Engine {
    hello: Hello, identity: Identity, hosts: Hosts, output: Output, shared: Arc<Shared>, sender: Sender<Message>,
    library: BTreeMap<TargetId, Vec<LibraryEntry>>, live: Option<Live>, discovery: Option<ServiceDaemon>,
}
fn error(output: &Output, re: Option<u64>, code: ErrorCode, message: impl Into<String>) { output.send(re, Event::Error { code, message: message.into() }); }
fn target_id(host: &Host) -> TargetId { TargetId::new(&host.id).expect("serverinfo checked its id") }
fn failure(e: &Error) -> FailReason {
    match e.kind { Kind::Auth => FailReason::Auth, Kind::Denied => FailReason::NotEntitled, Kind::Busy => FailReason::Busy, Kind::Video => FailReason::Video, Kind::Timeout => FailReason::Timeout, Kind::NotInstalled => FailReason::NotInstalled, Kind::Network => FailReason::Network, _ => FailReason::Engine }
}
fn auth_failure(e: &Error) -> AuthFail {
    match e.kind { Kind::Cancelled => AuthFail::Cancelled, Kind::Timeout => AuthFail::Expired, Kind::Auth if e.message == "The PIN did not match" => AuthFail::WrongPin, Kind::Denied | Kind::Auth => AuthFail::Denied, _ => AuthFail::Network }
}
fn descriptor() -> Descriptor {
    Descriptor { id: EngineId::new("gamestream").unwrap(), version: env!("CARGO_PKG_VERSION").into(), capabilities: Capabilities {
        kind: EngineKind::Host, targets: TargetKind::Host, auth: vec![AuthMethod::ShowPin], presentation: Presentation::Feeder,
        features: BTreeSet::from([Feature::Discovery, Feature::ManualHost, Feature::Wake, Feature::ManyTargets, Feature::Readiness, Feature::Resume, Feature::EndGame, Feature::Stats, Feature::OverlayKeys, Feature::Rumble, Feature::Surround]),
        video: Some(VideoCaps { codecs: vec![Codec::H264, Codec::H265, Codec::Av1], width: 3840, height: 2160, fps: 120, hdr10: true }),
    } }
}
impl Engine {
    fn open(mut hello: Hello, output: Output, shared: Arc<Shared>, sender: Sender<Message>) -> Result<Self> {
        hello.settings = host::schema().resolve(&hello.settings);
        let identity = Identity::load(Path::new(&hello.data_dir))?;
        let hosts = Hosts::load(Path::new(&hello.data_dir))?;
        Ok(Self { hello, identity, hosts, output, shared, sender, library: BTreeMap::new(), live: None, discovery: None })
    }
    fn discover(&mut self) -> Result<()> {
        if let Some(daemon) = self.discovery.take() { let _ = daemon.shutdown(); }
        let daemon = ServiceDaemon::new().map_err(|e| Error::new(Kind::Network, format!("PC discovery could not start: {e}")))?;
        let receiver = daemon.browse("_nvstream._tcp.local.").map_err(|e| Error::new(Kind::Network, e.to_string()))?;
        let sender = self.sender.clone();
        thread::Builder::new().name("gamestream-mdns".into()).spawn(move || {
            let mut addresses = HashMap::new();
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        if let Some(ip) = info.addresses.iter().find(|a| a.is_ipv4()).or_else(|| info.addresses.iter().next()) {
                            let address = if ip.is_ipv4() { format!("{ip}:{}", info.port) } else { format!("[{ip}]:{}", info.port) };
                            if addresses.get(&info.fullname) != Some(&address) {
                                addresses.insert(info.fullname.clone(), address.clone());
                                if sender.send(Message::Found(address)).is_err() { break; }
                            }
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, name) => { if let Some(address) = addresses.remove(&name) { if sender.send(Message::Removed(address)).is_err() { break; } } }
                    _ => {},
                }
            }
        })?;
        self.discovery = Some(daemon);
        Ok(())
    }
    fn targets(&self, re: Option<u64>) {
        let targets = self.hosts.hosts.iter().map(|h| Target {
            id: target_id(h), name: h.name.clone(), detail: Some(h.address.clone()),
            link: if h.certificate.is_some() { Link::Linked } else if h.ever_linked { Link::Expired } else { Link::Found },
            reach: if h.online { Reach::Online } else if h.mac.is_some() { Reach::Asleep } else { Reach::Offline },
        }).collect();
        self.output.send(re, Event::Targets { targets });
    }
    fn add(&mut self, address: String, cancel: &Cancel) -> Result<()> {
        let parsed = http::Address::parse(&address)?;
        let address = parsed.authority(parsed.port);
        if let Some(old) = self.hosts.hosts.iter().find(|h| h.address == address).cloned() {
            let mut host = old;
            host::server_info(&self.identity, &mut host, cancel)?;
            self.hosts.put(host)?;
            return Ok(());
        }
        let mut host = Host { id: String::new(), name: String::new(), address, https_port: 47984, certificate: None, mac: None, online: false, ever_linked: false };
        let info = host::server_info(&self.identity, &mut host, cancel)?;
        if let Some(old) = self.hosts.hosts.iter().find(|h| h.id == info.id) {
            host.certificate.clone_from(&old.certificate); host.mac.clone_from(&old.mac);
            host.ever_linked = old.ever_linked;
            if host.certificate.is_some() { host::server_info(&self.identity, &mut host, cancel)?; }
        }
        self.hosts.put(host)
    }
    fn refresh(&mut self, target: &TargetId, cancel: &Cancel) -> Result<(Host, ServerInfo)> {
        let mut host = self.hosts.get(target.as_str())?;
        let result = host::server_info(&self.identity, &mut host, cancel);
        if result.is_err() { host.online = false; }
        self.hosts.put(host.clone())?;
        self.targets(None);
        Ok((host, result?))
    }
    fn pair(&mut self, target: TargetId, seq: u64, cancel: Arc<Cancel>) -> Result<Host> {
        let mut host = self.hosts.get(target.as_str())?;
        *self.shared.pairing.lock().unwrap() = Some(cancel.clone());
        let result = (|| {
            host::server_info(&self.identity, &mut host, &cancel)?;
            let pin = format!("{:04}", u32::from_le_bytes(state::random::<4>()?) % 10000);
            self.output.send(Some(seq), Event::Auth { state: AuthState::ShowPin { pin: pin.clone() } });
            pair::link(&self.identity, &mut host, &pin, &cancel)?;
            host.ever_linked = true;
            host::server_info(&self.identity, &mut host, &cancel)?;
            self.hosts.put(host.clone())?;
            self.output.send(Some(seq), Event::Auth { state: AuthState::Done { target } });
            self.targets(None);
            Ok(host)
        })();
        *self.shared.pairing.lock().unwrap() = None;
        if let Err(e) = &result { self.output.send(Some(seq), Event::Auth { state: AuthState::Failed { reason: auth_failure(e), message: e.to_string() } }); }
        result
    }
    fn fetch_library(&mut self, target: &TargetId, cancel: &Cancel) -> Result<Vec<LibraryEntry>> {
        let (host, info) = self.refresh(target, cancel)?;
        if host.certificate.is_none() || !info.paired { return Err(Error::new(Kind::Auth, "Link this PC before opening its library")); }
        let xml = http::Client { identity: &self.identity, host: &host, cancel }.xml(true, "applist", "", TIMEOUT)?;
        host::apps(&xml, &info)
    }
    fn launch(&mut self, seq: u64, id: SessionId, target: TargetId, game: ProviderGameId) {
        if self.live.is_some() { self.output.send(Some(seq), Event::Session { session: id, state: SessionState::Failed { reason: FailReason::Busy, message: "A stream is already running in this engine".into() } }); return; }
        let cancel = Arc::new(Cancel::default());
        self.shared.begin_session(id, cancel.clone());
        self.output.send(Some(seq), Event::Session { session: id, state: SessionState::Checking });
        let result = (|| -> Result<Live> {
            let app = game.as_str().parse::<u32>().ok().filter(|id| *id != 0).ok_or_else(|| Error::new(Kind::Invalid, "Invalid GameStream app ID"))?;
            let (mut host, mut info) = self.refresh(&target, &cancel)?;
            if host.certificate.is_none() || !info.paired {
                self.output.send(Some(seq), Event::Session { session: id, state: SessionState::SignIn });
                host = self.pair(target.clone(), seq, cancel.clone())?;
                info = host::server_info(&self.identity, &mut host, &cancel)?;
            }
            let entries = self.fetch_library(&target, &cancel)?;
            if !entries.iter().any(|e| e.id == game) { return Err(Error::new(Kind::NotInstalled, "This game is no longer in the PC's app list")); }
            if info.current_game != 0 && info.current_game != app { return Err(Error::new(Kind::Busy, "A different game is already running on this PC. End it before starting this game.")); }
            let options = host::Options::select(&self.hello.settings, &self.hello.display, &info)?;
            let feeder = self.hello.feeder.as_deref().ok_or_else(|| Error::new(Kind::Video, "The app did not supply a feeder socket"))?;
            let key = state::random()?;
            let mut iv = [0;16]; iv[..4].copy_from_slice(&state::random::<4>()?);
            self.output.send(Some(seq), Event::Session { session: id, state: SessionState::Starting });
            let verb = if info.current_game == app { "resume" } else { "launch" };
            let xml = http::Client { identity: &self.identity, host: &host, cancel: &cancel }.xml(true, verb, &options.launch_arguments(app, &key, &iv), Duration::from_secs(120))?;
            let doc = host::document(&xml)?;
            let root = doc.root_element();
            if host::text(root, if verb == "resume" { "resume" } else { "gamesession" }) != "1" { return Err(Error::new(Kind::Denied, format!("The PC did not accept {verb}"))); }
            let rtsp = host::text(root, "sessionUrl0");
            if !rtsp.starts_with("rtsp://") && !rtsp.starts_with("rtspenc://") { return Err(Error::new(Kind::Invalid, "The PC did not return a valid streaming URL")); }
            cancel.check()?;
            self.shared.native_starting.store(true, Ordering::Release);
            let native = stream::NativeSession::start(id, &host.address, &info, &options, rtsp, feeder, key, iv, self.sender.clone(), self.output.clone(), cancel.clone());
            self.shared.native_starting.store(false, Ordering::Release);
            let native = native?;
            cancel.check()?;
            Ok(Live { id, target: target.clone(), game: app, native, stream: StreamInfo { codec: options.codec, width: options.width, height: options.height, fps: options.fps, hdr: false, audio_channels: 0, sdr_reason: None }, running: false })
        })();
        match result {
            Ok(live) => self.live = Some(live),
            Err(e) => {
                *self.shared.session.lock().unwrap() = None;
                let state = if cancel.check().is_err() {
                    if self.shared.end_game.load(Ordering::Acquire) && !self.shared.closing.load(Ordering::Acquire) {
                        match game.as_str().parse::<u32>().map_err(|_| Error::new(Kind::Invalid, "Invalid game ID")).and_then(|game| self.quit(&target, game)) {
                            Ok(()) => SessionState::Ended { reason: EndReason::Stopped },
                            Err(e) => SessionState::Failed { reason: failure(&e), message: format!("The stream stopped, but the game could not be ended: {e}") },
                        }
                    } else { SessionState::Ended { reason: EndReason::Suspended } }
                } else { SessionState::Failed { reason: failure(&e), message: e.to_string() } };
                self.output.send(Some(seq), Event::Session { session: id, state });
            }
        }
    }
    fn quit(&mut self, target: &TargetId, game: u32) -> Result<()> {
        let cancel = Arc::new(Cancel::default());
        *self.shared.operation.lock().unwrap() = Some(cancel.clone());
        let (host, info) = self.refresh(target, &cancel)?;
        if info.current_game == 0 { return Ok(()); }
        if info.current_game != game { return Err(Error::new(Kind::Busy, "A different game now runs on this PC; it was not ended")); }
        let xml = http::Client { identity: &self.identity, host: &host, cancel: &cancel }.xml(true, "cancel", "", Duration::from_secs(30))?;
        if host::text(host::document(&xml)?.root_element(), "cancel") != "1" { return Err(Error::new(Kind::Denied, "The PC refused to end the game")); }
        let (_, info) = self.refresh(target, &cancel)?;
        if info.current_game != 0 { return Err(Error::new(Kind::Denied, "The PC kept the game running; this client may not own the session")); }
        Ok(())
    }
    fn stop(&mut self, seq: u64, session: SessionId, end_game: bool) -> Result<()> {
        self.shared.stops.lock().unwrap().remove(&session);
        let Some(live) = self.live.take() else { return Ok(()); };
        if live.id != session { self.live = Some(live); return Err(Error::new(Kind::Invalid, "That session is not running")); }
        let target = live.target.clone();
        let game = live.game;
        drop(live);
        *self.shared.session.lock().unwrap() = None;
        let result = if end_game { self.quit(&target, game) } else { Ok(()) };
        let state = match result {
            Ok(()) => SessionState::Ended { reason: if end_game { EndReason::Stopped } else { EndReason::Suspended } },
            Err(e) => SessionState::Failed { reason: failure(&e), message: format!("The stream stopped, but the game could not be ended: {e}") },
        };
        self.output.send(Some(seq), Event::Session { session, state });
        Ok(())
    }
    fn request(&mut self, message: ToEngine) -> Result<()> {
        let seq = message.seq;
        let cancel = Arc::new(Cancel::default());
        *self.shared.operation.lock().unwrap() = Some(cancel.clone());
        match message.request {
            Request::Hello(_) => return Err(Error::new(Kind::Invalid, "hello was already received")),
            Request::Settings { values } => self.hello.settings = host::schema().resolve(&values),
            Request::Targets => {
                let targets: Vec<_> = self.hosts.hosts.iter().map(target_id).collect();
                for target in targets { cancel.check()?; let _ = self.refresh(&target, &cancel); }
                self.targets(Some(seq));
            }
            Request::AddTarget { address } => { self.add(address, &cancel)?; self.targets(Some(seq)); }
            Request::Forget { target } => {
                if self.live.as_ref().is_some_and(|l| l.target == target) { return Err(Error::new(Kind::Busy, "Stop the stream before forgetting its PC")); }
                let host = self.hosts.get(target.as_str())?;
                if host.certificate.is_some() {
                    if let Err(e) = (http::Client { identity: &self.identity, host: &host, cancel: &cancel }).xml(false, "unpair", "", TIMEOUT) {
                        error(&self.output, Some(seq), ErrorCode::Internal, format!("The pairing was removed locally, but could not be revoked on the PC: {e}"));
                    }
                }
                self.hosts.hosts.retain(|h| h.id != target.as_str()); self.hosts.save()?; self.library.remove(&target); self.targets(Some(seq));
            }
            Request::Wake { target } => { host::wake(&self.hosts.get(target.as_str())?)?; self.targets(Some(seq)); }
            Request::Auth { target, method } => {
                if method != AuthMethod::ShowPin { error(&self.output, Some(seq), ErrorCode::Unsupported, "Sunshine/Apollo uses a PIN shown on the TV and entered on the PC"); return Ok(()); }
                if self.live.is_some() { return Err(Error::new(Kind::Busy, "Stop the stream before linking a PC")); }
                let target = target.or_else(|| (self.hosts.hosts.len() == 1).then(|| target_id(&self.hosts.hosts[0]))).ok_or_else(|| Error::new(Kind::Invalid, "Choose or add a PC to link"))?;
                let _ = self.pair(target, seq, cancel);
            }
            Request::AuthPin { .. } => error(&self.output, Some(seq), ErrorCode::Unsupported, "Enter the PIN on the PC, not on the TV"),
            Request::AuthCancel => {}, // The stdin reader cancels a blocked PIN request immediately.
            Request::Library { target, cursor } => {
                let start = if let Some(cursor) = cursor { cursor.parse::<usize>().map_err(|_| Error::new(Kind::Invalid, "Invalid library cursor"))? } else { let entries = self.fetch_library(&target, &cancel)?; self.library.insert(target.clone(), entries); 0 };
                let entries = self.library.get(&target).ok_or_else(|| Error::new(Kind::Invalid, "Request the first library page before continuing"))?;
                if start > entries.len() { return Err(Error::new(Kind::Invalid, "Library cursor exceeds its last page")); }
                let end = start.saturating_add(200).min(entries.len());
                self.output.send(Some(seq), Event::Library { target, entries: entries[start..end].to_vec(), next: (end < entries.len()).then(|| end.to_string()) });
            }
            Request::Check { target, game } => {
                let result = self.fetch_library(&target, &cancel);
                let readiness = match result {
                    Ok(entries) => if entries.iter().any(|e| e.id == game) { Readiness::Ready } else { Readiness::Install },
                    Err(e) if e.kind == Kind::Auth => Readiness::SignIn,
                    Err(e) if e.kind == Kind::Denied => { error(&self.output, Some(seq), ErrorCode::BadRequest, e.to_string()); Readiness::Unknown }
                    Err(_) => if self.hosts.get(target.as_str()).is_ok_and(|h| h.mac.is_some()) { Readiness::Asleep } else { Readiness::Offline },
                };
                self.output.send(Some(seq), Event::Readiness { target, game, readiness });
            }
            Request::Launch { session, target, game } => self.launch(seq, session, target, game),
            Request::Stop { session, end_game } => self.stop(seq, session, end_game)?,
            Request::Route { session, input } => {
                let live = self.live.as_ref().filter(|l| l.id == session).ok_or_else(|| Error::new(Kind::Invalid, "That session is not running"))?;
                live.native.route(input)?;
            }
            Request::Stats { session, on, pad_timing } => {
                let live = self.live.as_mut().filter(|l| l.id == session).ok_or_else(|| Error::new(Kind::Invalid, "That session is not running"))?;
                live.native.stats(on, pad_timing);
            }
            Request::Action { key } if key == "discover" => self.discover()?,
            Request::Action { .. } => error(&self.output, Some(seq), ErrorCode::Unsupported, "Unknown settings action"),
            Request::RoundTrip { .. } => error(&self.output, Some(seq), ErrorCode::Unsupported, "This engine does not expose round-trip measurements"),
        }
        *self.shared.operation.lock().unwrap() = None;
        Ok(())
    }
    fn feedback(&mut self, f: stream::Feedback) {
        let Some(live) = self.live.as_mut().filter(|l| l.id == f.session) else { return; };
        match f.kind {
            1 => { live.stream.codec = if f.a & 0xf != 0 { Codec::H264 } else if f.a & 0xf00 != 0 { Codec::H265 } else { Codec::Av1 }; live.stream.width = f.b as u32; live.stream.height = f.c as u32; live.stream.fps = f.d as u32; }
            2 | 6 => {
                live.stream.hdr = f.a != 0;
                if f.kind == 2 { live.running = true; }
                if live.running { self.output.told(Event::Session { session: f.session, state: SessionState::Running { stream: Some(live.stream.clone()) } }); }
            }
            3 | 4 => {
                self.live = None; *self.shared.session.lock().unwrap() = None;
                let state = if f.kind == 3 && f.a == 0 { SessionState::Ended { reason: EndReason::Remote } } else { SessionState::Failed { reason: if f.kind == 4 { FailReason::Video } else { FailReason::Network }, message: if f.kind == 4 { f.message } else { format!("The PC stream ended with Moonlight error {}", f.a) } } };
                self.output.told(Event::Session { session: f.session, state });
            }
            5 => live.native.rumble(f.a as u8, f.b as u16, f.c as u16),
            7 => eprintln!("Moonlight stage {} failed ({})", f.message, f.a),
            9 => live.stream.audio_channels = f.a as u8,
            _ => {},
        }
    }
}
impl Drop for Engine { fn drop(&mut self) { self.live = None; if let Some(d) = self.discovery.take() { let _ = d.shutdown(); } } }

fn input(sender: Sender<Message>, output: Output, shared: Arc<Shared>) {
    let mut lines = ipc::LineBuffer::default(); let mut buf = [0;8192];
    let mut input = std::io::stdin().lock();
    loop {
        let n = match input.read(&mut buf) { Ok(0) | Err(_) => break, Ok(n) => n };
        let dropped = lines.dropped;
        lines.push(&buf[..n], |line| match serde_json::from_str::<ToEngine>(line) {
            Ok(message) => { shared.cancel(Some(&message.request)); let _ = sender.send(Message::Request(message)); }
            Err(e) => {
                let parsed = serde_json::from_str::<serde_json::Value>(line).ok();
                let re = parsed.as_ref().and_then(|v| v.get("seq")).and_then(|v| v.as_u64());
                let kind = parsed.as_ref().and_then(|v| v.get("type")).and_then(|v| v.as_str());
                let unknown = kind.is_some_and(|s| !["hello","settings","targets","add-target","forget","wake","auth","auth-pin","auth-cancel","library","check","launch","stop","route","stats","action"].contains(&s));
                error(&output, re, if unknown { ErrorCode::Unsupported } else { ErrorCode::BadRequest }, if unknown { format!("Unknown request type: {}", kind.unwrap()) } else { format!("Invalid engine request: {e}") });
            }
        });
        if lines.dropped != dropped { error(&output, None, ErrorCode::BadRequest, "An input line exceeded 1 MiB or was not UTF-8"); }
    }
    shared.cancel(None); let _ = sender.send(Message::Eof);
}
fn main() {
    if std::env::args().any(|a| a == "--version") { println!("lumagame-gamestream {}", env!("CARGO_PKG_VERSION")); return; }
    let output = Output::new(); let shared = Arc::new(Shared::default()); let (sender, receiver) = mpsc::channel();
    let (tx, out, ctrl) = (sender.clone(), output.clone(), shared.clone());
    thread::spawn(move || input(tx, out, ctrl));
    let mut engine: Option<Engine> = None;
    while !shared.closing.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(Message::Request(message)) => {
                let seq = message.seq;
                if let Some(engine) = engine.as_mut() {
                    if let Err(e) = engine.request(message) { error(&output, Some(seq), ErrorCode::BadRequest, e.to_string()); }
                } else if let Request::Hello(hello) = message.request {
                    if hello.protocol != ipc::PROTOCOL { error(&output, Some(seq), ErrorCode::Protocol, format!("Engine protocol {} is required", ipc::PROTOCOL)); break; }
                    match Engine::open(hello, output.clone(), shared.clone(), sender.clone()) {
                        Ok(mut opened) => {
                            output.send(Some(seq), Event::Ready { descriptor: descriptor(), schema: host::schema() }); opened.targets(None);
                            if let Err(e) = opened.discover() { error(&output, None, ErrorCode::Internal, e.to_string()); }
                            engine = Some(opened);
                        }
                        Err(e) => { error(&output, Some(seq), ErrorCode::Internal, format!("Could not open engine state: {e}")); break; }
                    }
                } else { error(&output, Some(seq), ErrorCode::Protocol, "Send hello before any other request"); }
            }
            Ok(Message::Feedback(f)) => if let Some(e) = engine.as_mut() { e.feedback(f); },
            Ok(Message::Found(address)) => if let Some(e) = engine.as_mut() {
                let cancel = Arc::new(Cancel::default()); *shared.operation.lock().unwrap() = Some(cancel.clone());
                if let Err(error) = e.add(address, &cancel) { eprintln!("PC discovery: {error}"); } else { e.targets(None); }
                *shared.operation.lock().unwrap() = None;
            },
            Ok(Message::Removed(address)) => if let Some(e) = engine.as_mut() { for h in &mut e.hosts.hosts { if h.address == address { h.online = false; } } e.targets(None); },
            Ok(Message::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {},
        }
        if let Some(e) = engine.as_mut() { if let Some(live) = e.live.as_mut() { if let Some(stats) = live.native.poll_stats() { output.told(Event::Stats { session: live.id, stats }); } } }
    }
    shared.cancel(None);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_and_capabilities_match_the_core_protocol() {
        let manifest = lumagame_core::engine::Manifest::parse("gamestream", include_str!("../engine.json")).unwrap();
        assert_eq!(manifest.id, descriptor().id);
        assert_eq!(manifest.protocol, ipc::PROTOCOL);
        assert!(descriptor().capabilities.has(Feature::EndGame));
        assert!(descriptor().capabilities.has(Feature::Surround));
    }
    #[test]
    fn stop_cancels_a_queued_or_inflight_launch_but_not_another_session() {
        let shared = Shared::default();
        shared.cancel(Some(&Request::Stop { session: SessionId(8), end_game: true }));
        let queued = Arc::new(Cancel::default());
        shared.begin_session(SessionId(8), queued.clone());
        assert!(queued.check().is_err());
        assert!(shared.end_game.load(Ordering::Acquire));
        let active = Arc::new(Cancel::default());
        shared.begin_session(SessionId(9), active.clone());
        shared.cancel(Some(&Request::Stop { session: SessionId(7), end_game: false }));
        assert!(active.check().is_ok());
        shared.cancel(Some(&Request::Stop { session: SessionId(9), end_game: false }));
        assert!(active.check().is_err());
        assert!(!shared.end_game.load(Ordering::Acquire));
    }
}
