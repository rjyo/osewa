use std::sync::Arc;

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose, SerialNumber,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::Resumption;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::NoServerSessionStorage;
use rustls::{
    ClientConfig, CommonState, DigitallySignedStruct, DistinguishedName as TlsName, ServerConfig, SignatureScheme,
};

use crate::devices::Devices;
use crate::keys::{Identity, PublicKey};
use crate::Error;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Wraps id in a throwaway self-signed certificate. Peers pin the raw key,
/// never the certificate, so its fields only have to parse.
fn certificate(id: &Identity) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let key = PrivatePkcs8KeyDer::from(id.pkcs8().to_vec());
    let kp = KeyPair::try_from(&key).expect("auth: identity is a valid Ed25519 key");
    let mut params = CertificateParams::default();
    let mut serial = [0u8; 16];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut serial).expect("system random");
    serial[0] &= 0x7f; // 127 bits, as Go's: positive whatever the draw.
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, "osewa");
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(100 * 365);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
    let cert = params.self_signed(&kp).expect("auth: self-signed certificate");
    (vec![cert.der().clone()], PrivateKeyDer::Pkcs8(key))
}

/// A TLS 1.3 server config that accepts only clients whose key is in d,
/// checked on every handshake. The server never resumes sessions: a resumed
/// session skips the certificate check, which would let a revoked device
/// back in on a ticket from before the revocation (Go re-checks in
/// VerifyConnection instead; rustls has no hook that runs on resumption).
pub fn server_config(id: &Identity, d: &Devices) -> ServerConfig {
    let provider = provider();
    let (certs, key) = certificate(id);
    let verifier = Arc::new(DeviceVerifier { devices: d.clone(), provider: provider.clone() });
    let mut c = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .expect("auth: identity certificate");
    c.session_storage = Arc::new(NoServerSessionStorage {});
    c.send_tls13_tickets = 0;
    c
}

/// A TLS 1.3 client config that presents id and accepts only a server
/// holding server_key. Like Go's (no ClientSessionCache), it doesn't resume.
pub fn client_config(id: &Identity, server_key: PublicKey) -> ClientConfig {
    let provider = provider();
    let (certs, key) = certificate(id);
    // Chain verification is replaced by the pin.
    let mut c = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier { pin: server_key, provider }))
        .with_client_auth_cert(certs, key)
        .expect("auth: identity certificate");
    c.resumption = Resumption::disabled();
    c
}

/// The peer's key from a completed handshake, or None.
pub fn peer_key(conn: &CommonState) -> Option<PublicKey> {
    cert_key(conn.peer_certificates()?.first()?)
}

/// The Ed25519 key a certificate is over, or None if it isn't one.
pub fn cert_key(cert: &CertificateDer<'_>) -> Option<PublicKey> {
    // SubjectPublicKeyInfo for Ed25519 (RFC 8410) is fixed up to the key.
    const SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    let ee = webpki::EndEntityCert::try_from(cert).ok()?;
    let spki = ee.subject_public_key_info();
    spki.as_ref().strip_prefix(&SPKI_PREFIX[..])?.try_into().ok()
}

/// The auth Error behind a failed handshake, if auth is why it failed. It
/// takes the rustls::Error, or the io::Error a rustls stream wraps it in.
pub fn auth_error<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a Error> {
    let tls = match e.downcast_ref::<std::io::Error>() {
        Some(io) => io.get_ref()?.downcast_ref::<rustls::Error>()?,
        None => e.downcast_ref::<rustls::Error>()?,
    };
    match tls {
        rustls::Error::Other(o) => o.0.downcast_ref::<Error>(),
        _ => None,
    }
}

fn reject(e: Error) -> rustls::Error {
    rustls::Error::Other(rustls::OtherError(Arc::new(e)))
}

#[derive(Debug)]
struct DeviceVerifier {
    devices: Devices,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for DeviceVerifier {
    fn root_hint_subjects(&self) -> &[TlsName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let k = cert_key(end_entity).ok_or_else(|| reject(Error::NoPeerKey))?;
        self.devices.find(&k).map_err(reject)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinVerifier {
    pin: PublicKey,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match cert_key(end_entity) {
            None => Err(reject(Error::NoPeerKey)),
            Some(k) if k != self.pin => Err(reject(Error::ServerKey)),
            Some(_) => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use rustls::client::Resumption;
    use rustls::{ClientConnection, HandshakeKind, ServerConnection, StreamOwned};

    use super::*;
    use crate::devices::Device;
    use crate::keys::encode_key;
    use crate::testutil::*;

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let ln = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = TcpStream::connect(ln.local_addr().unwrap()).unwrap();
        let (a, _) = ln.accept().unwrap();
        for s in [&a, &b] {
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        }
        (a, b)
    }

    /// Runs the server side of a test handshake on sock: handshake, read a
    /// byte, close. Returns the error and the client's key.
    pub(crate) fn serve(srv: Arc<ServerConfig>, sock: TcpStream) -> (io::Result<()>, Option<PublicKey>) {
        let mut s = StreamOwned::new(ServerConnection::new(srv).unwrap(), sock);
        let mut res = s.conn.complete_io(&mut s.sock).map(|_| ());
        let peer = peer_key(&s.conn);
        if res.is_ok() {
            res = s.read_exact(&mut [0u8]);
        }
        s.conn.send_close_notify();
        let _ = s.conn.complete_io(&mut s.sock);
        (res, peer)
    }

    /// Runs the client side: handshake, write a byte, read until the server
    /// closes. A TLS 1.3 client finishes its handshake before the server
    /// checks the client cert; the read surfaces the server's alert.
    pub(crate) fn dial(cli: Arc<ClientConfig>, sock: TcpStream) -> io::Result<ClientConnection> {
        let mut c = StreamOwned::new(ClientConnection::new(cli, "osewa".try_into().unwrap()).unwrap(), sock);
        while c.conn.is_handshaking() {
            c.conn.complete_io(&mut c.sock)?;
        }
        c.write_all(&[1])?;
        c.flush()?;
        let mut b = Vec::new();
        c.read_to_end(&mut b)?;
        Ok(c.conn)
    }

    /// One handshake over loopback TCP; both errors and the key the server
    /// saw.
    fn handshake(srv: &ServerConfig, cli: &ClientConfig) -> (io::Result<()>, io::Result<()>, Option<PublicKey>) {
        let (a, b) = tcp_pair();
        let srv = Arc::new(srv.clone());
        let t = std::thread::spawn(move || serve(srv, a));
        let cerr = dial(Arc::new(cli.clone()), b).map(|_| ());
        let (serr, peer) = t.join().unwrap();
        (serr, cerr, peer)
    }

    fn is(r: &io::Result<()>, want: fn(&Error) -> bool) -> bool {
        match r {
            Err(e) => auth_error(e).is_some_and(want),
            Ok(()) => false,
        }
    }

    #[test]
    fn tls() {
        let (server, device, stranger) = (Identity::generate(), Identity::generate(), Identity::generate());
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        d.add(Device { name: "phone".into(), key: encode_key(&device.public()) }).unwrap();
        let srv = server_config(&server, &d);

        let (serr, cerr, peer) = handshake(&srv, &client_config(&device, server.public()));
        assert!(serr.is_ok() && cerr.is_ok(), "enrolled: server {serr:?} client {cerr:?}");
        assert_eq!(peer, Some(device.public()), "peer_key mismatch");

        let (serr, _, _) = handshake(&srv, &client_config(&stranger, server.public()));
        assert!(is(&serr, |e| matches!(e, Error::UnknownDevice)), "unknown client: {serr:?}");

        let (_, cerr, _) = handshake(&srv, &client_config(&device, stranger.public()));
        assert!(is(&cerr, |e| matches!(e, Error::ServerKey)), "wrong pin: {cerr:?}");

        d.remove("phone").unwrap();
        let (serr, _, _) = handshake(&srv, &client_config(&device, server.public()));
        assert!(is(&serr, |e| matches!(e, Error::UnknownDevice)), "revoked: {serr:?}");

        // No client certificate at all.
        let no_cert = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinVerifier { pin: server.public(), provider: provider() }))
            .with_no_client_auth();
        let (serr, _, _) = handshake(&srv, &no_cert);
        assert!(serr.is_err(), "client without certificate accepted");
        // TLS 1.2 isn't compiled in; the Go interop test checks a 1.2 client
        // is refused.
    }

    // A server whose certificate isn't over an Ed25519 key can't match a pin.
    #[test]
    fn peer_key_needs_ed25519() {
        let kp = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = CertificateParams::default().self_signed(&kp).unwrap();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(kp.serialize_der()));
        let srv = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .unwrap();
        assert_eq!(cert_key(cert.der()), None);
        let (_, cerr, _) = handshake(&srv, &client_config(&Identity::generate(), Identity::generate().public()));
        assert!(is(&cerr, |e| matches!(e, Error::NoPeerKey)), "client: {cerr:?}");
    }

    // The handshake reports why the devices file isn't trusted, not just
    // "not enrolled", so the server log points at the fix.
    #[test]
    fn handshake_reports_untrusted_devices() {
        let (srv, dev) = (Identity::generate(), Identity::generate());
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        d.add(Device { name: "phone".into(), key: encode_key(&dev.public()) }).unwrap();
        chmod(&d.path, 0o666);
        let (serr, cerr, _) = handshake(&server_config(&srv, &d), &client_config(&dev, srv.public()));
        assert!(
            is(&serr, |e| e.to_string().contains("writable by others")) && cerr.is_err(),
            "server {serr:?}, client {cerr:?}"
        );
    }

    // A device revoked between connections must not get back in by resuming
    // its TLS session. The server never resumes, so a client that would
    // resume gets a full handshake and its key checked again.
    #[test]
    fn revoked_device_cannot_resume() {
        let (server, device) = (Identity::generate(), Identity::generate());
        let t = TempDir::new();
        let d = Devices::new(t.path().join("devices"));
        d.add(Device { name: "phone".into(), key: encode_key(&device.public()) }).unwrap();
        let srv = Arc::new(server_config(&server, &d));
        let mut cli = client_config(&device, server.public());
        cli.resumption = Resumption::in_memory_sessions(4);
        let cli = Arc::new(cli);
        let connect = || {
            let (a, b) = tcp_pair();
            let srv = srv.clone();
            let t = std::thread::spawn(move || serve(srv, a));
            let r = dial(cli.clone(), b);
            let _ = t.join().unwrap();
            r
        };
        connect().unwrap();
        let again = connect().unwrap();
        assert_eq!(again.handshake_kind(), Some(HandshakeKind::Full), "server resumed a session");
        d.remove("phone").unwrap();
        assert!(connect().is_err(), "revoked device got back in");
    }

    #[test]
    fn certificate_fields() {
        let id = Identity::generate();
        let (certs, _) = certificate(&id);
        assert_eq!(cert_key(&certs[0]), Some(id.public()));
    }
}
