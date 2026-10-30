//! The workloads and the transfer pattern are byte-for-byte the Go bench
//! package's: the Go e2e harness checks patterns with its own code, and a
//! benchmark is only comparable on the same bytes. Skips without a Go
//! toolchain or the Go checkout (set OSEWA_GO_DIR).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use osewa_bench::{pattern, w4, workload};

fn go_repo() -> Option<PathBuf> {
    std::env::var_os("OSEWA_GO_DIR").map(PathBuf::from).or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .map(|a| a.join("osewa"))
            .find(|p| fs::read_to_string(p.join("go.mod")).is_ok_and(|m| m.starts_with("module github.com/rjyo/osewa\n")))
    })
}

/// The Go program's output, or None with a note.
fn go_output() -> Option<Vec<u8>> {
    let Some(repo) = go_repo() else {
        eprintln!("skipping Go parity: no Go osewa checkout (set OSEWA_GO_DIR)");
        return None;
    };
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("goparity");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/goparity/main.go"), dir.join("main.go")).unwrap();
    let repo_mod = fs::read_to_string(repo.join("go.mod")).unwrap();
    let go_line = repo_mod.lines().find(|l| l.starts_with("go ")).unwrap_or("go 1.24");
    let gomod = format!(
        "module goparity\n\n{go_line}\n\nrequire github.com/rjyo/osewa v0.0.0\n\nreplace github.com/rjyo/osewa => {}\n",
        repo.display()
    );
    fs::write(dir.join("go.mod"), gomod).unwrap();
    fs::copy(repo.join("go.sum"), dir.join("go.sum")).unwrap();
    match Command::new("go").args(["run", "."]).current_dir(&dir).env("GOFLAGS", "-mod=mod").output() {
        Ok(o) if o.status.success() => Some(o.stdout),
        Ok(o) => panic!("goparity: {}\n{}", o.status, String::from_utf8_lossy(&o.stderr)),
        Err(e) => {
            eprintln!("skipping Go parity: go: {e}");
            None
        }
    }
}

fn read_pattern(n: i64) -> Vec<u8> {
    let mut b = Vec::new();
    pattern::pattern(n).read_to_end(&mut b).unwrap();
    b
}

#[test]
fn workloads_and_pattern_match_go() {
    let Some(out) = go_output() else { return };
    let ours: Vec<(&str, Vec<u8>)> = vec![
        ("W1(2000)", workload::w1(2000)),
        ("W2(2000, 80, 24)", workload::w2(2000, 80, 24)),
        ("W2(2000, 200, 60)", workload::w2(2000, 200, 60)),
        ("W2(450, 120, 40)", workload::w2(450, 120, 40)),
        ("W3(2000)", workload::w3(2000)),
        ("W4(107, 80, 24)", w4::w4(2 * w4::W4_SWITCH + 7, 80, 24)),
        ("W4(60, 200, 60)", w4::w4(60, 200, 60)),
        ("Pattern(0)", read_pattern(0)),
        ("Pattern(1)", read_pattern(1)),
        ("Pattern(7)", read_pattern(7)),
        ("Pattern(8)", read_pattern(8)),
        ("Pattern(9)", read_pattern(9)),
        ("Pattern(1000)", read_pattern(1000)),
        ("Pattern(1<<20+3)", read_pattern((1 << 20) + 3)),
    ];
    let mut rest = &out[..];
    for (name, want) in ours {
        let n = u64::from_le_bytes(rest[..8].try_into().unwrap()) as usize;
        let go = &rest[8..8 + n];
        rest = &rest[8 + n..];
        if go != want.as_slice() {
            let at = go.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(go.len().min(want.len()));
            panic!("{name}: Go {} bytes, Rust {} bytes, first difference at {at}", go.len(), want.len());
        }
    }
    assert!(rest.is_empty(), "Go wrote more sections");
}
