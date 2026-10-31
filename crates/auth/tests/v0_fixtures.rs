//! The protocol v0 on-disk format: a server key and devices files in
//! testdata/v0 load here, and get rewritten byte-for-byte as v0 lays them
//! out. devices.removed is devices.full after removing "laptop". Other
//! osewa implementations share a host's config directory, so the bytes
//! must match, not only the meaning.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use osewa_auth::*;

const TESTDATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/v0");

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "osewa-auth-v0-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies a fixture in with mode 0600 (git keeps only the executable bit).
fn fixture(name: &str, to: &Path) {
    fs::copy(Path::new(TESTDATA).join(name), to).unwrap();
    fs::set_permissions(to, fs::Permissions::from_mode(0o600)).unwrap();
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(Path::new(TESTDATA).join(name)).unwrap()
}

#[test]
fn v0_key_loads() {
    let t = TempDir::new();
    let path = t.0.join("server.key");
    fixture("server.key", &path);
    let id = Identity::load_or_create(&path).unwrap();
    assert_eq!(encode_key(&id.public()), read_fixture("server.pub").trim());

    // A key we create has the v0 layout: same length, same PKCS#8 header.
    let ours = t.0.join("ours.key");
    Identity::load_or_create(&ours).unwrap();
    let (v0, rs) = (read_fixture("server.key"), fs::read_to_string(&ours).unwrap());
    assert_eq!(v0.len(), rs.len());
    let header = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEI";
    assert!(v0.starts_with(header) && rs.starts_with(header), "{rs}");
}

#[test]
fn v0_devices_load_and_rewrite_identically() {
    let t = TempDir::new();
    let d = Devices::new(t.0.join("devices"));
    fixture("devices.full", &d.path);
    let names: Vec<String> = d.list().unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["phone", "laptop", "jyo's <iPad> & co\u{2028}"]);
    d.remove("laptop").unwrap();
    assert_eq!(fs::read_to_string(&d.path).unwrap(), read_fixture("devices.removed"));
    assert_eq!(fs::metadata(&d.path).unwrap().permissions().mode() & 0o777, 0o600);
}
