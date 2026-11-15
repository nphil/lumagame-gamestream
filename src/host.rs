use crate::{http::{Address, Client}, state::{Cancel, Error, Host, Identity, Kind, Result}};
use lumagame_core::{engine::Codec, ids::{ExternalId, ProviderGameId, TargetId}, ipc::{Art, Display, LibraryEntry, Readiness, Role}, settings::{SettingsSchema, Value, Values}};
use roxmltree::{Document, Node};
use std::{net::UdpSocket, time::Duration};

pub const TIMEOUT: Duration = Duration::from_secs(5);
pub fn document(text: &str) -> Result<Document<'_>> {
    let doc = Document::parse(text).map_err(|e| Error::new(Kind::Invalid, format!("Invalid PC XML: {e}")))?;
    let root = doc.root_element();
    if !root.has_tag_name("root") { return Err(Error::new(Kind::Invalid, "PC XML has no root element")); }
    let status = root.attribute("status_code").and_then(|s| s.parse::<u32>().ok()).ok_or_else(|| Error::new(Kind::Invalid, "PC XML has no status"))?;
    if status != 200 {
        let message = root.attribute("status_message").unwrap_or("Request rejected");
        let kind = match status { 401 => Kind::Auth, 403 => Kind::Denied, 409 => Kind::Busy, _ => Kind::Network };
        return Err(Error::new(kind, format!("PC error {status}: {message}{}", if status == 403 { ". Allow this client in Apollo's device permissions on the PC." } else { "" })));
    }
    Ok(doc)
}
pub fn text<'a, 'input>(node: Node<'a, 'input>, tag: &str) -> &'a str { node.children().find(|n| n.has_tag_name(tag)).and_then(|n| n.text()).unwrap_or("") }
fn number(node: Node<'_, '_>, tag: &str) -> Option<u32> { text(node, tag).parse().ok() }
fn flag(node: Node<'_, '_>, tag: &str) -> bool { matches!(text(node, tag), "1" | "true") }
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub id: String, pub name: String, pub version: String, pub gfe_version: String,
    pub https_port: u16, pub paired: bool, pub current_game: u32, pub codecs: u32,
    pub mac: Option<String>, pub permission: Option<u32>, pub virtual_display: bool, pub virtual_ready: bool,
}
impl ServerInfo {
    pub fn parse(xml: &str) -> Result<Self> {
        let doc = document(xml)?; let root = doc.root_element();
        let id = text(root, "uniqueid");
        TargetId::new(id).map_err(|e| Error::new(Kind::Invalid, e.to_string()))?;
        let version = text(root, "appversion");
        let parts: Vec<_> = version.split('.').collect();
        if parts.len() != 4 || parts.iter().any(|part| part.parse::<i32>().is_err()) || parts[0].parse::<u32>().unwrap_or(0) < 7 {
            return Err(Error::new(Kind::Invalid, "This engine requires a Sunshine or Apollo GameStream v7 server"));
        }
        let https_port = number(root, "HttpsPort").unwrap_or(47984);
        if https_port == 0 || https_port > 65535 { return Err(Error::new(Kind::Invalid, "The PC advertised an invalid HTTPS port")); }
        let mac = text(root, "mac");
        let name = text(root, "hostname");
        if name.is_empty() || name.len() > 128 { return Err(Error::new(Kind::Invalid, "The PC did not supply a valid name (1–128 bytes)")); }
        Ok(Self {
            id: id.into(), name: name.into(), version: version.into(), gfe_version: text(root, "GfeVersion").into(),
            https_port: https_port as u16, paired: flag(root, "PairStatus"),
            current_game: if text(root, "state").ends_with("_SERVER_BUSY") { number(root, "currentgame").unwrap_or(0) } else { 0 },
            codecs: number(root, "ServerCodecModeSupport").unwrap_or(1),
            mac: parse_mac(mac).ok().map(|_| mac.to_owned()), permission: number(root, "Permission"),
            virtual_display: flag(root, "VirtualDisplayCapable"), virtual_ready: flag(root, "VirtualDisplayDriverReady"),
        })
    }
    pub fn apply(&self, host: &mut Host) {
        host.id.clone_from(&self.id); host.name.clone_from(&self.name); host.https_port = self.https_port; host.online = true;
        if self.mac.is_some() { host.mac.clone_from(&self.mac); }
    }
}
pub fn server_info(identity: &Identity, host: &mut Host, cancel: &Cancel) -> Result<ServerInfo> {
    let secure = host.certificate.is_some();
    let result = Client { identity, host, cancel }.xml(secure, "serverinfo", "", TIMEOUT);
    let (xml, authenticated) = match result {
        Err(e) if secure && e.kind == Kind::Auth => {
            // HTTP is used only to rediscover an expired pairing; it never authorizes a library or launch.
            let xml = Client { identity, host, cancel }.xml(false, "serverinfo", "", TIMEOUT)?;
            (xml, false)
        }
        other => (other?, secure),
    };
    let info = ServerInfo::parse(&xml)?;
    if !host.id.is_empty() && host.id != info.id { return Err(Error::new(Kind::Auth, "A different PC now answers at this address. Add it as a new PC.")); }
    if !authenticated || !info.paired { host.certificate = None; }
    info.apply(host);
    Ok(info)
}
pub fn apps(xml: &str, info: &ServerInfo) -> Result<Vec<LibraryEntry>> {
    if info.permission == Some(0) { return Err(Error::new(Kind::Denied, "Apollo has not granted this client permission to list games. Change its device permissions on the PC.")); }
    let doc = document(xml)?;
    let mut entries = Vec::new();
    for app in doc.root_element().children().filter(|n| n.has_tag_name("App")) {
        let id = number(app, "ID").filter(|id| *id != 0).ok_or_else(|| Error::new(Kind::Invalid, "An app has no valid GameStream ID"))?;
        let title = text(app, "AppTitle");
        if id == 114514 && title == "Permission Denied" && text(app, "UUID").is_empty() { return Err(Error::new(Kind::Denied, "Apollo refused the game list. Allow this client in the PC's device permissions.")); }
        let steam = ["SteamAppId", "SteamAppID", "steam_app_id"].into_iter().find_map(|tag| number(app, tag).filter(|id| *id != 0));
        if title.len() > 512 { return Err(Error::new(Kind::Invalid, "An app title exceeds the engine's 512-byte limit")); }
        let role = if matches!(title.to_ascii_lowercase().as_str(), "desktop" | "steam" | "steam big picture") { Role::Launcher } else { Role::Game };
        entries.push(LibraryEntry {
            id: ProviderGameId::new(id.to_string()).unwrap(), title: if title.trim().is_empty() { format!("App {id}") } else { title.to_owned() }, role,
            external: steam.map(|id| ExternalId::new("steam", id.to_string()).unwrap()).into_iter().collect(),
            store: steam.map(|_| "steam".into()), store_account: None, art: Art::default(), readiness: Readiness::Ready,
            resumable: info.current_game == id,
        });
    }
    Ok(entries)
}

pub fn schema() -> SettingsSchema {
    serde_json::from_str(include_str!("../settings.json")).expect("the shipped settings schema")
}
fn choice<'a>(values: &'a Values, key: &str, default: &'a str) -> &'a str { if let Some(Value::Text(v)) = values.get(key) { v } else { default } }
fn toggle(values: &Values, key: &str, default: bool) -> bool { if let Some(Value::Bool(v)) = values.get(key) { *v } else { default } }
fn integer(values: &Values, key: &str, default: u32) -> u32 { if let Some(Value::Int(v)) = values.get(key) { u32::try_from(*v).unwrap_or(default) } else { default } }
#[derive(Debug, Clone)]
pub struct Options {
    pub width: u32, pub height: u32, pub fps: u32, pub bitrate: u32, pub format: i32, pub codec: Codec, pub hdr: bool, pub virtual_display: bool,
}
impl Options {
    pub fn select(values: &Values, display: &Display, host: &ServerInfo) -> Result<Self> {
        // The keys and ranges are LumaGame's Streaming settings, so the app's common values apply; a mode above the TV's is lowered to it.
        let width = integer(values, "width", 3840).min(display.width);
        let height = integer(values, "height", 2160).min(display.height);
        if width == 0 || height == 0 { return Err(Error::new(Kind::Invalid, "The display has no usable stream mode")); }
        let fps = integer(values, "fps", 120).min(display.refresh_hz).min(120);
        if fps == 0 { return Err(Error::new(Kind::Invalid, "The display has no usable frame rate")); }
        let hdr = toggle(values, "hdr", false);
        let codec = match choice(values, "codec", "auto") {
            "h264" => Codec::H264, "h265" => Codec::H265, "av1" => Codec::Av1,
            _ if hdr => Codec::H265,
            _ if fps <= 60 && host.codecs & 0x10000 != 0 && display.codecs.contains(&Codec::Av1) => Codec::Av1,
            _ if host.codecs & 0x100 != 0 && display.codecs.contains(&Codec::H265) => Codec::H265,
            _ => Codec::H264,
        };
        if !display.codecs.contains(&codec) { return Err(Error::new(Kind::Invalid, "This display cannot decode the selected codec")); }
        if codec == Codec::Av1 && fps > 60 { return Err(Error::new(Kind::Invalid, "AV1 is limited to 60 fps on this TV; choose HEVC for 120 fps")); }
        if hdr && (!display.hdr10 || codec != Codec::H265) { return Err(Error::new(Kind::Invalid, "HDR requires an HDR10 display and HEVC Main10")); }
        let (mask, format) = match codec { Codec::H264 => (1, 1), Codec::H265 if hdr => (0x200, 0x200), Codec::H265 => (0x100, 0x100), Codec::Av1 => (0x10000, 0x1000), _ => unreachable!() };
        if host.codecs & mask == 0 { return Err(Error::new(Kind::Invalid, "The PC does not advertise the selected codec/profile. Check its encoder settings.")); }
        let virtual_display = toggle(values, "virtual-display", true) && host.virtual_display;
        if virtual_display && !host.virtual_ready { return Err(Error::new(Kind::Invalid, "Apollo's virtual-display driver is not ready on the PC")); }
        let bitrate = if let Some(Value::Int(v)) = values.get("bitrate-mbps") { *v as u32 * 1000 } else { 50000 };
        Ok(Self { width, height, fps, bitrate, format, codec, hdr, virtual_display })
    }
    pub fn launch_arguments(&self, app: u32, key: &[u8; 16], iv: &[u8; 16]) -> String {
        let hdr = if self.hdr { "&hdrMode=1&clientHdrCapVersion=0&clientHdrCapSupportedFlagsInUint32=0&clientHdrCapMetaDataId=NV_STATIC_METADATA_TYPE_1&clientHdrCapDisplayData=0x0x0x0x0x0x0x0x0x0x0" } else { "" };
        format!("appid={app}&mode={}x{}x{}&additionalStates=1&sops=1&rikey={}&rikeyid={}&localAudioPlayMode=0&surroundAudioInfo=196610&remoteControllersBitmap=1&gcmap=1&gcpersist=0&virtualDisplay={}&scaleFactor=100{hdr}{}", self.width, self.height, self.fps, crate::state::hex(key), u32::from_be_bytes(iv[..4].try_into().unwrap()), u8::from(self.virtual_display), crate::stream::launch_extensions())
    }
}

pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let parts: Vec<_> = text.split([':', '-']).collect();
    if parts.len() != 6 { return Err(Error::new(Kind::Invalid, "The PC has not supplied a usable Wake-on-LAN address")); }
    let mut mac = [0; 6];
    for (i, p) in parts.iter().enumerate() {
        if p.len() != 2 { return Err(Error::new(Kind::Invalid, "Invalid MAC address")); }
        mac[i] = u8::from_str_radix(p, 16).map_err(|_| Error::new(Kind::Invalid, "Invalid MAC address"))?;
    }
    if mac == [0; 6] || mac[0] & 1 != 0 { return Err(Error::new(Kind::Invalid, "No Wake-on-LAN address is known for this PC")); }
    Ok(mac)
}
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] { let mut bytes = [0xff; 102]; for dst in bytes[6..].chunks_exact_mut(6) { dst.copy_from_slice(&mac); } bytes }
pub fn wake(host: &Host) -> Result<()> {
    let packet = magic_packet(parse_mac(host.mac.as_deref().unwrap_or(""))?);
    let socket = UdpSocket::bind("0.0.0.0:0")?; socket.set_broadcast(true)?;
    socket.send_to(&packet, "255.255.255.255:9")?;
    let address = Address::parse(&host.address)?;
    // Unicast also reaches hosts through routers that keep their sleeping PC's ARP entry.
    socket.send_to(&packet, (address.host.as_str(), 9))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serverinfo_preserves_permissions_codecs_and_busy_semantics() {
        let mut info = ServerInfo::parse(include_str!("../tests/fixtures/serverinfo-apollo.xml")).unwrap();
        assert!(info.paired && info.virtual_ready && info.virtual_display);
        assert_eq!(info.current_game, 42);
        assert_eq!(info.codecs, 197377);
        let apps = apps(include_str!("../tests/fixtures/applist.xml"), &info).unwrap();
        assert_eq!(apps[1].external[0].to_string(), "steam:1245620");
        assert!(apps[1].resumable && apps[0].external.is_empty());
        assert_eq!(apps[0].role, Role::Launcher);
        info.permission = Some(0);
        assert_eq!(apps_result(&info).unwrap_err().kind, Kind::Denied);
        let idle = include_str!("../tests/fixtures/serverinfo-apollo.xml").replace("_SERVER_BUSY", "_SERVER_FREE");
        assert_eq!(ServerInfo::parse(&idle).unwrap().current_game, 0);
        assert_eq!(document(include_str!("../tests/fixtures/denied.xml")).unwrap_err().kind, Kind::Denied);
    }
    fn apps_result(info: &ServerInfo) -> Result<Vec<LibraryEntry>> { apps(include_str!("../tests/fixtures/applist.xml"), info) }
    #[test]
    fn upstream_xml_accepts_the_negative_build_version_used_by_sunshine() {
        for (xml, apollo) in [(include_str!("../tests/fixtures/upstream-apollo.xml"), true), (include_str!("../tests/fixtures/upstream-sunshine.xml"), false)] {
            let info = ServerInfo::parse(xml).unwrap();
            assert_eq!(info.version, "7.1.431.-1");
            assert!(info.paired);
            assert_eq!(info.virtual_display, apollo);
            assert_eq!(info.virtual_ready, apollo);
            assert_eq!(info.current_game, 0);
            assert_eq!(info.codecs, 197377);
        }
    }
    #[test]
    fn hdr_and_av1_follow_the_actual_host_and_display_limits() {
        let info = ServerInfo::parse(include_str!("../tests/fixtures/serverinfo-apollo.xml")).unwrap();
        let display = Display { width: 3840, height: 2160, refresh_hz: 120, hdr10: true, codecs: vec![Codec::H264, Codec::H265, Codec::Av1] };
        let schema = schema(); schema.validate().unwrap();
        let mut values = schema.defaults();
        let options = Options::select(&values, &display, &info).unwrap();
        assert_eq!(options.codec, Codec::H265);
        values.insert("codec".into(), Value::Text("av1".into()));
        assert!(Options::select(&values, &display, &info).is_err());
        values.insert("fps".into(), Value::Int(60));
        assert_eq!(Options::select(&values, &display, &info).unwrap().format, 0x1000);
        values.insert("hdr".into(), Value::Bool(true));
        assert!(Options::select(&values, &display, &info).is_err());
        values.insert("codec".into(), Value::Text("h265".into()));
        let options = Options::select(&values, &display, &info).unwrap();
        assert_eq!(options.format, 0x200);
        let args = options.launch_arguments(42, &[0; 16], &[1; 16]);
        assert!(args.contains("virtualDisplay=1") && args.contains("mode=3840x2160x60") && args.contains("hdrMode=1"));
    }
    #[test]
    fn the_apps_common_stream_values_apply_and_are_lowered_to_the_display() {
        let info = ServerInfo::parse(include_str!("../tests/fixtures/serverinfo-apollo.xml")).unwrap();
        let common = Values::from([
            ("width".into(), Value::Int(2560)), ("height".into(), Value::Int(1440)), ("fps".into(), Value::Int(120)),
            ("hdr".into(), Value::Bool(false)), ("bitrate-mbps".into(), Value::Int(150)), ("codec".into(), Value::Text("h264".into())),
        ]);
        for (key, value) in &common { assert_eq!(schema().check(key, value), Ok(()), "{key}"); }
        let values = schema().resolve(&common);
        let tv = Display { width: 1920, height: 1080, refresh_hz: 60, hdr10: false, codecs: vec![Codec::H264, Codec::H265] };
        let o = Options::select(&values, &tv, &info).unwrap();
        assert_eq!((o.width, o.height, o.fps, o.bitrate, o.codec), (1920, 1080, 60, 150_000, Codec::H264));
    }
    #[test]
    fn wake_packet_is_the_standard_six_prefix_and_sixteen_mac_copies() {
        let mac = parse_mac("02:11:22:33:44:55").unwrap(); let bytes = magic_packet(mac);
        assert_eq!(&bytes[..6], &[255;6]); assert!(bytes[6..].chunks_exact(6).all(|b| b == mac));
        assert!(parse_mac("00:00:00:00:00:00").is_err());
    }
}
