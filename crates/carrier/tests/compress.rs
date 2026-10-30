//! Compression ratio and throughput of server-to-client terminal output,
//! the Rust carrier beside coder/websocket (osewa's Go server and client)
//! on the same frames. Numbers mean something in release:
//!
//!     cargo test --release -p osewa-carrier --test compress -- --nocapture

mod common;

use std::time::Instant;

use common::{frames, go_helper, paged_text, terminal_data, Counting};
use osewa_carrier::ws;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::LocalSet;

struct Run {
    payload: u64,
    wire: u64,
    secs: f64,
}

impl Run {
    fn ratio(&self) -> f64 {
        self.payload as f64 / self.wire as f64
    }
    fn report(&self, who: &str) {
        eprintln!(
            "{who}: {} bytes as {} on the wire, {:.2}x, {:.0} MB/s",
            self.payload,
            self.wire,
            self.ratio(),
            self.payload as f64 / self.secs / 1e6
        );
    }
}

/// A Rust server writes the frames to a Rust client over loopback TCP,
/// each on its own thread (as the Go run's goroutines are).
fn rust(fs: Vec<Vec<u8>>, level: ws::Level) -> Run {
    let rt = || tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap().to_string();
    let n = fs.len();
    let server = std::thread::spawn(move || {
        rt().block_on(LocalSet::new().run_until(async move {
            ln.set_nonblocking(true).unwrap();
            let ln = TcpListener::from_std(ln).unwrap();
            let (tcp, _) = ln.accept().await.unwrap();
            tcp.set_nodelay(true).unwrap();
            let conn = ws::accept(tcp, true).await.unwrap();
            // Nothing goes before the client counts.
            conn.read().await.unwrap();
            for f in &fs {
                conn.write_level(f, level).await.unwrap();
            }
            let _ = conn.read().await;
        }))
    });
    let run = rt().block_on(async {
        let tcp = TcpStream::connect(&addr).await.unwrap();
        let (s, read) = Counting::new(tcp);
        let conn = ws::dial(s, &addr).await.unwrap();
        let before = read.get();
        let start = Instant::now();
        conn.write(b"go").await.unwrap();
        let mut payload = 0;
        for _ in 0..n {
            payload += conn.read().await.unwrap().len() as u64;
        }
        let secs = start.elapsed().as_secs_f64();
        conn.close_now().unwrap();
        Run { payload, wire: read.get() - before, secs }
    });
    server.join().unwrap();
    run
}

/// Runs data both ways and compares; min_ratio bounds Rust's.
fn compare(name: &str, data: &[u8], level: ws::Level, min_ratio: f64) {
    let file = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("osewa-carrier-bench-{name}.bin"));
    std::fs::write(&file, data).unwrap();
    let r = rust(frames(data), level);
    r.report(&format!("{name}: rust -> rust"));
    if cfg!(debug_assertions) {
        eprintln!("(a debug build: throughput is meaningless; run with --release)");
    }
    assert!(r.ratio() > min_ratio, "{:.2}x", r.ratio());

    let Some(helper) = go_helper() else { return };
    let out = std::process::Command::new(helper).arg("bench").arg(&file).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}");
    let v: Vec<&str> = stdout.trim().strip_prefix("bench ").expect("bench line").split(' ').collect();
    let g = Run { payload: v[0].parse().unwrap(), wire: v[1].parse().unwrap(), secs: v[2].parse().unwrap() };
    g.report(&format!("{name}: go   -> go  "));
    assert_eq!(g.payload, r.payload, "the same frames");
    // Within a few percent of coder/websocket's flate.BestSpeed.
    let rel = r.ratio() / g.ratio();
    eprintln!("{name}: rust ratio / go ratio: {rel:.3}");
    assert!(rel > 0.95, "rust {:.2}x, go {:.2}x", r.ratio(), g.ratio());
}

/// Mixed output: paged files, listings, a build log, top's redraws.
#[test]
fn terminal_output() {
    // Long enough to time; repeats are past deflate's 32 KiB window.
    compare("mixed", &terminal_data().repeat(8), ws::Level::Fast, 3.5);
}

/// Text alone, as a pager shows it.
#[test]
fn text_output() {
    compare("text", &paged_text().repeat(40), ws::Level::Fast, 3.0);
}


/// A full-screen TUI's screens (W2's, 80x24, as the holder's catch-up
/// sends them): libghostty snapshots, heavily styled. They deflate much
/// worse at the fast level than at BestSpeed; the server sends them dense.
#[test]
fn tui_screens() {
    let z = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/w2-screens.deflate")).unwrap();
    let mut d = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::DeflateDecoder::new(&z[..]), &mut d).unwrap();
    compare("tui", &d.repeat(4), ws::Level::Dense, 4.0);
}
