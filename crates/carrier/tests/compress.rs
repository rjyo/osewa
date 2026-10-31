//! Compression ratio and throughput of server-to-client terminal output
//! over the WebSocket carrier. Numbers mean something in release:
//!
//!     cargo test --release -p osewa-carrier --test compress -- --nocapture

mod common;

use std::time::Instant;

use common::{frames, paged_text, terminal_data, Counting};
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

/// A server writes the frames to a client over loopback TCP, each on its
/// own thread, so neither end's compression waits on the other's.
fn run(fs: Vec<Vec<u8>>, level: ws::Level) -> Run {
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

/// Sends data and checks the ratio is above min_ratio.
fn check(name: &str, data: &[u8], level: ws::Level, min_ratio: f64) {
    let r = run(frames(data), level);
    r.report(&format!("{name}: server -> client"));
    if cfg!(debug_assertions) {
        eprintln!("(a debug build: throughput is meaningless; run with --release)");
    }
    assert!(r.ratio() > min_ratio, "{:.2}x", r.ratio());
}

/// Mixed output: paged files, listings, a build log, top's redraws.
#[test]
fn terminal_output() {
    // Long enough to time; repeats are past deflate's 32 KiB window.
    check("mixed", &terminal_data().repeat(8), ws::Level::Fast, 3.5);
}

/// Text alone, as a pager shows it.
#[test]
fn text_output() {
    check("text", &paged_text().repeat(40), ws::Level::Fast, 3.0);
}


/// A full-screen TUI's screens (W2's, 80x24, as the holder's catch-up
/// sends them): libghostty snapshots, heavily styled. They deflate much
/// worse at the fast level than at deflate level 1; the server sends them
/// dense.
#[test]
fn tui_screens() {
    let z = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/w2-screens.deflate")).unwrap();
    let mut d = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::DeflateDecoder::new(&z[..]), &mut d).unwrap();
    check("tui", &d.repeat(4), ws::Level::Dense, 4.0);
}
