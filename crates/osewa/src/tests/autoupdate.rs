use std::os::unix::fs::PermissionsExt;

use osewa_client::release::Release;

use super::{args, EnvGuard, TempDir};
use crate::autoupdate::{self, Mode};
use crate::cli::Out;

fn rel(v: &str, compatible: bool) -> Release {
    let bump = if compatible { 0 } else { 1 };
    Release { osewa: v.into(), version: osewa_wire::VERSION + bump, snapshot_version: osewa_client::SNAPSHOT_VERSION }
}

#[test]
fn notices() {
    use autoupdate::notice;
    // Development builds and up-to-date ones say nothing.
    assert_eq!(notice(None, Mode::Ask, &rel("0.2.0", true)), None);
    assert_eq!(notice(Some("0.2.0"), Mode::Ask, &rel("0.2.0", true)), None);
    assert_eq!(notice(Some("0.2.0"), Mode::Ask, &rel("0.1.9", true)), None);
    // Off says nothing at all.
    assert_eq!(notice(Some("0.1.0"), Mode::Off, &rel("0.2.0", false)), None);
    // Auto installs a compatible one quietly; ask names it.
    assert_eq!(notice(Some("0.1.0"), Mode::Auto, &rel("0.1.1", true)), None);
    let n = notice(Some("0.1.0"), Mode::Ask, &rel("0.1.1", true)).unwrap();
    assert!(n.contains("0.1.1") && n.contains("osewa update"), "{n}");
    // One that can't talk to this build is announced either way, never
    // installed on its own.
    for m in [Mode::Auto, Mode::Ask] {
        let n = notice(Some("0.1.0"), m, &rel("0.2.0", false)).unwrap();
        assert!(n.contains("osewa update <host>"), "{n}");
    }
}

#[test]
fn set_auto_update() {
    let mut g = EnvGuard::new();
    let dir = TempDir::new();
    g.set("OSEWA_CONFIG_DIR", dir.path().join("config"));
    g.set("OSEWA_AUTO_UPDATE", "");
    let run = |a: &[&str]| {
        let (out, err) = (super::Buf::default(), super::Buf::default());
        let code = autoupdate::set_main(&args(a), Out::new(out.clone()), Out::new(err.clone()));
        (code, out.string(), err.string())
    };
    assert_eq!(run(&[]), (0, "auto-update auto\n".into(), String::new()), "auto by default");
    assert_eq!(run(&["auto-update", "ask"]).0, 0);
    assert_eq!(autoupdate::mode(), Mode::Ask);
    assert_eq!(run(&["auto-update"]).1, "auto-update ask\n");
    let st = std::fs::metadata(dir.path().join("config/settings.json")).unwrap();
    assert_eq!(st.permissions().mode() & 0o777, 0o600);
    assert_eq!(run(&["auto-update", "sometimes"]).0, 2);
    assert_eq!(run(&["colour", "blue"]).0, 2);
    // The environment wins, for a host that must never update itself.
    g.set("OSEWA_AUTO_UPDATE", "off");
    assert_eq!(autoupdate::mode(), Mode::Off);
}

/// A stand-in osewa reporting version v.
fn fake_osewa(v: &str) -> Vec<u8> {
    format!("#!/bin/sh\n[ \"$1\" = version ] && echo '{{\"osewa\":\"{v}\",\"version\":0,\"snapshot_version\":1}}'\n").into_bytes()
}

#[test]
fn install_replaces_only_with_the_named_version() {
    let dir = TempDir::new();
    let path = dir.path().join("osewa");
    std::fs::write(&path, b"old").unwrap();

    // A download that isn't the version it should be leaves the old one.
    let e = autoupdate::install(&path, &fake_osewa("0.1.4"), "0.1.5").unwrap_err().to_string();
    assert!(e.contains("not 0.1.5"), "{e}");
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    // As does one that can't run here.
    assert!(autoupdate::install(&path, b"\x7fELF not this machine's", "0.1.5").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"old");

    autoupdate::install(&path, &fake_osewa("0.1.5"), "0.1.5").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), fake_osewa("0.1.5"));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, vec![std::ffi::OsString::from("osewa")], "no temporary files left");
}
