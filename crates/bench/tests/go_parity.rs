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

/// Go's osewa-bench built from the checkout, or None.
fn go_bench() -> Option<PathBuf> {
    let repo = go_repo()?;
    let bin = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("osewa-bench-go");
    match Command::new("go").args(["build", "-o"]).arg(&bin).arg("./bench/cmd/osewa-bench").current_dir(&repo).status() {
        Ok(s) if s.success() => Some(bin),
        _ => {
            eprintln!("skipping: can't build Go's osewa-bench");
            None
        }
    }
}

/// Runs `gen` for its duration and returns its output and the CPU it used
/// (user + system, from wait4).
fn gen_run(bin: &Path, args: &[&str]) -> (Vec<u8>, std::time::Duration) {
    use std::process::Stdio;
    let mut child = Command::new(bin).arg("gen").args(args).stdin(Stdio::null()).stdout(Stdio::piped()).spawn().unwrap();
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    let mut status = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let pid = child.id() as libc::pid_t;
    assert_eq!(unsafe { libc::wait4(pid, &mut status, 0, &mut ru) }, pid);
    std::mem::forget(child);
    let tv = |t: libc::timeval| std::time::Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    (out, tv(ru.ru_utime) + tv(ru.ru_stime))
}

/// Stamps differ by the clock; the rest is the same.
fn unstamped(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'@' && b.get(i + 1) == Some(&b'T') && i + 16 <= b.len() && b[i + 15] == b'@' && b[i + 2..i + 15].iter().all(u8::is_ascii_digit) {
            out.extend_from_slice(b"@T@");
            i += 16;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// gen writes Go's bytes, stamps aside, for every flood the e2e rig runs.
#[test]
fn gen_output_matches_go() {
    let Some(go) = go_bench() else { return };
    let rust = Path::new(env!("CARGO_BIN_EXE_osewa-bench"));
    for kind in ["w1", "w2", "w4"] {
        let a = ["-kind", kind, "-cols", "80", "-rows", "24", "-duration", "200ms"];
        let (g, _) = gen_run(&go, &a);
        let (r, _) = gen_run(rust, &a);
        let (g, r) = (unstamped(&g), unstamped(&r));
        let n = g.len().min(r.len());
        assert!(n > 100_000, "{kind}: {} and {} bytes", g.len(), r.len());
        assert!(g[..n] == r[..n], "{kind}: output differs from Go's at byte {:?}", g.iter().zip(&r).position(|(a, b)| a != b));
    }
}

/// gen costs no more CPU per byte than Go's: under an e2e flood it shares
/// the host's CPU with the holder, and a W2 flood is where the holder needs
/// it most (a gen that searched every block for its stamp placeholder
/// doubled e2e's W2 echo latency). Only meaningful optimized: run with
/// `cargo test --release`.
#[test]
fn gen_cpu_per_byte_at_most_go() {
    if cfg!(debug_assertions) {
        eprintln!("skipping: gen's CPU cost is measured in release builds");
        return;
    }
    let Some(go) = go_bench() else { return };
    let rust = Path::new(env!("CARGO_BIN_EXE_osewa-bench"));
    for kind in ["w1", "w2", "w4"] {
        let a = ["-kind", kind, "-cols", "80", "-rows", "24", "-duration", "1s"];
        let per_mb = |bin: &Path| {
            let (out, cpu) = gen_run(bin, &a);
            cpu.as_secs_f64() * 1e3 / (out.len() as f64 / 1e6)
        };
        // Best of three each, against noise.
        let g = (0..3).map(|_| per_mb(&go)).fold(f64::MAX, f64::min);
        let r = (0..3).map(|_| per_mb(rust)).fold(f64::MAX, f64::min);
        eprintln!("{kind}: Go {g:.3} ms/MB, Rust {r:.3} ms/MB");
        assert!(r <= g * 1.1, "{kind}: gen costs {r:.3} ms of CPU per MB, Go's {g:.3}");
    }
}
