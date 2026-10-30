use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use base64::engine::{general_purpose, GeneralPurpose};
use base64::Engine;
use ring::signature::{Ed25519KeyPair, KeyPair};
use rustls::pki_types::pem::{PemObject, SectionKind};

use crate::{create_temp, dir_of, io_at, Error};

/// A raw Ed25519 public key.
pub type PublicKey = [u8; 32];

/// PKCS#8 v1 for Ed25519 up to the seed: what Go's MarshalPKCS8PrivateKey
/// writes, so a key we create is byte-for-byte one Go could have.
const PKCS8_PREFIX: [u8; 16] =
    [0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];

/// This process's long-term key.
pub struct Identity {
    pkcs8: Vec<u8>,
    public: PublicKey,
}

impl Identity {
    /// Reads a PEM PKCS#8 Ed25519 key from path, creating one (file 0600,
    /// directory 0700) if the file doesn't exist.
    pub fn load_or_create(path: &Path) -> Result<Identity, Error> {
        let b = match fs::read(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return create(path),
            Err(e) => return Err(io_at("read", path, e)),
            Ok(b) => b,
        };
        // Like ssh with an unprotected private key: refuse rather than use a
        // key others may have read.
        if let Ok(md) = fs::metadata(path) {
            let perm = md.permissions().mode() & 0o777;
            if perm & 0o077 != 0 {
                return Err(Error::Msg(format!(
                    "auth: {} is accessible by others (mode {perm:04o}); chmod 600 it",
                    path.display()
                )));
            }
        }
        // pem.Decode looks at the first block only.
        let der = match <(SectionKind, Vec<u8>)>::from_pem_slice(&b) {
            Ok((SectionKind::PrivateKey, der)) => der,
            _ => return Err(Error::Msg(format!("auth: {}: not a PEM private key", path.display()))),
        };
        Identity::from_pkcs8(der).map_err(|e| Error::Msg(format!("auth: {}: {e}", path.display())))
    }

    /// An identity from a PKCS#8 (v1 or v2) Ed25519 private key.
    pub fn from_pkcs8(der: Vec<u8>) -> Result<Identity, Error> {
        let kp = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
            .map_err(|e| Error::Msg(format!("not an Ed25519 PKCS#8 key ({e})")))?;
        let public = kp.public_key().as_ref().try_into().expect("Ed25519 public key is 32 bytes");
        Ok(Identity { pkcs8: der, public })
    }

    /// A fresh random identity, not saved anywhere.
    pub fn generate() -> Identity {
        let mut seed = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed).expect("system random");
        let mut der = PKCS8_PREFIX.to_vec();
        der.extend_from_slice(&seed);
        Identity::from_pkcs8(der).expect("generated key parses")
    }

    /// The identity's public key.
    pub fn public(&self) -> PublicKey {
        self.public
    }

    /// The private key as PKCS#8 DER.
    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }
}

fn create(path: &Path) -> Result<Identity, Error> {
    let id = Identity::generate();
    let dir = dir_of(path);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| io_at("mkdir", dir, e))?;
    // Write the whole key under a temporary name, then link it into place:
    // the link fails if another process got there first, and nobody ever
    // sees a half-written key. The loser reads the winner's.
    let (mut f, tmp) = create_temp(dir, ".key-")?;
    f.write_all(pem_encode("PRIVATE KEY", &id.pkcs8).as_bytes()).map_err(|e| io_at("write", &tmp.0, e))?;
    drop(f);
    match fs::hard_link(&tmp.0, path) {
        Ok(()) => Ok(id),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Identity::load_or_create(path),
        Err(e) => Err(io_at("link", path, e)),
    }
}

/// pem.Encode's output: 64-column base64 between the armor lines.
fn pem_encode(kind: &str, der: &[u8]) -> String {
    let b = general_purpose::STANDARD.encode(der);
    let mut s = format!("-----BEGIN {kind}-----\n");
    for line in b.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        s.push('\n');
    }
    s.push_str(&format!("-----END {kind}-----\n"));
    s
}

/// Go's base64.StdEncoding: padded, but lenient about trailing bits and
/// ignoring line breaks.
const STD: GeneralPurpose =
    GeneralPurpose::new(&base64::alphabet::STANDARD, general_purpose::PAD.with_decode_allow_trailing_bits(true));

/// Encodes a public key as base64 of its raw 32 bytes.
pub fn encode_key(k: &PublicKey) -> String {
    STD.encode(k)
}

/// Parses encode_key's output.
pub fn decode_key(s: &str) -> Result<PublicKey, Error> {
    let s: String = s.chars().filter(|&c| c != '\r' && c != '\n').collect();
    let b = STD.decode(s).map_err(|e| Error::Msg(format!("auth: bad key: {e}")))?;
    b.as_slice().try_into().map_err(|_| Error::Msg(format!("auth: bad key: {} bytes", b.len())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn identity_persists() {
        let t = TempDir::new();
        let dir = t.path().join("cfg");
        let path = dir.join("server.key");
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public(), b.public(), "identity changed on reload");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // The file is exactly what Go's pem.Encode of MarshalPKCS8PrivateKey
        // would write.
        assert_eq!(fs::read_to_string(&path).unwrap(), pem_encode("PRIVATE KEY", a.pkcs8()));
        assert_eq!(a.pkcs8().len(), 48);
    }

    #[test]
    fn identity_bad_file() {
        let t = TempDir::new();
        let path = t.path().join("bad.key");
        write_mode(&path, b"nope", 0o600);
        assert!(Identity::load_or_create(&path).is_err(), "garbage key loaded");
        write_mode(&path, b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", 0o600);
        assert!(Identity::load_or_create(&path).is_err(), "bad PKCS#8 loaded");
        assert!(Identity::load_or_create(t.path()).is_err(), "directory loaded as key");
    }

    #[test]
    fn key_round_trip() {
        let id = Identity::generate();
        let s = encode_key(&id.public());
        assert_eq!(decode_key(&s).unwrap(), id.public());
        assert!(decode_key("!!").is_err(), "bad base64 accepted");
        assert!(decode_key("AAAA").is_err(), "short key accepted");
    }

    #[test]
    fn identity_refuses_unprotected_key() {
        let t = TempDir::new();
        let path = t.path().join("id.key");
        Identity::load_or_create(&path).unwrap();
        chmod(&path, 0o644);
        let err = Identity::load_or_create(&path).err().expect("0644 key loaded");
        assert!(err.to_string().contains("chmod 600"), "{err}");
        chmod(&path, 0o600);
        Identity::load_or_create(&path).unwrap();
    }
}
