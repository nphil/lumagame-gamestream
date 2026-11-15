use crate::state::{Cancel, Error, Host, Identity, Kind, Result, hex, random};
use openssl::{ssl::{SslConnector, SslMethod, SslVerifyMode}, x509::X509};
use std::{io::{BufRead, BufReader, Read, Write}, net::{IpAddr, TcpStream, ToSocketAddrs}, time::{Duration, Instant}};

const MAX_BODY: usize = 8 << 20;
const MAX_HEADER: usize = 16 << 10;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address { pub host: String, pub port: u16 }
impl Address {
    pub fn parse(text: &str) -> Result<Self> {
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (host, tail) = rest.split_once(']').ok_or_else(|| Error::new(Kind::Invalid, "Unclosed IPv6 address"))?;
            if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) { return Err(Error::new(Kind::Invalid, "Invalid IPv6 address")); }
            (host, if tail.is_empty() { 47989 } else { parse_port(tail.strip_prefix(':').ok_or_else(|| Error::new(Kind::Invalid, "Invalid PC port"))?)? })
        } else if text.parse::<IpAddr>().is_ok() {
            (text, 47989)
        } else if let Some((host, port)) = text.rsplit_once(':') {
            (host, parse_port(port)?)
        } else { (text, 47989) };
        if host.is_empty() || host.len() > 253 || (host.parse::<IpAddr>().is_err() && !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')) {
            return Err(Error::new(Kind::Invalid, "Enter a PC name or IP address, optionally followed by its GameStream HTTP port"));
        }
        Ok(Self { host: host.to_owned(), port })
    }
    pub fn authority(&self, port: u16) -> String {
        if self.host.contains(':') { format!("[{}]:{port}", self.host) } else { format!("{}:{port}", self.host) }
    }
}
fn parse_port(text: &str) -> Result<u16> { text.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| Error::new(Kind::Invalid, "Invalid PC port")) }

pub struct Client<'a> { pub identity: &'a Identity, pub host: &'a Host, pub cancel: &'a Cancel }
impl Client<'_> {
    pub fn get(&self, secure: bool, command: &str, arguments: &str, timeout: Duration) -> Result<String> {
        self.cancel.check()?;
        let address = Address::parse(&self.host.address)?;
        let port = if secure { self.host.https_port } else { address.port };
        let mut last = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut connected = None;
        for endpoint in (address.host.as_str(), port).to_socket_addrs()? {
            self.cancel.check()?;
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else { break };
            match TcpStream::connect_timeout(&endpoint, remaining) {
                Ok(socket) => { connected = Some(socket); break; }
                Err(e) => last = Some(e),
            }
        }
        let mut socket = connected.ok_or_else(|| last.map(Error::from).unwrap_or_else(|| Error::new(Kind::Network, "The PC address did not resolve")))?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(Duration::from_secs(5)))?;
        self.cancel.attach(&socket)?;
        let args = if arguments.is_empty() { String::new() } else { format!("&{arguments}") };
        let request = format!("GET /{command}?uniqueid={}&uuid={}{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/xml\r\nUser-Agent: LumaGame/{}\r\n\r\n", self.identity.id, hex(&random::<16>()?), args, address.authority(port), env!("CARGO_PKG_VERSION"));
        let response = if secure {
            let pinned = self.host.certificate.as_ref().ok_or_else(|| Error::new(Kind::Auth, "The PC must be linked first"))?;
            let pinned = X509::from_pem(pinned.as_bytes())?.to_der()?;
            let mut connector = SslConnector::builder(SslMethod::tls_client())?;
            connector.set_certificate(self.identity.certificate()?.as_ref())?;
            connector.set_private_key(self.identity.private_key()?.as_ref())?;
            // GameStream certificates are self-signed. Only the exact certificate authenticated by the PIN exchange is trusted.
            let mismatch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let rejected = mismatch.clone();
            connector.set_verify_callback(SslVerifyMode::PEER, move |_, ctx| {
                let matches = ctx.error_depth() == 0 && ctx.current_cert().and_then(|cert| cert.to_der().ok()).is_some_and(|der| der.len() == pinned.len() && openssl::memcmp::eq(&der, &pinned));
                if !matches { rejected.store(true, std::sync::atomic::Ordering::Release); }
                matches
            });
            let mut config = connector.build().configure()?;
            config.set_verify_hostname(false);
            let mut stream = config.connect(&address.host, socket).map_err(|e| {
                if mismatch.load(std::sync::atomic::Ordering::Acquire) {
                    Error::new(Kind::Auth, "The PC's TLS certificate changed. Link the PC again to verify its identity.")
                } else {
                    Error::new(Kind::Network, format!("The PC's encrypted connection failed: {e}"))
                }
            })?;
            stream.write_all(request.as_bytes())?;
            read_response(stream)
        } else {
            socket.write_all(request.as_bytes())?;
            read_response(socket)
        };
        self.cancel.check()?;
        String::from_utf8(response?).map_err(|_| Error::new(Kind::Invalid, "The PC returned non-UTF-8 XML"))
    }
    pub fn xml(&self, secure: bool, command: &str, arguments: &str, timeout: Duration) -> Result<String> {
        let text = self.get(secure, command, arguments, timeout)?;
        crate::host::document(&text)?;
        Ok(text)
    }
}

fn bounded_line(reader: &mut impl BufRead, remaining: &mut usize) -> Result<String> {
    let mut bytes = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() { return Err(Error::new(Kind::Invalid, "The PC closed an incomplete HTTP response")); }
        let n = buffer.iter().position(|b| *b == b'\n').map_or(buffer.len(), |n| n + 1);
        if n > *remaining { return Err(Error::new(Kind::Invalid, "The PC's HTTP headers exceed 16 KiB")); }
        let end = buffer[n - 1] == b'\n';
        bytes.extend_from_slice(&buffer[..n]);
        *remaining -= n;
        reader.consume(n);
        if end { break; }
    }
    String::from_utf8(bytes).map_err(|_| Error::new(Kind::Invalid, "Invalid HTTP headers"))
}
fn read_response(stream: impl Read) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(stream);
    let mut remaining = MAX_HEADER;
    let line = bounded_line(&mut reader, &mut remaining)?;
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok()).ok_or_else(|| Error::new(Kind::Invalid, "Invalid HTTP status"))?;
    if status != 200 { return Err(Error::new(if status == 401 { Kind::Auth } else if status == 403 { Kind::Denied } else { Kind::Network }, format!("PC HTTP error {status}"))); }
    let mut length = None;
    let mut chunked = false;
    loop {
        let line = bounded_line(&mut reader, &mut remaining)?;
        if line.trim().is_empty() { break; }
        let (name, value) = line.split_once(':').ok_or_else(|| Error::new(Kind::Invalid, "Malformed HTTP header"))?;
        if name.eq_ignore_ascii_case("content-length") {
            let n = value.trim().parse::<usize>().map_err(|_| Error::new(Kind::Invalid, "Invalid HTTP length"))?;
            if n > MAX_BODY || length.is_some_and(|old| old != n) { return Err(Error::new(Kind::Invalid, "Invalid or oversized HTTP response")); }
            length = Some(n);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.trim().eq_ignore_ascii_case("chunked") { return Err(Error::new(Kind::Invalid, "Unsupported HTTP transfer encoding")); }
            chunked = true;
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let line = bounded_line(&mut reader, &mut remaining)?;
            let n = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16).map_err(|_| Error::new(Kind::Invalid, "Invalid HTTP chunk"))?;
            if n == 0 { break; }
            if n > MAX_BODY - body.len() { return Err(Error::new(Kind::Invalid, "HTTP response too large")); }
            let at = body.len(); body.resize(at + n, 0); reader.read_exact(&mut body[at..])?;
            let mut crlf = [0; 2]; reader.read_exact(&mut crlf)?;
            if crlf != *b"\r\n" { return Err(Error::new(Kind::Invalid, "Invalid HTTP chunk boundary")); }
        }
    } else if let Some(n) = length {
        body.resize(n, 0); reader.read_exact(&mut body)?;
    } else {
        reader.take((MAX_BODY + 1) as u64).read_to_end(&mut body)?;
        if body.len() > MAX_BODY { return Err(Error::new(Kind::Invalid, "HTTP response too large")); }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses_cannot_inject_requests() {
        assert_eq!(Address::parse("[::1]:49000").unwrap().authority(49001), "[::1]:49001");
        assert_eq!(Address::parse("gaming-pc.local").unwrap().port, 47989);
        for bad in ["", "pc:0", "pc/path", "pc\r\nX:1", "https://pc", "pc:65536"] { assert!(Address::parse(bad).is_err(), "{bad}"); }
    }
    #[test]
    fn response_framing_is_bounded_and_checks_truncation() {
        assert_eq!(read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nxml"[..]).unwrap(), b"xml");
        assert_eq!(read_response(&b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nxml\r\n0\r\n\r\n"[..]).unwrap(), b"xml");
        assert!(read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nx"[..]).is_err());
        assert!(read_response(&b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n"[..]).is_err());
    }
    #[test]
    fn paired_tls_trusts_only_the_exact_recorded_certificate() {
        use openssl::{pkey::PKey, ssl::SslAcceptor};
        use std::{net::TcpListener, thread};
        let cert = X509::from_pem(include_bytes!("../tests/fixtures/pairing-cert.pem")).unwrap();
        let key = PKey::private_key_from_pem(include_bytes!("../tests/fixtures/pairing-key.pem")).unwrap();
        for case in 0..3 {
            let matching = case == 0;
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
            acceptor.set_certificate(&cert).unwrap(); acceptor.set_private_key(&key).unwrap();
            let server = thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                if case == 2 { return; }
                if let Ok(mut stream) = acceptor.build().accept(socket) {
                    let mut input = [0;4096];
                    let n = stream.read(&mut input).unwrap();
                    assert!(std::str::from_utf8(&input[..n]).unwrap().starts_with("GET /serverinfo?uniqueid=fixture&uuid="));
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 25\r\n\r\n<root status_code=\"200\"/>").unwrap();
                }
            });
            let mut der = cert.to_der().unwrap();
            if !matching { *der.last_mut().unwrap() ^= 1; }
            let pinned = String::from_utf8(X509::from_der(&der).unwrap().to_pem().unwrap()).unwrap();
            let host = Host { id: "fixture".into(), name: "Test".into(), address: "127.0.0.1".into(), https_port: port, certificate: Some(pinned), mac: None, online: true, ever_linked: true };
            let identity = Identity { id: "fixture".into(), cert: include_str!("../tests/fixtures/pairing-cert.pem").into(), key: include_str!("../tests/fixtures/pairing-key.pem").into() };
            let result = Client { identity: &identity, host: &host, cancel: &Cancel::default() }.xml(true, "serverinfo", "", TIMEOUT_FOR_TEST);
            assert_eq!(result.is_ok(), matching, "{result:?}");
            if !matching { assert_eq!(result.unwrap_err().kind, if case == 1 { Kind::Auth } else { Kind::Network }); }
            server.join().unwrap();
        }
    }
    const TIMEOUT_FOR_TEST: Duration = Duration::from_secs(2);
}
