use openssl::{asn1::Asn1Time, bn::BigNum, hash::MessageDigest, pkey::{PKey, Private}, rand::rand_bytes, rsa::Rsa, x509::{X509, X509NameBuilder}};
use serde::{Deserialize, Serialize};
use std::{fmt, fs::{self, OpenOptions}, io::Write, net::{Shutdown, TcpStream}, os::unix::fs::{OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}, sync::{Mutex, atomic::{AtomicBool, Ordering}}};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind { Network, Denied, Auth, Invalid, Busy, Video, Cancelled, Timeout, NotInstalled }
#[derive(Debug, Clone)]
pub struct Error { pub kind: Kind, pub message: String }
pub type Result<T, E = Error> = std::result::Result<T, E>;
impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self { Self { kind, message: message.into() } }
}
impl fmt::Display for Error { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.message) } }
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        let kind = if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) { Kind::Timeout } else { Kind::Network };
        Self::new(kind, e.to_string())
    }
}
impl From<openssl::error::ErrorStack> for Error { fn from(e: openssl::error::ErrorStack) -> Self { Self::new(Kind::Invalid, e.to_string()) } }
impl From<serde_json::Error> for Error { fn from(e: serde_json::Error) -> Self { Self::new(Kind::Invalid, e.to_string()) } }

#[derive(Default)]
pub struct Cancel { stopped: AtomicBool, socket: Mutex<Option<TcpStream>> }
impl Cancel {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(s) = self.socket.lock().unwrap().as_ref() { let _ = s.shutdown(Shutdown::Both); }
    }
    pub fn check(&self) -> Result<()> {
        if self.stopped.load(Ordering::Acquire) { Err(Error::new(Kind::Cancelled, "Cancelled")) } else { Ok(()) }
    }
    pub fn attach(&self, socket: &TcpStream) -> Result<()> {
        let mut s = self.socket.lock().unwrap();
        self.check()?;
        *s = Some(socket.try_clone()?);
        Ok(())
    }
}

pub fn random<const N: usize>() -> Result<[u8; N]> { let mut bytes = [0; N]; rand_bytes(&mut bytes)?; Ok(bytes) }
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes { out.push(DIGITS[(b >> 4) as usize] as char); out.push(DIGITS[(b & 15) as usize] as char); }
    out
}
pub fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 { return Err(Error::new(Kind::Invalid, "Invalid hexadecimal response")); }
    s.as_bytes().chunks_exact(2).map(|pair| {
        let a = (pair[0] as char).to_digit(16);
        let b = (pair[1] as char).to_digit(16);
        a.zip(b).map(|(a,b)| ((a << 4) | b) as u8).ok_or_else(|| Error::new(Kind::Invalid, "Invalid hexadecimal response"))
    }).collect()
}

pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("new-{}", hex(&random::<8>()?)));
    let mut out = OpenOptions::new().create_new(true).write(true).mode(0o600).open(&temp)?;
    let result = (|| -> Result<()> {
        out.write_all(bytes)?; out.sync_all()?;
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() { fs::File::open(parent)?.sync_all()?; }
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Identity { pub id: String, pub cert: String, pub key: String }
impl Identity {
    pub fn load(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let path = dir.join("identity.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let identity: Self = serde_json::from_slice(&bytes)?;
                if identity.id.len() != 16 || !identity.id.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(Error::new(Kind::Invalid, "The saved pairing identity has an invalid client ID")); }
                let key = identity.private_key()?;
                if !identity.certificate()?.public_key()?.public_eq(&key) { return Err(Error::new(Kind::Invalid, "The saved pairing certificate and key do not match")); }
                Ok(identity)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                private_write(&path, &serde_json::to_vec(&identity)?)?;
                Ok(identity)
            }
            Err(e) => Err(e.into()),
        }
    }
    fn generate() -> Result<Self> {
        let key = PKey::from_rsa(Rsa::generate(2048)?)?;
        let mut name = X509NameBuilder::new()?; name.append_entry_by_text("CN", "LumaGame")?;
        let name = name.build();
        let mut cert = X509::builder()?;
        cert.set_version(2)?;
        let serial = BigNum::from_slice(&random::<16>()?)?.to_asn1_integer()?;
        cert.set_serial_number(&serial)?;
        cert.set_subject_name(&name)?; cert.set_issuer_name(&name)?; cert.set_pubkey(&key)?;
        let not_before = Asn1Time::days_from_now(0)?;
        let not_after = Asn1Time::days_from_now(3650)?;
        cert.set_not_before(&not_before)?;
        cert.set_not_after(&not_after)?;
        cert.sign(&key, MessageDigest::sha256())?;
        Ok(Self { id: hex(&random::<8>()?), cert: String::from_utf8(cert.build().to_pem()?).unwrap(), key: String::from_utf8(key.private_key_to_pem_pkcs8()?).unwrap() })
    }
    pub fn certificate(&self) -> Result<X509> { Ok(X509::from_pem(self.cert.as_bytes())?) }
    pub fn private_key(&self) -> Result<PKey<Private>> { Ok(PKey::private_key_from_pem(self.key.as_bytes())?) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub address: String,
    pub https_port: u16,
    #[serde(default)] pub certificate: Option<String>,
    #[serde(default)] pub mac: Option<String>,
    #[serde(default)] pub ever_linked: bool,
    #[serde(skip)] pub online: bool,
}
#[derive(Serialize, Deserialize, Default)]
pub struct Hosts { #[serde(default)] pub hosts: Vec<Host>, #[serde(skip)] path: PathBuf }
impl Hosts {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("hosts.json");
        let mut state: Self = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        for host in &state.hosts {
            lumagame_core::ids::TargetId::new(host.id.clone()).map_err(|_| Error::new(Kind::Invalid, "A saved PC has an invalid ID"))?;
            crate::http::Address::parse(&host.address)?;
            if host.https_port == 0 { return Err(Error::new(Kind::Invalid, "A saved PC has an invalid HTTPS port")); }
            if let Some(cert) = &host.certificate { X509::from_pem(cert.as_bytes())?; }
        }
        state.path = path;
        Ok(state)
    }
    pub fn save(&self) -> Result<()> { private_write(&self.path, &serde_json::to_vec(self)?) }
    pub fn get(&self, id: &str) -> Result<Host> {
        self.hosts.iter().find(|h| h.id == id).cloned().ok_or_else(|| Error::new(Kind::Invalid, "Unknown PC"))
    }
    pub fn put(&mut self, host: Host) -> Result<()> {
        if let Some(old) = self.hosts.iter_mut().find(|h| h.id == host.id) { *old = host; } else { self.hosts.push(host); }
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_state_replacement_is_atomic_and_corruption_is_not_erased() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        private_write(&path, b"first").unwrap();
        private_write(&path, b"broken").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(Hosts::load(dir.path()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
