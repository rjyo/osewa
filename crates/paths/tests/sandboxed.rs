//! Every test harness that starts an osewa server or holder process gives
//! it a sandbox home (osewa_paths::sandbox, or OSEWA_HOME): a process
//! started without one runs its sessions in the real home, where a test's
//! shell history, installs and files then land.

use std::path::{Path, PathBuf};

/// Test sources: crates/*/tests/**, crates/*/src/tests/**, crates/*/src/tests.rs.
fn test_sources(crates: &Path) -> Vec<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(es) = std::fs::read_dir(d) else { return };
        for e in es.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for c in std::fs::read_dir(crates).unwrap().flatten() {
        let c = c.path();
        walk(&c.join("tests"), &mut out);
        walk(&c.join("src/tests"), &mut out);
        let t = c.join("src/tests.rs");
        if t.exists() {
            out.push(t);
        }
    }
    out
}

/// What starts an osewa server or holder process from a test.
const STARTS: &[&str] = &["\"server\", \"--listen\"", "SELF_ARGS", "start_daemon(", "cli::serve("];
// In-process servers (Server::new) are made by each crate's harness, Env and
// auth_config in the server's tests, LiveHost and Host elsewhere, which give
// Config.home; their tests check the session's home.
/// What gives it a sandbox home.
const SANDBOXES: &[&str] = &["sandbox(", "sandbox_home(", "HOME_OVERRIDE", "host_dirs("];

#[test]
fn every_spawned_server_and_holder_is_sandboxed() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let files = test_sources(&crates);
    assert!(files.len() > 20, "found only {} test sources under {}", files.len(), crates.display());
    let mut starting = 0;
    for f in files {
        let s = std::fs::read_to_string(&f).unwrap();
        if !STARTS.iter().any(|p| s.contains(p)) {
            continue;
        }
        starting += 1;
        assert!(
            SANDBOXES.iter().any(|p| s.contains(p)),
            "{} starts an osewa server or holder without a sandbox home: use osewa_paths::sandbox on its Command",
            f.display()
        );
    }
    assert!(starting >= 3, "the check found only {starting} harnesses: its patterns are stale");
}
