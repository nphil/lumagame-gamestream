use crate::{host::{TIMEOUT, document, text}, http::Client, state::{Cancel, Error, Host, Identity, Kind, Result, hex, random, unhex}};
use openssl::{hash::MessageDigest, sign::{Signer, Verifier}, symm::{Cipher, Crypter, Mode}, x509::X509};
use std::time::Duration;

fn aes(bytes: &[u8], key: &[u8; 16], mode: Mode) -> Result<Vec<u8>> {
    if bytes.is_empty() || bytes.len() % 16 != 0 { return Err(Error::new(Kind::Invalid, "Invalid pairing challenge length")); }
    let mut cipher = Crypter::new(Cipher::aes_128_ecb(), mode, key, None)?;
    cipher.pad(false);
    let mut out = vec![0; bytes.len() + 16];
    let used = cipher.update(bytes, &mut out)?;
    let used = used + cipher.finalize(&mut out[used..])?;
    out.truncate(used); Ok(out)
}
fn digest(parts: &[&[u8]]) -> Result<Vec<u8>> {
    let mut hasher = openssl::hash::Hasher::new(MessageDigest::sha256())?;
    for part in parts { hasher.update(part)?; }
    Ok(hasher.finish()?.to_vec())
}
fn key(salt: &[u8; 16], pin: &str) -> Result<[u8; 16]> { Ok(digest(&[salt, pin.as_bytes()])?[..16].try_into().unwrap()) }
fn paired(xml: &str) -> Result<DocumentValue<'_>> {
    let doc = document(xml)?;
    if text(doc.root_element(), "paired") != "1" { return Err(Error::new(Kind::Denied, "The PC refused pairing. Enter the TV's PIN in Sunshine or Apollo on the PC.")); }
    Ok(DocumentValue(doc))
}
struct DocumentValue<'a>(roxmltree::Document<'a>);
impl DocumentValue<'_> { fn bytes(&self, tag: &str) -> Result<Vec<u8>> { unhex(text(self.0.root_element(), tag)) } }

pub struct Pairing {
    salt: [u8; 16], challenge: [u8; 16], secret: [u8; 16], key: [u8; 16],
    server: Option<X509>, server_hash: Vec<u8>,
}
impl Pairing {
    pub fn new(pin: &str) -> Result<Self> {
        if pin.len() != 4 || !pin.bytes().all(|b| b.is_ascii_digit()) { return Err(Error::new(Kind::Invalid, "A pairing PIN has four digits")); }
        let salt = random()?;
        Ok(Self { salt, challenge: random()?, secret: random()?, key: key(&salt, pin)?, server: None, server_hash: Vec::new() })
    }
    pub fn certificate_request(&self, identity: &Identity) -> String { format!("devicename=LumaGame&updateState=1&phrase=getservercert&salt={}&clientcert={}", hex(&self.salt), hex(identity.cert.as_bytes())) }
    pub fn accept_certificate(&mut self, xml: &str) -> Result<String> {
        let cert = paired(xml)?.bytes("plaincert")?;
        if cert.is_empty() { return Err(Error::new(Kind::Busy, "The PC already has a pairing request open. Cancel that request before linking this TV.")); }
        self.server = Some(X509::from_pem(&cert)?);
        Ok(format!("devicename=LumaGame&updateState=1&clientchallenge={}", hex(&aes(&self.challenge, &self.key, Mode::Encrypt)?)))
    }
    pub fn answer_challenge(&mut self, identity: &Identity, xml: &str) -> Result<String> {
        let challenge = aes(&paired(xml)?.bytes("challengeresponse")?, &self.key, Mode::Decrypt)?;
        if challenge.len() != 48 { return Err(Error::new(Kind::Invalid, "Invalid server pairing challenge")); }
        self.server_hash = challenge[..32].to_vec();
        let cert = identity.certificate()?;
        let response = digest(&[&challenge[32..48], cert.signature().as_slice(), &self.secret])?;
        Ok(format!("devicename=LumaGame&updateState=1&serverchallengeresp={}", hex(&aes(&response, &self.key, Mode::Encrypt)?)))
    }
    pub fn verify_server(&self, identity: &Identity, xml: &str) -> Result<String> {
        let secret = paired(xml)?.bytes("pairingsecret")?;
        if secret.len() <= 16 { return Err(Error::new(Kind::Invalid, "Invalid server pairing secret")); }
        let cert = self.server.as_ref().ok_or_else(|| Error::new(Kind::Invalid, "Pairing response out of order"))?;
        let public_key = cert.public_key()?;
        let mut verifier = Verifier::new(MessageDigest::sha256(), &public_key)?;
        verifier.update(&secret[..16])?;
        if !verifier.verify(&secret[16..])? { return Err(Error::new(Kind::Auth, "The PC's pairing signature is invalid. Its identity could not be verified.")); }
        let expected = digest(&[&self.challenge, cert.signature().as_slice(), &secret[..16]])?;
        if self.server_hash.len() != expected.len() || !openssl::memcmp::eq(&self.server_hash, &expected) { return Err(Error::new(Kind::Auth, "The PIN did not match")); }
        let key = identity.private_key()?;
        let mut signer = Signer::new(MessageDigest::sha256(), &key)?;
        signer.update(&self.secret)?;
        let mut out = self.secret.to_vec(); out.extend_from_slice(&signer.sign_to_vec()?);
        Ok(format!("devicename=LumaGame&updateState=1&clientpairingsecret={}", hex(&out)))
    }
}

pub fn link(identity: &Identity, host: &mut Host, pin: &str, cancel: &Cancel) -> Result<()> {
    let mut state = Pairing::new(pin)?;
    let result = (|| -> Result<String> {
        let client = Client { identity, host, cancel };
        // The first request remains open until the PIN is entered on the PC; cancellation shuts down its socket.
        let cert = client.xml(false, "pair", &state.certificate_request(identity), Duration::from_secs(180))?;
        let challenge = client.xml(false, "pair", &state.accept_certificate(&cert)?, TIMEOUT)?;
        let secret = client.xml(false, "pair", &state.answer_challenge(identity, &challenge)?, TIMEOUT)?;
        let reply = client.xml(false, "pair", &state.verify_server(identity, &secret)?, TIMEOUT)?;
        paired(&reply)?;
        let certificate = String::from_utf8(state.server.as_ref().unwrap().to_pem()?).unwrap();
        let mut verified_host = host.clone(); verified_host.certificate = Some(certificate.clone());
        let tls = Client { identity, host: &verified_host, cancel }.xml(true, "pair", "devicename=LumaGame&updateState=1&phrase=pairchallenge", TIMEOUT)?;
        paired(&tls)?;
        cancel.check()?;
        Ok(certificate)
    })();
    match result {
        Ok(certificate) => { host.certificate = Some(certificate); Ok(()) }
        Err(error) => {
            // Only this client's random uniqueid is unpaired; an abandoned first step must not block the next attempt.
            let cleanup = Cancel::default();
            let _ = Client { identity, host, cancel: &cleanup }.get(false, "unpair", "", Duration::from_secs(2));
            cancel.check()?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_recorded_pairing_vectors_match_aes_and_certificate_signatures() {
        let vector: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/pairing-vector.json")).unwrap();
        let salt: [u8;16] = unhex(vector["salt"].as_str().unwrap()).unwrap().try_into().unwrap();
        let key = key(&salt, vector["pin"].as_str().unwrap()).unwrap();
        let encrypted = unhex(vector["client_challenge"].as_str().unwrap()).unwrap();
        let plain = aes(&encrypted, &key, Mode::Decrypt).unwrap();
        assert_eq!(aes(&plain, &key, Mode::Encrypt).unwrap(), encrypted);
        assert_eq!(plain, b"CLIENT CHALLENGE");
        let cert = X509::from_pem(include_bytes!("../tests/fixtures/pairing-cert.pem")).unwrap();
        let secret = unhex(vector["client_pairing_secret"].as_str().unwrap()).unwrap();
        let pubkey = cert.public_key().unwrap();
        let mut verify = Verifier::new(MessageDigest::sha256(), &pubkey).unwrap(); verify.update(&secret[..16]).unwrap();
        assert!(verify.verify(&secret[16..]).unwrap());
        let wrong = key_for_wrong_pin(&salt);
        assert_ne!(aes(&encrypted, &wrong, Mode::Decrypt).unwrap(), plain);
    }
    fn key_for_wrong_pin(salt: &[u8;16]) -> [u8;16] { key(salt, "0000").unwrap() }
    #[test]
    fn server_authentication_refuses_wrong_pin_tampering_and_truncated_messages() {
        let identity = Identity { id: "fixture".into(), cert: include_str!("../tests/fixtures/pairing-cert.pem").into(), key: include_str!("../tests/fixtures/pairing-key.pem").into() };
        let cert = identity.certificate().unwrap();
        let server_secret = [3u8;16];
        let mut pairing = Pairing::new("5338").unwrap();
        let hello = format!("<root status_code=\"200\"><paired>1</paired><plaincert>{}</plaincert></root>", hex(identity.cert.as_bytes()));
        let request = pairing.accept_certificate(&hello).unwrap();
        assert!(request.contains("clientchallenge="));
        let mut response = digest(&[&pairing.challenge, cert.signature().as_slice(), &server_secret]).unwrap();
        response.extend_from_slice(&[9; 16]);
        let challenge = format!("<root status_code=\"200\"><paired>1</paired><challengeresponse>{}</challengeresponse></root>", hex(&aes(&response, &pairing.key, Mode::Encrypt).unwrap()));
        let answer = pairing.answer_challenge(&identity, &challenge).unwrap();
        let encrypted = unhex(answer.split_once("serverchallengeresp=").unwrap().1).unwrap();
        assert_eq!(aes(&encrypted, &pairing.key, Mode::Decrypt).unwrap(), digest(&[&[9; 16], cert.signature().as_slice(), &pairing.secret]).unwrap());
        let key = identity.private_key().unwrap();
        let mut sign = Signer::new(MessageDigest::sha256(), &key).unwrap(); sign.update(&server_secret).unwrap();
        let mut secret = server_secret.to_vec(); secret.extend_from_slice(&sign.sign_to_vec().unwrap());
        let xml = format!("<root status_code=\"200\"><paired>1</paired><pairingsecret>{}</pairingsecret></root>", hex(&secret));
        assert!(pairing.verify_server(&identity, &xml).unwrap().contains("clientpairingsecret="));
        pairing.server_hash[0] ^= 1;
        assert_eq!(pairing.verify_server(&identity, &xml).unwrap_err().message, "The PIN did not match");
        secret[20] ^= 1;
        let tampered = format!("<root status_code=\"200\"><paired>1</paired><pairingsecret>{}</pairingsecret></root>", hex(&secret));
        assert!(pairing.verify_server(&identity, &tampered).unwrap_err().message.contains("signature"));
        assert!(pairing.answer_challenge(&identity, "<root status_code=\"200\"><paired>1</paired><challengeresponse>00</challengeresponse></root>").is_err());
    }
}
