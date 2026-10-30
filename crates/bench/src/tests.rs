//! Go's bench/cmd/osewa-bench/main_test.go and serve_test.go, and the bench
//! package's own tests (pattern_test.go and the workload tests of
//! bench_test.go).

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use osewa_ghostty::{self as ghostty, Terminal};

use crate::gen::{gen_with, units, workload, Generator, BLOCK_LINES, FRAMED_PREFIX, PLACEHOLDER};
use crate::pattern::{check_pattern, pattern};
use crate::resume::{resume_main, visible_text};
use crate::serve::bytes_main;
use crate::stamp::{from_unix_ms, newest_stamp, stamp, stamp_indexes};
use crate::w4::{fit, w4, w4_frames, STAMP_PLACEHOLDER, W4_SWITCH};
use crate::workload::{escape_share, r1, w1, w2, w3, R1_COLS, R1_LINES, R1_ROWS};
use crate::{clock_main, run};

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn read_all(mut r: impl Read) -> Vec<u8> {
    let mut b = Vec::new();
    r.read_to_end(&mut b).unwrap();
    b
}

struct FailWriter;

impl Write for FailWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("closed"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer threads can share, as Go's syncBuffer.
#[derive(Clone, Default)]
struct SyncBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SyncBuffer {
    fn write(&mut self, p: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(p);
        Ok(p.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SyncBuffer {
    fn string(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// io.Pipe's read end: what the sender writes, EOF once it is dropped.
struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
}

fn pipe() -> (Sender<Vec<u8>>, PipeReader) {
    let (tx, rx) = mpsc::channel();
    (tx, PipeReader { rx, buf: Vec::new() })
}

impl Read for PipeReader {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(b) => self.buf = b,
                Err(_) => return Ok(0),
            }
        }
        let n = p.len().min(self.buf.len());
        p[..n].copy_from_slice(&self.buf[..n]);
        self.buf.drain(..n);
        Ok(n)
    }
}

fn new_term(cols: usize, rows: usize, scrollback: usize) -> Terminal {
    Terminal::new(cols, rows, ghostty::Options { scrollback_bytes: scrollback, continuation: true, ..Default::default() }).unwrap()
}

// pattern_test.go

#[test]
fn pattern_deterministic() {
    let a = read_all(pattern(100_000));
    let b = read_all(pattern(100_000));
    assert!(a.len() == 100_000 && a == b, "pattern: {} bytes, equal {}", a.len(), a == b);
    assert!(a.iter().filter(|&&c| c == a[0]).count() <= 1000, "pattern looks constant");
    assert_eq!(read_all(pattern(10)), a[..10], "pattern(10) is not a prefix of pattern(100000)");
}

/// Reads one byte at a time, as iotest.OneByteReader.
struct OneByte<R>(R);

impl<R: Read> Read for OneByte<R> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if p.is_empty() {
            return Ok(0);
        }
        self.0.read(&mut p[..1])
    }
}

/// Fails every read, as iotest.ErrReader.
struct ErrReader;

impl Read for ErrReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("boom"))
    }
}

#[test]
fn check_pattern_errors() {
    let p = read_all(pattern(70_000));
    check_pattern(OneByte(&p[..]), 70_000).unwrap();
    let mut bad = p.clone();
    bad[65_537] ^= 1;
    let mut long = p.clone();
    long.push(0);
    for (data, want) in [(&bad[..], "byte 65537"), (&p[..69_999], "short: 69999 of 70000"), (&long[..], "more than 70000")] {
        let err = check_pattern(data, 70_000).expect_err(want).to_string();
        assert!(err.contains(want), "check_pattern = {err}, want {want:?}");
    }
    let err = check_pattern((&p[..10]).chain(ErrReader), 70_000).unwrap_err();
    assert!(err.to_string().contains("boom") && err.to_string().contains("read at 10"), "check_pattern = {err}, want boom");
    let err = check_pattern(ErrReader, 0).unwrap_err();
    assert!(err.to_string().contains("boom"), "check_pattern on empty = {err}, want boom");
}

// bench_test.go: the workloads' own tests.

#[test]
fn workloads_deterministic() {
    let fs: [fn() -> Vec<u8>; 4] = [|| w1(300), || w2(450, 120, 40), || w3(300), || w4(120, 100, 30)];
    for f in fs {
        let (a, b) = (f(), f());
        assert!(a == b && !a.is_empty(), "workload is not deterministic");
    }
}

#[test]
fn stamps() {
    let at = from_unix_ms(1761645600123);
    let s = format!(
        "x\n{}y {}@T12@",
        String::from_utf8(stamp(at - Duration::from_secs(1))).unwrap(),
        String::from_utf8(stamp(at)).unwrap()
    );
    assert_eq!(newest_stamp(&s), Some(at));
    assert_eq!(stamp_indexes(&s).len(), 2, "want two stamps");
    assert_eq!(newest_stamp("nothing"), None, "stamp found in plain text");
}

/// W4 is what it claims: frames on the alternate screen with both views,
/// few distinct styles, and a stamp slot in every frame.
#[test]
fn w4_frames_shape() {
    for (i, f) in w4_frames(2 * W4_SWITCH, 80, 24).iter().enumerate() {
        let s = String::from_utf8_lossy(f);
        assert!(s.starts_with("\x1b[?2026h") && s.contains(STAMP_PLACEHOLDER), "frame {i} lacks the sync bracket or stamp slot");
    }
    let mut term = new_term(80, 24, 0);
    term.write(&w4(W4_SWITCH + 1, 80, 24));
    assert!(term.alt_screen(), "W4 is not on the alternate screen");
    let txt = term.text();
    assert!(txt.contains("package holder") && txt.contains("NORMAL"), "code view missing:\n{txt}");
    let mut tbl = new_term(80, 24, 0);
    tbl.write(&w4(3, 80, 24));
    let txt = tbl.text();
    assert!(txt.contains("PID USER") && txt.contains("F10Quit"), "table view missing:\n{txt}");
    let all = String::from_utf8(w4(2 * W4_SWITCH, 80, 24)).unwrap();
    let styles = sgrs(&all);
    assert!(styles.len() <= 12, "W4 uses {} distinct SGRs, want few: {styles:?}", styles.len());
    assert!(fit("abc", 2) == "ab" && fit("a", 3) == "a  ", "fit");
}

/// The distinct `\x1b\[[0-9;]*m` sequences in s.
fn sgrs(s: &str) -> std::collections::BTreeSet<String> {
    let b = s.as_bytes();
    let mut out = std::collections::BTreeSet::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == 0x1b && b[i + 1] == b'[' {
            let mut j = i + 2;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            if j < b.len() && b[j] == b'm' {
                out.insert(s[i..=j].to_string());
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The screen snapshot (through READY) of a terminal fed p.
fn screen_size(p: &[u8], cols: usize, rows: usize) -> usize {
    let mut t = new_term(cols, rows, osewa_holder::DEFAULT_SCROLLBACK_BYTES);
    t.write(p);
    t.snapshot_screen().unwrap().len()
}

#[test]
fn screen_sizes() {
    let w2s = screen_size(&w2(400, 200, 60), 200, 60);
    let w4s = screen_size(&w4(60, 200, 60), 200, 60);
    assert!(w4s < w2s, "W4 screen ({w4s} B) not smaller than W2's ({w2s} B)");
}

#[test]
fn workload_shape() {
    assert_eq!(escape_share(&w1(500)), 0.0, "W1 escape share");
    let s = escape_share(&w2(2000, R1_COLS, R1_ROWS));
    assert!((0.35..=0.45).contains(&s), "W2 escape share = {s:.2}, want about 0.40");
    let w3s = String::from_utf8(w3(500)).unwrap();
    for want in ["\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", "\u{65e5}\u{672c}\u{8a9e}", "e\u{301}"] {
        assert!(w3s.contains(want), "W3 lacks {want:?}");
    }
    assert_eq!(escape_share(b"a\x1b[1;31mb\x1b7c\x1b"), 10.0 / 13.0);
    assert_eq!(escape_share(b""), 0.0);
}

/// The fixture is what it claims: 10,000 lines of scrollback on the primary
/// screen, and a snapshot that restores them.
#[test]
fn r1_fixture() {
    let mut term = new_term(R1_COLS, R1_ROWS, 64 << 20);
    term.write(&r1());
    let snap = term.snapshot().unwrap();
    assert!(!term.alt_screen(), "R1 ends on the alternate screen");
    let got = ghostty::restore(&snap, Default::default()).unwrap();
    let b = got.format(ghostty::Format::Plain, true, false).unwrap();
    let n = b.iter().filter(|&&c| c == b'\n').count() + 1;
    assert!(n >= R1_LINES, "restored {n} lines, want at least {R1_LINES}");
    assert!(term.text() == got.text(), "restored R1 differs from the source terminal");
}

// main_test.go

#[test]
fn units_keep_lines() {
    let u = units(b"a\r\n\x1b[?1049hredraw\x1b[?1049lb\r\ntail");
    let want: [&[u8]; 3] = [b"a\r\n", b"\x1b[?1049hredraw\x1b[?1049lb\r\n", b"tail"];
    assert_eq!(u, want, "units");
    for k in ["w1", "w2", "w3", "w4"] {
        let u = workload(k, 80, 24).unwrap();
        assert!(u.len() >= 100, "workload {k}: {} units", u.len());
    }
    assert!(workload("w9", 80, 24).is_err(), "unknown workload accepted");
}

/// A clock that moves a millisecond per reading, as the Go test's.
fn fake_clock(start: SystemTime) -> crate::gen::Clock {
    let mut t = start;
    Box::new(move || {
        t += Duration::from_millis(1);
        t
    })
}

#[test]
fn gen_stamps_every_block() {
    let mut b = Vec::new();
    let u = workload("w1", 80, 24).unwrap();
    Generator::new(&mut b, u, 0, Duration::from_millis(50), fake_clock(from_unix_ms(1761645600000))).run().unwrap();
    let s = String::from_utf8(b).unwrap();
    let blocks = stamp_indexes(&s);
    assert!(blocks.len() >= 10, "{} stamps in {} bytes", blocks.len(), s.len());
    for w in blocks.windows(2) {
        let lines = s[w[0][0]..w[1][0]].matches('\n').count();
        assert_eq!(lines, BLOCK_LINES + 1, "stamps {lines} lines apart");
    }
}

#[test]
fn gen_paces() {
    let mut b = Vec::new();
    let start = Instant::now();
    assert_eq!(run(&args(&["gen", "-kind", "w2", "-rate", "40000", "-duration", "500ms"]), &mut b, &mut io::stderr()), 0, "gen failed");
    let el = start.elapsed();
    // 40 kB/s for half a second, give or take a block.
    assert!(b.len() <= 20000 + 4096 && el >= Duration::from_millis(400), "wrote {} bytes in {el:?}", b.len());
}

#[test]
fn gen_write_error() {
    let u = workload("w3", 80, 24).unwrap();
    let mut w = FailWriter;
    assert!(Generator::new(&mut w, u, 0, Duration::ZERO, Box::new(SystemTime::now)).run().is_err(), "write error ignored");
}

#[derive(serde::Deserialize, Debug)]
pub(crate) struct TypeResult {
    pub latencies_ms: Option<Vec<f64>>,
    pub timeouts: i64,
}

#[test]
fn drive_type() {
    let mut out = Vec::new();
    let code = run(
        &args(&["drive", "-mode", "type", "-ready", "", "-settle", "100ms", "-keys", "12", "-line", "5", "-interval", "5ms", "--", "cat"]),
        &mut out,
        &mut io::stderr(),
    );
    assert_eq!(code, 0, "drive failed");
    assert!(out.ends_with(b"}\n"), "not one JSON line: {:?}", String::from_utf8_lossy(&out));
    let r: TypeResult = serde_json::from_slice(&out).unwrap();
    assert!(r.latencies_ms.as_ref().is_some_and(|l| l.len() == 12) && r.timeouts == 0, "result {r:?}");
}

#[test]
fn errors() {
    let (mut out, mut errb) = (Vec::new(), Vec::new());
    for a in [
        &[][..],
        &["bogus"],
        &["gen", "-kind", "nope"],
        &["gen", "-badflag"],
        &["drive"],
        &["drive", "-mode", "nope", "-ready", "", "-settle", "0", "--", "cat"],
        &["drive", "-ready", "never-shows", "--", "true"],
        &["drive", "--", "/nonexistent/binary"],
        &["resume"],
        &["resume", "-addr", "x:1", "-session", "s", "-key", "k", "-server-key", "!!"],
        &["resume", "-badflag"],
    ] {
        assert_ne!(run(&args(a), &mut out, &mut errb), 0, "{a:?} succeeded");
    }
    let errs = String::from_utf8_lossy(&errb);
    assert!(errs.contains("usage"), "no usage: {errs}");
    // The messages are the Go helper's.
    for want in [
        "osewa-bench: unknown command \"bogus\"",
        "osewa-bench: unknown workload \"nope\"",
        "osewa-bench: flag provided but not defined: -badflag",
        "osewa-bench: drive: no command",
        "osewa-bench: drive: unknown mode \"nope\"",
        "osewa-bench: drive: \"never-shows\" never appeared:",
        "osewa-bench: fork/exec /nonexistent/binary: no such file or directory",
        "osewa-bench: resume: -addr, -session and -key are required",
    ] {
        assert!(errs.contains(want), "stderr lacks {want:?}:\n{errs}");
    }
    assert_eq!(run(&[], &mut out, &mut Vec::new()), 2, "no command");
}

/// Typed bytes are drawn on the last row between blocks, edit keys work,
/// and ^C stops the flood.
#[test]
fn gen_echo() {
    for rate in ["0", "200000"] {
        let (tx, pr) = pipe();
        let out = SyncBuffer::default();
        let mut w = out.clone();
        let a = args(&["-kind", "w1", "-echo", "-rows", "10", "-rate", rate]);
        let done = std::thread::spawn(move || gen_with(&a, pr, &mut w));
        tx.send(b"abx\x7fc".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        tx.send(b"\x15zz\r".to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        tx.send(b"\x03".to_vec()).unwrap();
        done.join().unwrap().unwrap();
        let s = out.string();
        for want in ["\x1b[1;9r", "\x1b7\x1b[10;1H\x1b[2K> abc\x1b8", "> zz\x1b8", "> \x1b8"] {
            assert!(s.contains(want), "rate {rate}: output lacks {want:?}");
        }
    }
    // End of input stops it too.
    let mut out = SyncBuffer::default();
    gen_with(&args(&["-kind", "w1", "-echo", "-rate", "100000"]), &b"q"[..], &mut out).unwrap();
}

#[test]
fn repaints_stamped() {
    let u = workload("w2", 80, 24).unwrap();
    let mut b = Vec::new();
    Generator::new(&mut b, u, 0, Duration::from_millis(200), Box::new(SystemTime::now)).run().unwrap();
    let s = String::from_utf8_lossy(&b);
    assert!(!s.contains(std::str::from_utf8(PLACEHOLDER).unwrap()), "placeholder left in output");
    assert!(s.contains("\x1b[?1049h\x1b[H\x1b[2J\x1b[24;1H@T") && s.contains("\x1b[1;1H@T"), "repaint not stamped");
}

/// W4 frames go out whole, each with its own stamp and no stamp lines, and
/// with -echo they leave the last row alone.
#[test]
fn gen_framed() {
    let out = SyncBuffer::default();
    let (tx, pr) = pipe();
    let mut w = out.clone();
    let done = std::thread::spawn(move || gen_with(&args(&["-kind", "w4", "-echo", "-rows", "24"]), pr, &mut w));
    tx.send(b"x".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    tx.send(b"\x03".to_vec()).unwrap();
    done.join().unwrap().unwrap();
    let s = out.string();
    assert!(!s.contains("@\r\n") && !s.contains(STAMP_PLACEHOLDER), "stamp line or unfilled placeholder in W4 output");
    let frames = s.matches("\x1b[?2026h").count();
    let stamps = stamp_indexes(&s).len();
    assert!(frames >= 2 * W4_SWITCH && stamps == frames, "{frames} frames, {stamps} stamps");
    assert_eq!(s.matches("\x1b[24;1H").count(), s.matches("\x1b7\x1b[24;1H").count(), "W4 drew on the echo row");
    assert_eq!(s.matches(FRAMED_PREFIX).count(), 1, "alternate screen entered more than once: it clears the echo row");
}

#[test]
fn clock() {
    let mut out = Vec::new();
    let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    clock_main(&b"a\nb\n"[..], &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = s.split_whitespace().collect();
    assert_eq!(lines.len(), 2, "clock replied {s:?}");
    let ns: u128 = lines[1].parse().unwrap();
    let after = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    assert!(ns >= before && ns <= after, "clock {ns} outside the call");
    assert!(clock_main(&b"x\n"[..], &mut FailWriter).is_err(), "write error ignored");
}

#[test]
fn resume_errors() {
    let dir = crate::tests::temp_dir();
    let key = format!("{dir}/k.pem");
    let sk = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let missing = format!("{dir}/missing");
    let a = args(&["-addr", "127.0.0.1:1", "-session", "s", "-key", &key, "-server-key", sk, "-want", &missing]);
    assert!(resume_main(&a, &mut io::sink()).is_err(), "missing -want file accepted");
    let a = args(&["-addr", "127.0.0.1:1", "-session", "s", "-key", &key, "-runs", "1", "-server-key", sk]);
    assert!(resume_main(&a, &mut io::sink()).is_err(), "dial to a closed port succeeded");
    let a = args(&["-addr", "127.0.0.1:1", "-session", "s", "-key", "/nonexistent/dir/k", "-server-key", sk]);
    assert!(resume_main(&a, &mut io::sink()).is_err(), "bad key path accepted");
    let mut tm = new_term(10, 3, 0);
    tm.write(b"ab  \r\ncd");
    assert_eq!(visible_text(&tm), "ab\ncd");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fresh private directory under the system temp dir.
pub(crate) fn temp_dir() -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("osewa-bench-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.to_string_lossy().into_owned()
}

// serve_test.go

/// Starts the mux on a free port, on its own runtime thread.
fn start_serve() -> String {
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap().to_string();
    ln.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _ = crate::serve::serve(tokio::net::TcpListener::from_std(ln).unwrap()).await;
        });
    });
    addr
}

struct Resp {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

/// One HTTP/1.1 request on its own connection. A chunked body goes out in
/// pieces, as Go's client sends a body of unknown length.
fn http(addr: &str, method: &str, path: &str, body: Option<(&[u8], bool)>) -> Resp {
    let mut c = TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n");
    match body {
        None => req += "\r\n",
        Some((b, false)) => req += &format!("Content-Length: {}\r\n\r\n", b.len()),
        Some((_, true)) => req += "Transfer-Encoding: chunked\r\n\r\n",
    }
    c.write_all(req.as_bytes()).unwrap();
    match body {
        Some((b, false)) => c.write_all(b).unwrap(),
        Some((b, true)) => {
            for chunk in b.chunks(7919) {
                c.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).unwrap();
                c.write_all(chunk).unwrap();
                c.write_all(b"\r\n").unwrap();
            }
            c.write_all(b"0\r\n\r\n").unwrap();
        }
        None => {}
    }
    let raw = read_all(&mut c);
    let i = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("no response head") + 4;
    let headers = String::from_utf8_lossy(&raw[..i]).into_owned();
    let status = headers[9..12].parse().unwrap();
    Resp { status, headers, body: raw[i..].to_vec() }
}

fn ws_frame(op: u8, p: &[u8]) -> Vec<u8> {
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut b = vec![0x80 | op];
    match p.len() {
        n if n < 126 => b.push(0x80 | n as u8),
        n if n <= 0xffff => {
            b.push(0x80 | 126);
            b.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            b.push(0x80 | 127);
            b.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    b.extend_from_slice(&mask);
    b.extend(p.iter().enumerate().map(|(i, c)| c ^ mask[i % 4]));
    b
}

fn read_ws(c: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut h = [0u8; 2];
    c.read_exact(&mut h).unwrap();
    assert_eq!(h[1] & 0x80, 0, "server frames are unmasked");
    let mut n = (h[1] & 0x7f) as u64;
    if n == 126 {
        let mut b = [0u8; 2];
        c.read_exact(&mut b).unwrap();
        n = u16::from_be_bytes(b) as u64;
    } else if n == 127 {
        let mut b = [0u8; 8];
        c.read_exact(&mut b).unwrap();
        n = u64::from_be_bytes(b);
    }
    let mut p = vec![0u8; n as usize];
    c.read_exact(&mut p).unwrap();
    (h[0] & 0x0f, p)
}

#[test]
fn serve_mux() {
    let addr = start_serve();
    let r = http(&addr, "GET", "/hello", None);
    assert!(r.status == 200 && r.body == b"hello from osewa-bench\n", "/hello = {} {:?}", r.status, r.body);
    let r = http(&addr, "GET", "/bytes?n=200000", None);
    assert!(r.headers.contains("Content-Length: 200000\r\n"), "headers {:?}", r.headers);
    check_pattern(&r.body[..], 200000).unwrap();
    assert_eq!(http(&addr, "GET", "/bytes?n=x", None).status, 400, "/bytes?n=x");

    let p = read_all(pattern(100000));
    for chunked in [false, true] {
        let r = http(&addr, "POST", "/check?n=100000", Some((&p, chunked)));
        assert!(r.status == 200 && r.body == b"ok\n", "/check (chunked {chunked}) = {} {:?}", r.status, String::from_utf8_lossy(&r.body));
        let r = http(&addr, "POST", "/check?n=100001", Some((&p, chunked)));
        assert!(r.status == 422 && String::from_utf8_lossy(&r.body).contains("short"), "/check short = {} {:?}", r.status, r.body);
    }
    assert_eq!(http(&addr, "POST", "/check?n=-1", Some((b"", false))).status, 400, "/check?n=-1");

    // Keep-alive: two requests on one connection.
    let mut c = TcpStream::connect(&addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"GET /hello HTTP/1.1\r\nHost: t\r\n\r\nGET /bytes?n=3 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").unwrap();
    let s = read_all(&mut c);
    assert_eq!(s.windows(15).filter(|w| w == b"HTTP/1.1 200 OK").count(), 2, "{:?}", String::from_utf8_lossy(&s));

    // The WebSocket echo, binary and text, up to a megabyte.
    let mut c = TcpStream::connect(&addr).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(b"GET /ws HTTP/1.1\r\nHost: t\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        c.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 101") && head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"), "{head}");
    for n in [1usize, 1000, 1 << 20] {
        let msg = vec![b'x'; n];
        c.write_all(&ws_frame(2, &msg)).unwrap();
        let (op, got) = read_ws(&mut c);
        assert!(op == 2 && got == msg, "ws echo of {n} bytes: op {op}, {} bytes", got.len());
    }
    c.write_all(&ws_frame(1, b"text")).unwrap();
    assert_eq!(read_ws(&mut c), (1, b"text".to_vec()));
    c.write_all(&ws_frame(8, &1000u16.to_be_bytes())).unwrap();
    assert_eq!(read_ws(&mut c), (8, 1000u16.to_be_bytes().to_vec()));
    // A plain GET on /ws is refused, and the handler returns.
    let code = http(&addr, "GET", "/ws", None).status;
    assert!(code != 101 && code != 200, "plain GET /ws = {code}");
}

#[test]
fn serve_main() {
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let a = addr.clone();
    std::thread::spawn(move || crate::serve::serve_main(&args(&["-listen", &a])));
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(&addr).is_err() {
        assert!(Instant::now() < deadline, "serve never listened");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(http(&addr, "GET", "/hello", None).status, 200);
    assert!(crate::serve::serve_main(&args(&["-listen", &addr])).is_err(), "serve on a taken address succeeded");
    assert!(crate::serve::serve_main(&args(&["-nope"])).is_err(), "serve with a bad flag succeeded");
}

#[test]
fn bytes() {
    let mut out = Vec::new();
    assert_eq!(run(&args(&["bytes", "-n", "5000"]), &mut out, &mut io::sink()), 0, "bytes failed");
    check_pattern(&out[..], 5000).unwrap();
    let mut ok = Vec::new();
    bytes_main(&args(&["-check", "-n", "5000"]), &out[..], &mut ok).unwrap();
    assert_eq!(ok, b"ok\n");
    assert!(bytes_main(&args(&["-check", "-n", "5001"]), &out[..], &mut io::sink()).is_err(), "bytes -check accepted a short stream");
    assert!(bytes_main(&args(&["-x"]), &b""[..], &mut io::sink()).is_err(), "bytes with a bad flag succeeded");
}
