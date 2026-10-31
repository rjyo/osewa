//! The device's list of enrolled servers, hosts.json in the osewa config
//! directory, shared by every client on the machine (the CLI, app-hook and
//! any other v0 client, so its format is fixed). It pins each server's key, so
//! it is trusted only when private.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use osewa_wire as wire;
use serde::{Deserialize, Serialize};

/// One enrolled server in hosts.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Host {
    pub name: String,
    pub addr: String,
    pub server_key: String,
    /// The ssh target the host was enrolled through (`user@host`, `host`,
    /// or an ssh_config alias, as given to login), so other programs that
    /// name hosts by ssh target can find the enrollment for exactly that
    /// account. Empty for hosts saved before it was recorded.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ssh: String,
    /// The UDP port the host's server last advertised QUIC on (WELCOME
    /// carriers), 0 for none: connections then race QUIC and the WebSocket
    /// (`Config::quic_port`).
    #[serde(skip_serializing_if = "is_zero")]
    pub quic_port: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Why a hosts call failed.
#[derive(Debug)]
pub enum Error {
    /// Find didn't find the host.
    Unknown(String),
    Io(io::Error),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unknown(name) => write!(f, "unknown host {}; run osewa login first", wire::quote(name)),
            Error::Io(e) => e.fmt(f),
            Error::Invalid(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<Error> for crate::Error {
    fn from(e: Error) -> crate::Error {
        match e {
            Error::Io(e) => e.into(),
            e => crate::Error::Other(e.to_string()),
        }
    }
}

impl Error {
    pub fn is_unknown(&self) -> bool {
        matches!(self, Error::Unknown(_))
    }

    fn is_not_found(&self) -> bool {
        matches!(self, Error::Io(e) if e.kind() == io::ErrorKind::NotFound)
    }
}

/// The config directory: $OSEWA_CONFIG_DIR, else $XDG_CONFIG_HOME/osewa,
/// else ~/.config/osewa.
pub fn default_dir() -> PathBuf {
    osewa_paths::config_dir()
}

/// hosts.json in dir.
pub fn path(dir: &Path) -> PathBuf {
    dir.join("hosts.json")
}

/// The device key in dir, created by the first login.
pub fn key_path(dir: &Path) -> PathBuf {
    dir.join("client.key")
}

/// Returns every host in dir, none if there is no hosts.json yet.
pub fn load(dir: &Path) -> Result<Vec<Host>, Error> {
    // hosts.json pins server keys: whoever could rewrite it could stand in
    // for a host. Trust it only where nobody else can write.
    match osewa_paths::check_private(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    if let Err(e) = check_private_file(&path(dir)) {
        if e.is_not_found() {
            return Ok(Vec::new());
        }
        return Err(e);
    }
    let b = fs::read(path(dir))?;
    wire::decode_json::<Option<Vec<Host>>>(&b)
        .map(Option::unwrap_or_default)
        .map_err(|e| Error::Invalid(format!("{}: {e}", path(dir).display())))
}

/// Adds h to dir's hosts.json, replacing a host of the same name.
/// Concurrent saves, from other processes too, all land.
pub fn save(dir: &Path, h: Host) -> Result<(), Error> {
    osewa_paths::ensure(dir)?;
    // Concurrent logins (other processes included) must not lose a host.
    let _lock = lock_file(&dir.join("hosts.json.lock"))?;
    let mut hosts = load(dir)?;
    let mut replaced = false;
    for x in hosts.iter_mut() {
        if x.name == h.name {
            *x = h.clone();
            replaced = true;
        }
    }
    if !replaced {
        hosts.push(h);
    }
    let mut b = marshal_indent(&hosts);
    b.push(b'\n');
    // Write then rename, so a crash never leaves a truncated file.
    let tmp = dir.join(format!(".hosts-{}", tmp_suffix()));
    let res = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(&b)?;
        drop(f);
        fs::rename(&tmp, path(dir))
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(res?)
}

fn tmp_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: AtomicU32 = AtomicU32::new(0);
    let ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    format!("{}-{}-{ns}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// hosts.json's layout: two-space indented JSON (serde_json's pretty
/// form), with <, > and & (and U+2028, U+2029) escaped in strings, so
/// every client rewrites the file to the same bytes.
fn marshal_indent<T: Serialize>(v: &T) -> Vec<u8> {
    let b = serde_json::to_vec_pretty(v).expect("hosts: marshal");
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => out.extend_from_slice(b"\\u003c"),
            b'>' => out.extend_from_slice(b"\\u003e"),
            b'&' => out.extend_from_slice(b"\\u0026"),
            0xe2 if b.get(i + 1) == Some(&0x80) && matches!(b.get(i + 2), Some(0xa8 | 0xa9)) => {
                out.extend_from_slice(if b[i + 2] == 0xa8 { b"\\u2028" } else { b"\\u2029" });
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// An exclusive flock, released when dropped.
struct Lock(#[allow(dead_code)] fs::File);

fn lock_file(p: &Path) -> io::Result<Lock> {
    let f = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(p)?;
    loop {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(Lock(f));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Errors unless p is a regular file (not a symlink) owned by this user
/// with no group or other permissions.
fn check_private_file(p: &Path) -> Result<(), Error> {
    let st = fs::symlink_metadata(p)?;
    if !st.file_type().is_file() {
        return Err(Error::Invalid(format!("{} is not a regular file", p.display())));
    }
    let uid = unsafe { libc::getuid() };
    if st.uid() != uid {
        return Err(Error::Invalid(format!("{} is owned by uid {}, not {uid}", p.display(), st.uid())));
    }
    let perm = st.permissions().mode() & 0o777;
    if perm & 0o077 != 0 {
        return Err(Error::Invalid(format!(
            "{} has mode {}; it must be private (chmod 600)",
            p.display(),
            perm_string(perm)
        )));
    }
    Ok(())
}

/// Permission bits as ls shows them: -rw-r--r--.
fn perm_string(perm: u32) -> String {
    let mut s = String::from("-");
    for i in (0..9).rev() {
        s.push(if perm & (1 << i) == 0 { '-' } else { b"xwr"[i % 3] as char });
    }
    s
}

/// Returns the host called name.
pub fn find(dir: &Path, name: &str) -> Result<Host, Error> {
    load(dir)?.into_iter().find(|h| h.name == name).ok_or_else(|| Error::Unknown(name.to_string()))
}

impl Host {
    /// What [`crate::Conn::dial`] needs to reach the host as this device:
    /// its URL, and mutual TLS with the device key in dir and the host's
    /// pinned server key. client_id names the app instance (keep it for
    /// the instance's life), name the device as the server shows it.
    pub fn config(&self, dir: &Path, client_id: &str, name: &str) -> Result<crate::Config, Error> {
        let id = osewa_auth::Identity::load_or_create(&key_path(dir)).map_err(|e| match e {
            osewa_auth::Error::Io(e) => Error::Io(e),
            e => Error::Invalid(e.to_string()),
        })?;
        let key = osewa_auth::decode_key(&self.server_key)
            .map_err(|e| Error::Invalid(format!("host {}: {e}", wire::quote(&self.name))))?;
        let (dir, h) = (dir.to_path_buf(), self.clone());
        Ok(crate::Config {
            url: format!("wss://{}{}", self.addr, wire::PATH),
            tls: Some(Arc::new(osewa_auth::client_config(&id, key))),
            client_id: client_id.to_string(),
            name: name.to_string(),
            quic_port: u16::try_from(self.quic_port).unwrap_or(0),
            // Remember what the server advertises, for the next connection.
            on_carriers: Some(Arc::new(move |cs: &[wire::Carrier]| remember_quic(&dir, &h, cs))),
            ..Default::default()
        })
    }
}

/// Saves h's QUIC port as its server advertised it, when it changed and the
/// host is still enrolled as it was.
fn remember_quic(dir: &Path, h: &Host, cs: &[wire::Carrier]) {
    let mut port = 0;
    for c in cs {
        if c.carrier == "quic" && c.port > 0 {
            port = c.port;
        }
    }
    if port == h.quic_port {
        return;
    }
    let Ok(mut cur) = find(dir, &h.name) else { return };
    if cur.addr != h.addr || cur.server_key != h.server_key || cur.quic_port == port {
        return;
    }
    cur.quic_port = port;
    let _ = save(dir, cur);
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A private temporary directory, removed when dropped.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("osewa-client-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
            TempDir(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    pub fn chmod(p: &Path, m: u32) {
        fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn h(name: &str, addr: &str, key: &str) -> Host {
        Host { name: name.into(), addr: addr.into(), server_key: key.into(), ..Default::default() }
    }

    #[test]
    fn hosts() {
        let t = TempDir::new();
        let dir = t.path().join("cfg");
        assert!(load(&dir).unwrap().is_empty());
        let err = find(&dir, "a").unwrap_err();
        assert!(err.to_string().contains("osewa login"), "missing host: {err}");
        save(&dir, h("a", "a:1", "k1")).unwrap();
        save(&dir, h("b", "b:1", "k2")).unwrap();
        save(&dir, h("a", "a:2", "k3")).unwrap();
        assert_eq!(load(&dir).unwrap().len(), 2);
        let a = find(&dir, "a").unwrap();
        assert!(a.addr == "a:2" && a.server_key == "k3", "a = {a:?}");
        assert_eq!(mode(&path(&dir)), 0o600);
        assert_eq!(mode(&dir), 0o700);

        fs::write(path(&dir), "{").unwrap();
        assert!(load(&dir).is_err(), "want error for corrupt hosts.json");
        assert!(save(&dir, h("c", "", "")).is_err(), "save over corrupt file");
    }

    #[test]
    fn save_host_concurrent() {
        let t = TempDir::new();
        let dir = t.path().to_path_buf();
        let threads: Vec<_> = (0..20)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || save(&dir, h(&format!("h{i}"), "a:1", "")).unwrap())
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(load(&dir).unwrap().len(), 20);
    }

    // #13: hosts.json pins server keys, so it's trusted only when private.
    #[test]
    fn hosts_must_be_private() {
        let t = TempDir::new();
        let dir = t.path();
        save(dir, h("a", "a:1", "k")).unwrap();
        find(dir, "a").unwrap();

        chmod(&path(dir), 0o644);
        let err = load(dir).unwrap_err();
        assert!(err.to_string().contains("private"), "world-readable hosts.json trusted: {err}");
        assert!(err.to_string().contains("-rw-r--r--"), "{err}");
        chmod(&path(dir), 0o600);

        chmod(dir, 0o755);
        assert!(load(dir).is_err(), "hosts.json trusted in a shared directory");
        chmod(dir, 0o700);

        // A symlink could point anywhere.
        let t2 = TempDir::new();
        let other = t2.path().join("hosts.json");
        fs::write(&other, fs::read(path(dir)).unwrap()).unwrap();
        chmod(&other, 0o600);
        fs::remove_file(path(dir)).unwrap();
        std::os::unix::fs::symlink(&other, path(dir)).unwrap();
        assert!(load(dir).is_err(), "symlinked hosts.json trusted");

        // No file yet is fine.
        let t3 = TempDir::new();
        assert!(load(t3.path()).unwrap().is_empty());
        load(&t3.path().join("missing")).unwrap();
    }

    #[test]
    fn config() {
        let t = TempDir::new();
        let dir = t.path();
        let mut host = h("box", "box:7322", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let cfg = host.config(dir, "app-1", "laptop").unwrap();
        assert_eq!(cfg.url, "wss://box:7322/osewa");
        assert!(cfg.client_id == "app-1" && cfg.name == "laptop");
        assert!(cfg.tls.as_ref().unwrap().client_auth_cert_resolver.has_certs(), "no client certificate");
        assert!(key_path(dir).exists(), "no device key written");
        host.server_key = "nope".into();
        let err = host.config(dir, "a", "b").unwrap_err();
        assert!(err.to_string().contains("box"), "bad server key: {err}");
        assert!(find(dir, "x").unwrap_err().is_unknown());
        assert!(!default_dir().as_os_str().is_empty(), "no default dir");
    }

    // The ssh target round-trips, and hosts.json from before it loads.
    #[test]
    fn host_ssh() {
        let t = TempDir::new();
        let dir = t.path().join("cfg");
        save(&dir, Host { ssh: "alice@box".into(), ..h("a", "a:1", "k1") }).unwrap();
        save(&dir, h("b", "b:1", "k2")).unwrap();
        assert_eq!(find(&dir, "a").unwrap().ssh, "alice@box");
        let b = fs::read_to_string(path(&dir)).unwrap();
        assert_eq!(b.matches("\"ssh\"").count(), 1, "hosts.json writes an empty ssh: {b}");
        fs::write(path(&dir), r#"[{"name":"old","addr":"o:1","server_key":"k"}]"#).unwrap();
        let old = find(&dir, "old").unwrap();
        assert!(old.ssh.is_empty() && old.server_key == "k", "old = {old:?}");
    }

    // The v0 hosts.json bytes, and a newline.
    #[test]
    fn hosts_json_is_v0() {
        let t = TempDir::new();
        let dir = t.path();
        save(dir, Host { ssh: "me@<box>&".into(), quic_port: 7323, ..h("a", "a:1", "k1") }).unwrap();
        save(dir, h("b", "[::1]:7322", "k2")).unwrap();
        let want = "[\n  {\n    \"name\": \"a\",\n    \"addr\": \"a:1\",\n    \"server_key\": \"k1\",\n    \"ssh\": \"me@\\u003cbox\\u003e\\u0026\",\n    \"quic_port\": 7323\n  },\n  {\n    \"name\": \"b\",\n    \"addr\": \"[::1]:7322\",\n    \"server_key\": \"k2\"\n  }\n]\n";
        assert_eq!(fs::read_to_string(path(dir)).unwrap(), want);
        assert_eq!(find(dir, "a").unwrap().ssh, "me@<box>&");
    }

    // A WELCOME's QUIC port is remembered for the host as enrolled.
    #[test]
    fn remembers_quic_port() {
        let t = TempDir::new();
        let dir = t.path();
        let host = h("box", "box:7322", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        save(dir, host.clone()).unwrap();
        let cfg = host.config(dir, "a", "b").unwrap();
        let cb = cfg.on_carriers.unwrap();
        cb(&[wire::Carrier { carrier: "quic".into(), port: 7400, alpn: vec![] }]);
        assert_eq!(find(dir, "box").unwrap().quic_port, 7400);
        // An empty list forgets it.
        let cfg = find(dir, "box").unwrap().config(dir, "a", "b").unwrap();
        (cfg.on_carriers.unwrap())(&[]);
        assert_eq!(find(dir, "box").unwrap().quic_port, 0);
        // A host enrolled again meanwhile is left alone.
        save(dir, Host { server_key: "other".into(), ..host.clone() }).unwrap();
        cb(&[wire::Carrier { carrier: "quic".into(), port: 9, alpn: vec![] }]);
        assert_eq!(find(dir, "box").unwrap().quic_port, 0);
    }
}
