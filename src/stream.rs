use crate::{host::{Options, ServerInfo}, http::Address, state::{Cancel, Error, Kind, Result}, wire::Output};
use lumagame_core::{ids::SessionId, ipc::{Event, InputRoute, PadKey, PadStats, Stats}};
use lumatv_gfn_pad::{PadConfig, PadEvent, PadRuntime, PadSink, PadState, RawReport, lab::Timing, latency, menu::{MenuKey, MenuKeys}};
use std::{ffi::{CStr, CString, c_char, c_int, c_void}, sync::{Arc, Mutex, mpsc::Sender}, time::{Duration, Instant}};

#[repr(C)]
struct Config {
    address: *const c_char, version: *const c_char, gfe_version: *const c_char, rtsp_url: *const c_char, feeder: *const c_char,
    width: c_int, height: c_int, fps: c_int, bitrate: c_int, format: c_int, server_codecs: c_int, key: [u8;16], iv: [u8;16],
}
#[repr(C)]
#[derive(Default)]
struct NativeStats { frames: u64, bytes: u64, client_us: u64, host_tenths: u64, host_samples: u64, rtt: u32, rtt_valid: c_int }
unsafe extern "C" {
    fn gs_start(config: *const Config, callback: extern "C" fn(*mut c_void, c_int, c_int, c_int, c_int, c_int, *const c_char), context: *mut c_void) -> c_int;
    fn gs_stop();
    fn gs_interrupt();
    fn gs_controller(slot: u8, mask: u16, buttons: u16, lt: u8, rt: u8, lx: i16, ly: i16, rx: i16, ry: i16);
    fn gs_set_stats(on: c_int);
    fn gs_get_stats(stats: *mut NativeStats);
    fn gs_launch_extensions() -> *const c_char;
}
pub fn launch_extensions() -> &'static str { unsafe { CStr::from_ptr(gs_launch_extensions()).to_str().unwrap() } }
pub fn interrupt() { unsafe { gs_interrupt() } }

#[derive(Debug)]
pub struct Feedback { pub session: SessionId, pub kind: i32, pub a: i32, pub b: i32, pub c: i32, pub d: i32, pub message: String }
struct Context { session: SessionId, sender: Sender<crate::Message>, cancel: Arc<Cancel>, failure: Mutex<Option<Error>> }
extern "C" fn callback(context: *mut c_void, kind: c_int, a: c_int, b: c_int, c: c_int, d: c_int, message: *const c_char) {
    // The boxed context is kept until LiStopConnection has joined all callbacks and the feeder reader has ended.
    let context = unsafe { &*(context as *const Context) };
    if kind == 8 {
        if context.cancel.check().is_err() { interrupt(); }
        return;
    }
    let message = if message.is_null() { String::new() } else { unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned() };
    if kind == 4 || kind == 7 {
        let mut failure = context.failure.lock().unwrap();
        if failure.is_none() { *failure = Some(Error::new(if kind == 4 { Kind::Video } else { Kind::Network }, if kind == 4 { message.clone() } else { format!("Moonlight stage {message} failed ({a})") })); }
    }
    let _ = context.sender.send(crate::Message::Feedback(Feedback { session: context.session, kind, a, b, c, d, message }));
}
#[derive(Default)]
struct TimedPad { name:String, connected:bool, timing:Timing, at:u64 }
struct InputState { active: bool, game: bool, mask: u16, menus: [MenuKeys;4], sampling:bool, timing:[TimedPad;4] }
impl InputState {
    fn sample(&mut self,on:bool,pad_timing:bool) {
        self.sampling=on && pad_timing;
        if !self.sampling {for pad in &mut self.timing {pad.timing=Timing::default();pad.at=0;}}
    }
    fn raw(&mut self,slot:u8,report:&RawReport) {
        if !self.active || !self.sampling {return;}
        if let Some(pad)=self.timing.get_mut(usize::from(slot)) {pad.timing.push(report.kernel_ns);pad.at=report.kernel_ns;}
    }
    fn stats(&self,now:u64)->Option<PadStats> {
        let pad=self.timing.iter().filter(|p|p.connected && p.at>0).max_by_key(|p|p.at)?;
        let s=pad.timing.summary(now);
        Some(PadStats {name:pad.name.clone(),reports:s.reports,rate_hz:s.rate_hz,p99_us:s.p99_us,
            bursts:s.bursts.count,burst_gap_us:s.bursts.gap_us,age_ms:now.saturating_sub(pad.at)/1_000_000})
    }
}
fn menu_key(key:MenuKey)->PadKey {
    match key {MenuKey::Up=>PadKey::Up,MenuKey::Down=>PadKey::Down,MenuKey::Left=>PadKey::Left,MenuKey::Right=>PadKey::Right,
        MenuKey::Ok=>PadKey::Ok,MenuKey::Back=>PadKey::Back,MenuKey::Menu=>PadKey::Menu}
}
struct Input { state: Mutex<InputState>, session: SessionId, output: Output }
fn controller(slot: u8, mask: u16, s: PadState) { unsafe { gs_controller(slot, mask & 0xf, s.buttons, s.left_trigger, s.right_trigger, s.left_x, s.left_y, s.right_x, s.right_y) } }
impl PadSink for Input {
    fn pad_event(&self, bitmap: u16, event: PadEvent) {
        let mut input = self.state.lock().unwrap();
        if !input.active { return; }
        input.mask = bitmap & 0xf;
        match event {
            PadEvent::Report { slot, state, .. } if input.game => controller(slot, bitmap, state),
            PadEvent::Report { slot, state, .. } => {
                if let Some(menu) = input.menus.get_mut(slot as usize) {
                    menu.feed(&state, |key| {
                        let key = menu_key(key);
                        self.output.told(Event::Key { key, down: true });
                        self.output.told(Event::Key { key, down: false });
                    });
                }
            }
            PadEvent::Connected {slot,name,..} => {
                if let Some(pad)=input.timing.get_mut(usize::from(slot)) {*pad=TimedPad {name,connected:true,..Default::default()};}
                controller(slot,bitmap,PadState::default());
            }
            PadEvent::Disconnected {slot} => {
                if let Some(pad)=input.timing.get_mut(usize::from(slot)) {*pad=TimedPad::default();}
                controller(slot,bitmap,PadState::default());
            }
            PadEvent::Guide { pressed: true, .. } | PadEvent::ExitChord { .. } => self.output.told(Event::Overlay { session: self.session }),
            _ => {},
        }
    }
    fn raw(&self,slot:u8,report:&RawReport) {self.state.lock().unwrap().raw(slot,report);}
}

pub struct NativeSession {
    // Drop order is explicit: disable the pad sink, join Moonlight/feeder callbacks, then free their context.
    _context: Box<Context>, input: Arc<Input>, pads: PadRuntime, stats: bool, last_stats: Instant,
    rumble: [(u16,u16);4], last_rumble: Instant,
}
impl NativeSession {
    #[allow(clippy::too_many_arguments)]
    pub fn start(session: SessionId, address: &str, info: &ServerInfo, options: &Options, rtsp: &str, feeder: &str, key: [u8;16], iv: [u8;16], sender: Sender<crate::Message>, output: Output, cancel: Arc<Cancel>) -> Result<Self> {
        let string = |s: &str| CString::new(s).map_err(|_| Error::new(Kind::Invalid, "A stream parameter contains a NUL byte"));
        let address = string(&Address::parse(address)?.host)?;
        let version = string(&info.version)?; let gfe = string(&info.gfe_version)?; let rtsp = string(rtsp)?; let feeder = string(feeder)?;
        let config = Config { address: address.as_ptr(), version: version.as_ptr(), gfe_version: gfe.as_ptr(), rtsp_url: rtsp.as_ptr(), feeder: feeder.as_ptr(), width: options.width as i32, height: options.height as i32, fps: options.fps as i32, bitrate: options.bitrate as i32, format: options.format, server_codecs: info.codecs as i32, key, iv };
        let mut context = Box::new(Context { session, sender, cancel, failure: Mutex::new(None) });
        let rc = unsafe { gs_start(&config, callback, &mut *context as *mut Context as *mut c_void) };
        let failure = context.failure.lock().unwrap().take();
        if rc != 0 { return Err(failure.unwrap_or_else(|| Error::new(Kind::Network, format!("Moonlight could not start the stream (error {rc}). Check Sunshine/Apollo and the PC firewall.")))); }
        if let Some(failure) = failure { unsafe { gs_stop() }; return Err(failure); }
        let input = Arc::new(Input { state: Mutex::new(InputState { active: true, game: true, mask: 0, menus: Default::default(), sampling:false, timing:std::array::from_fn(|_|TimedPad::default()) }), session, output });
        let pads = PadRuntime::start(PadConfig { boost: None, stats_every: Duration::from_secs(1), raw:true, ..PadConfig::default() }, input.clone());
        pads.set_grab(true);
        Ok(Self { _context: context, input, pads, stats: false, last_stats: Instant::now(), rumble: [(0,0);4], last_rumble: Instant::now() })
    }
    pub fn route(&self, route: InputRoute) -> Result<()> {
        if route == InputRoute::Other { return Err(Error::new(Kind::Invalid, "Unknown controller input destination")); }
        let mut input = self.input.state.lock().unwrap();
        let game = route == InputRoute::Game;
        if input.game != game {
            for slot in 0..4 { if input.mask & (1 << slot) != 0 { controller(slot, input.mask, PadState::default()); } }
            input.game = game;
            input.menus = Default::default();
        }
        Ok(())
    }
    pub fn stats(&mut self, on: bool, pad_timing: bool) {
        self.stats = on; self.last_stats = Instant::now(); self.input.state.lock().unwrap().sample(on,pad_timing);
        unsafe { gs_set_stats(i32::from(on)) }
    }
    pub fn poll_stats(&mut self) -> Option<Stats> {
        if self.last_rumble.elapsed() >= Duration::from_secs(30) {
            for (slot, &(low, high)) in self.rumble.iter().enumerate() { if low != 0 || high != 0 { let _ = self.pads.rumble(slot as u8, low, high, 65535); } }
            self.last_rumble = Instant::now();
        }
        if !self.stats || self.last_stats.elapsed() < Duration::from_secs(1) { return None; }
        let seconds = self.last_stats.elapsed().as_secs_f32(); self.last_stats = Instant::now();
        let mut s = NativeStats::default(); unsafe { gs_get_stats(&mut s) };
        Some(Stats { rtt_ms: (s.rtt_valid != 0).then_some(s.rtt as f32), fps: Some(s.frames as f32 / seconds), bitrate_kbps: Some((s.bytes as f32 * 8.0 / seconds / 1000.0) as u32), client_ms: (s.frames != 0).then(|| s.client_us as f32 / s.frames as f32 / 1000.0), host_ms: (s.host_samples != 0).then(|| s.host_tenths as f32 / s.host_samples as f32 / 10.0),
            pad:self.input.state.lock().unwrap().stats(latency::mono_ns()), ..Stats::default() })
    }
    pub fn rumble(&mut self, slot: u8, low: u16, high: u16) {
        if let Some(state) = self.rumble.get_mut(slot as usize) { *state = (low, high); let _ = self.pads.rumble(slot, low, high, 65535); }
    }
}
impl Drop for NativeSession {
    fn drop(&mut self) {
        {
            let mut input = self.input.state.lock().unwrap();
            for slot in 0..4 { if input.mask & (1 << slot) != 0 { controller(slot, input.mask, PadState::default()); let _ = self.pads.rumble(slot, 0, 0, 0); } }
            input.active = false;
        }
        self.pads.stop();
        unsafe { gs_stop() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timing_is_opt_in_clears_on_off_and_menu_y_is_one_press() {
        let mut input=InputState {active:true,game:false,mask:1,menus:Default::default(),sampling:false,timing:std::array::from_fn(|_|TimedPad::default())};
        input.timing[0].name="DualShock 4".into();input.timing[0].connected=true;
        let report=|ms:u64| RawReport {kernel_ns:ms*1_000_000,read_ns:ms*1_000_000,..Default::default()};
        input.raw(0,&report(1000));assert!(input.stats(1_000_000_000).is_none());
        input.sample(true,false);input.raw(0,&report(1000));assert!(input.stats(1_000_000_000).is_none());
        input.sample(true,true);
        for ms in [1000,1004,1008,1012] {input.raw(0,&report(ms));}
        let stats=input.stats(1_014_000_000).unwrap();
        assert_eq!((stats.name.as_str(),stats.rate_hz,stats.p99_us,stats.age_ms),("DualShock 4",250.0,4000,2));
        assert_eq!(input.stats(4_000_000_000).unwrap().age_ms,2988);
        input.sample(true,false);input.raw(0,&report(4004));assert!(input.stats(4_004_000_000).is_none());
        input.sample(false,true);input.raw(0,&report(4008));assert!(input.stats(4_008_000_000).is_none());
        input.sample(true,true);input.raw(0,&report(4012));assert!(input.stats(4_012_000_000).is_some());
        let y=PadState {buttons:lumatv_gfn_pad::button::Y,..Default::default()};
        let mut keys=Vec::new();
        input.menus[0].feed(&y,|key|keys.push(menu_key(key)));
        input.menus[0].feed(&y,|key|keys.push(menu_key(key)));
        assert_eq!(keys,[PadKey::Menu]);
    }
    #[test]
    fn feeder_startup_errors_are_reported_without_starting_a_network_stream() {
        use std::{io::Write, os::unix::net::UnixListener, sync::mpsc, thread};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("feeder.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let text = b"Another engine owns the player";
            let mut packet = vec![21, 0, 0, 0];
            packet.extend_from_slice(&(4u32 + text.len() as u32).to_le_bytes());
            packet.extend_from_slice(&3i32.to_le_bytes());
            packet.extend_from_slice(text);
            connection.write_all(&packet).unwrap();
        });
        let path = CString::new(socket.as_os_str().as_encoded_bytes()).unwrap();
        let config = Config { address: c"127.0.0.1".as_ptr(), version: c"7.1.431.-1".as_ptr(), gfe_version: c"3.23.0.74".as_ptr(), rtsp_url: c"rtsp://127.0.0.1:48010".as_ptr(), feeder: path.as_ptr(), width: 1920, height: 1080, fps: 60, bitrate: 20000, format: 1, server_codecs: 1, key: [0;16], iv: [0;16] };
        let (sender, receiver) = mpsc::channel();
        let mut context = Box::new(Context { session: SessionId(1), sender, cancel: Arc::new(Cancel::default()), failure: Mutex::new(None) });
        assert_ne!(unsafe { gs_start(&config, callback, (&mut *context as *mut Context).cast()) }, 0);
        let failure = context.failure.lock().unwrap();
        let failure = failure.as_ref().unwrap();
        assert_eq!(failure.kind, Kind::Video);
        assert!(failure.message.contains("Another engine owns the player"));
        assert!(matches!(receiver.try_recv(), Ok(crate::Message::Feedback(Feedback { kind: 4, .. }))));
        server.join().unwrap();
    }
}
