//! `osewa-bench vs`: osewa against ssh and mosh on a real host, the
//! numbers an interactive user feels on that path: connecting and running a
//! command from cold, a bulk download alone, a keystroke's echo idle, and
//! the echo while a download shares the link.
//!
//! ```text
//! osewa-bench vs [-runs N] [-keys N] [-quic] [-mosh] <host>
//! ```
//!
//! The host is an osewa host name (hosts.json) whose `ssh` target is also
//! how ssh and mosh reach it. osewa's download runs on the same connection
//! as its terminal, and ssh's on the same ControlMaster, as a user's
//! terminal and their forwards or scp share one link. mosh can carry only a
//! terminal, so its download runs over a separate ssh connection on the same
//! path, as a mosh user's would.
//!
//! Echo is timed without prediction, so it is the round trip a key really
//! takes: osewa and ssh from the bytes coming back (cat echoes only the
//! letter), mosh from a libghostty rendering of its screen, since mosh
//! redraws with escape sequences.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use osewa_client::{block_on, hosts, AttachOptions, Conn, CreateOptions, ExecOptions};

use crate::drive::Display;
use crate::Error;

/// What an echo that never came counts as.
const MISSED: Duration = Duration::from_secs(15);

/// How long a terminal gets to come up before its first key.
const SETTLE: Duration = Duration::from_secs(1);

struct Opts {
    keys: usize,
    gap: Duration,
    warmup: Duration,
    measure: Duration,
    connects: usize,
}

/// One protocol's numbers from one run. None where it can't be measured.
#[derive(Default, Clone)]
struct Side {
    connect: Vec<Duration>,
    /// Cold connects that failed and were tried again: a public host's
    /// path now and then drops a fresh TCP flow's every SYN.
    connect_failed: usize,
    bulk: Option<f64>,
    idle_echo: Vec<Duration>,
    load_echo: Vec<Duration>,
    load_bulk: Option<f64>,
}

/// A download running into a byte counter until stopped.
struct Stream {
    stop: Box<dyn FnOnce()>,
}

/// A terminal running cat, typed into one letter at a time.
trait Echo {
    /// Sends key and waits until it is seen, or MISSED.
    fn key(&mut self, k: char) -> Duration;
    fn close(self: Box<Self>);
}

trait Proto {
    fn name(&self) -> &'static str;
    /// None: this protocol carries no bulk data of its own.
    fn stream(&self, got: Arc<AtomicU64>) -> Result<Stream, Error>;
    fn terminal(&self) -> Result<Box<dyn Echo>, Error>;
    fn connect(&self) -> Result<Duration, Error>;
    /// Whether stream's numbers are this protocol's own (false for mosh,
    /// whose download goes over ssh).
    fn owns_bulk(&self) -> bool {
        true
    }
}

pub fn vs_main(args: &[String], w: &mut dyn Write) -> Result<(), Error> {
    let mut fs = osewa_flag::FlagSet::new("vs");
    fs.value("runs", "1", "rounds; each measures every protocol once, in turn")
        .value("keys", "40", "keystrokes per echo run")
        .value("gap", "150ms", "pause between keystrokes")
        .value("warmup", "3s", "of a download before measuring it, past slow start")
        .value("measure", "15s", "of a download alone")
        .value("connects", "5", "cold connects per protocol")
        .bool("quic", true, "measure osewa over QUIC too")
        .bool("mosh", true, "measure mosh too")
        .value("mosh-server", "", "mosh-server path on the host, if not on its PATH");
    let p = crate::parse(&fs, args)?;
    if p.args.len() != 1 {
        return Err(Error("vs: usage: osewa-bench vs [flags] <host>".into()));
    }
    let o = Opts {
        keys: crate::int(&p, "keys")?.max(1) as usize,
        gap: crate::duration(&p, "gap")?,
        warmup: crate::duration(&p, "warmup")?,
        measure: crate::duration(&p, "measure")?,
        connects: crate::int(&p, "connects")?.max(0) as usize,
    };
    let runs = crate::int(&p, "runs")?.max(1) as usize;
    let dir = hosts::default_dir();
    let h = hosts::find(&dir, &p.args[0]).map_err(|e| Error(format!("vs: {e}")))?;
    let target = if h.ssh.is_empty() { h.name.clone() } else { h.ssh.clone() };
    let id = format!("osewa-bench-{}", std::process::id());
    let config = |carrier: &str| -> Result<osewa_client::Config, Error> {
        let mut c = h.config(&dir, &format!("{id}-{carrier}"), "osewa-bench").map_err(|e| Error(format!("vs: {e}")))?;
        c.carrier = carrier.into();
        Ok(c)
    };

    let ssh = Ssh::start(&target)?;
    let mut protos: Vec<Box<dyn Proto>> = Vec::new();
    if p.bool("quic") {
        protos.push(Box::new(Osewa::start("osewa QUIC", config("quic")?)?));
    }
    protos.push(Box::new(Osewa::start("osewa WebSocket", config("ws")?)?));
    protos.push(Box::new(ssh.clone()));
    if p.bool("mosh") {
        protos.push(Box::new(Mosh { target: target.clone(), server: p.str("mosh-server") }));
    }

    let mut all: Vec<Vec<Side>> = vec![Vec::new(); protos.len()];
    for run in 0..runs {
        for (i, pr) in protos.iter().enumerate() {
            eprintln!("vs: run {}/{runs}: {}", run + 1, pr.name());
            all[i].push(measure(&o, pr.as_ref()).map_err(|e| Error(format!("vs: {}: {e}", pr.name())))?);
        }
    }
    let names: Vec<&str> = protos.iter().map(|p| p.name()).collect();
    let out = table(&names, &all);
    // Closes the osewa connections before the ssh master goes.
    drop(protos);
    ssh.stop();
    w.write_all(out.as_bytes()).map_err(Error::io)
}

fn measure(o: &Opts, p: &dyn Proto) -> Result<Side, Error> {
    let mut s = Side::default();
    for _ in 0..o.connects {
        let mut tries = 0;
        loop {
            match p.connect() {
                Ok(d) => break s.connect.push(d),
                Err(e) if tries < CONNECT_RETRIES => {
                    eprintln!("vs: {}: connect failed, trying again: {e}", p.name());
                    tries += 1;
                    s.connect_failed += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
    if p.owns_bulk() {
        let got = Arc::new(AtomicU64::new(0));
        let st = p.stream(got.clone())?;
        s.bulk = Some(rate_over(&got, o.warmup, o.measure));
        eprintln!("vs: {}: download alone {:.2} MB/s", p.name(), s.bulk.unwrap());
        (st.stop)();
    }
    let mut term = p.terminal()?;
    s.idle_echo = type_keys(term.as_mut(), o);
    let got = Arc::new(AtomicU64::new(0));
    let st = p.stream(got.clone())?;
    thread::sleep(o.warmup);
    let (before, t0) = (got.load(Ordering::Relaxed), Instant::now());
    s.load_echo = type_keys(term.as_mut(), o);
    s.load_bulk = Some((got.load(Ordering::Relaxed) - before) as f64 / t0.elapsed().as_secs_f64() / 1e6);
    eprintln!("vs: {}: download during the echo {:.2} MB/s", p.name(), s.load_bulk.unwrap());
    (st.stop)();
    term.close();
    Ok(s)
}

/// Retries of a cold connect before a run gives up.
const CONNECT_RETRIES: usize = 2;

/// How long a fresh ssh connection may take to answer: a flow whose SYNs
/// are all dropped otherwise waits out the system's whole retry schedule.
const SSH_CONNECT_TIMEOUT: &str = "ConnectTimeout=10";

fn rate_over(got: &AtomicU64, warmup: Duration, d: Duration) -> f64 {
    thread::sleep(warmup);
    let (before, t0) = (got.load(Ordering::Relaxed), Instant::now());
    thread::sleep(d);
    (got.load(Ordering::Relaxed) - before) as f64 / t0.elapsed().as_secs_f64() / 1e6
}

fn type_keys(e: &mut dyn Echo, o: &Opts) -> Vec<Duration> {
    let mut out = Vec::with_capacity(o.keys);
    for i in 0..o.keys {
        out.push(e.key((b'a' + (i % 26) as u8) as char));
        thread::sleep(o.gap);
    }
    out
}

/// Reads an endless download from r into got on a thread of its own.
fn count_into(mut r: impl Read + Send + 'static, got: Arc<AtomicU64>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut b = vec![0u8; 64 << 10];
        while let Ok(n) = r.read(&mut b) {
            if n == 0 {
                return;
            }
            got.fetch_add(n as u64, Ordering::Relaxed);
        }
    })
}

/// Waits on a channel of output chunks until one holds k.
fn wait_echo(seen: &mpsc::Receiver<Vec<u8>>, k: char, t: Instant) -> Duration {
    let deadline = t + MISSED;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return MISSED;
        }
        match seen.recv_timeout(deadline - now) {
            Ok(b) if b.contains(&(k as u8)) => return t.elapsed(),
            Ok(_) => {}
            Err(_) => return MISSED,
        }
    }
}

fn drain(seen: &mpsc::Receiver<Vec<u8>>) {
    while seen.try_recv().is_ok() {}
}

struct Osewa {
    name: &'static str,
    cfg: osewa_client::Config,
    c: Conn,
}

impl Osewa {
    fn start(name: &'static str, cfg: osewa_client::Config) -> Result<Osewa, Error> {
        let c = dial(cfg.clone())?;
        Ok(Osewa { name, cfg, c })
    }
}

fn dial(cfg: osewa_client::Config) -> Result<Conn, Error> {
    block_on(async {
        let c = Conn::dial(cfg).await?;
        c.welcome().await?;
        Ok::<_, osewa_client::Error>(c)
    })
    .map_err(|e| Error(e.to_string()))
}

impl Drop for Osewa {
    fn drop(&mut self) {
        block_on(self.c.close());
    }
}

impl Proto for Osewa {
    fn name(&self) -> &'static str {
        self.name
    }

    fn connect(&self) -> Result<Duration, Error> {
        let t0 = Instant::now();
        let c = dial(self.cfg.clone())?;
        let r = block_on(c.output(ExecOptions { argv: vec!["true".into()], ..Default::default() }));
        let d = t0.elapsed();
        block_on(c.close());
        r.map_err(|e| Error(e.to_string()))?;
        Ok(d)
    }

    fn stream(&self, got: Arc<AtomicU64>) -> Result<Stream, Error> {
        let e = block_on(self.c.exec(ExecOptions { argv: vec!["cat".into(), "/dev/urandom".into()], ..Default::default() }))
            .map_err(|e| Error(e.to_string()))?;
        let out = e.stdout();
        let t = thread::spawn(move || {
            let mut b = vec![0u8; 64 << 10];
            while let Ok(n) = block_on(out.read(&mut b)) {
                if n == 0 {
                    return;
                }
                got.fetch_add(n as u64, Ordering::Relaxed);
            }
        });
        Ok(Stream {
            stop: Box::new(move || {
                let _ = block_on(e.close());
                let _ = t.join();
            }),
        })
    }

    fn terminal(&self) -> Result<Box<dyn Echo>, Error> {
        let err = |e: osewa_client::Error| Error(e.to_string());
        let id = block_on(self.c.create(CreateOptions { cols: 80, rows: 24, argv: vec!["cat".into()], ..Default::default() })).map_err(err)?;
        let (tx, seen) = mpsc::channel();
        let opt = AttachOptions {
            cols: 80,
            rows: 24,
            own: true,
            on_output: Some(Box::new(move |b: &[u8]| {
                let _ = tx.send(b.to_vec());
            })),
            ..Default::default()
        };
        let sess = block_on(self.c.attach(&id, opt)).map_err(err)?;
        thread::sleep(SETTLE);
        drain(&seen);
        Ok(Box::new(OsewaEcho { c: self.c.clone(), id, sess, seen }))
    }
}

struct OsewaEcho {
    c: Conn,
    id: String,
    sess: osewa_client::Session,
    seen: mpsc::Receiver<Vec<u8>>,
}

impl Echo for OsewaEcho {
    fn key(&mut self, k: char) -> Duration {
        drain(&self.seen);
        let t = Instant::now();
        if self.sess.send_text(&k.to_string()).is_err() {
            return MISSED;
        }
        wait_echo(&self.seen, k, t)
    }

    fn close(self: Box<Self>) {
        self.sess.close();
        // The holder would keep an exited session for its retention time.
        let _ = block_on(self.c.kill(&self.id));
    }
}

/// ssh over one ControlMaster, as a user's terminal and transfers share a
/// connection.
#[derive(Clone)]
struct Ssh {
    target: String,
    sock: String,
    stopped: Arc<AtomicBool>,
}

impl Ssh {
    fn start(target: &str) -> Result<Ssh, Error> {
        // Unix socket paths are short; a scratch directory's may not fit.
        let sock = format!("/tmp/osewa-bench-{}.sock", std::process::id());
        let st = Command::new("ssh")
            .args(["-MNf", "-o", &format!("ControlPath={sock}"), "-o", SSH_CONNECT_TIMEOUT, "--", target])
            .status()
            .map_err(Error::io)?;
        if !st.success() {
            return Err(Error(format!("vs: ssh -MNf {target}: {st}")));
        }
        Ok(Ssh { target: target.into(), sock, stopped: Arc::new(AtomicBool::new(false)) })
    }

    fn cmd(&self, tty: bool, command: &str) -> Command {
        let mut c = Command::new("ssh");
        c.args(["-o", &format!("ControlPath={}", self.sock), "-o", "ControlMaster=no"]);
        if tty {
            c.arg("-tt");
        }
        c.args(["--", &self.target, command]);
        c
    }

    fn stop(&self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let _ = Command::new("ssh").args(["-O", "exit", "-o", &format!("ControlPath={}", self.sock), "--", &self.target]).stderr(Stdio::null()).status();
        }
    }
}

fn ssh_stream(mut c: Command, got: Arc<AtomicU64>) -> Result<Stream, Error> {
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(Error::io)?;
    let t = count_into(child.stdout.take().unwrap(), got);
    Ok(Stream {
        stop: Box::new(move || {
            let _ = child.kill();
            let _ = child.wait();
            let _ = t.join();
        }),
    })
}

impl Proto for Ssh {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn connect(&self) -> Result<Duration, Error> {
        let t0 = Instant::now();
        let out = Command::new("ssh")
            .args(["-o", "ControlPath=none", "-o", SSH_CONNECT_TIMEOUT, "--", &self.target, "true"])
            .output()
            .map_err(Error::io)?;
        if !out.status.success() {
            return Err(Error(format!("ssh connect: {}: {}", out.status, String::from_utf8_lossy(&out.stderr))));
        }
        Ok(t0.elapsed())
    }

    fn stream(&self, got: Arc<AtomicU64>) -> Result<Stream, Error> {
        ssh_stream(self.cmd(false, "cat /dev/urandom"), got)
    }

    fn terminal(&self) -> Result<Box<dyn Echo>, Error> {
        let mut child = self.cmd(true, "cat").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(Error::io)?;
        let (tx, seen) = mpsc::channel();
        let mut out = child.stdout.take().unwrap();
        let t = thread::spawn(move || {
            let mut b = vec![0u8; 4096];
            while let Ok(n) = out.read(&mut b) {
                if n == 0 || tx.send(b[..n].to_vec()).is_err() {
                    return;
                }
            }
        });
        thread::sleep(SETTLE);
        drain(&seen);
        Ok(Box::new(SshEcho { child, seen, t: Some(t) }))
    }
}

struct SshEcho {
    child: Child,
    seen: mpsc::Receiver<Vec<u8>>,
    t: Option<JoinHandle<()>>,
}

impl Echo for SshEcho {
    fn key(&mut self, k: char) -> Duration {
        drain(&self.seen);
        let t = Instant::now();
        let ok = self.child.stdin.as_mut().is_some_and(|w| w.write_all(&[k as u8]).and_then(|_| w.flush()).is_ok());
        if !ok {
            return MISSED;
        }
        wait_echo(&self.seen, k, t)
    }

    fn close(mut self: Box<Self>) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = self.t.take() {
            let _ = t.join();
        }
    }
}

/// mosh, its screen rendered with libghostty as a terminal would.
struct Mosh {
    target: String,
    server: String,
}

impl Mosh {
    fn argv(&self, command: &str) -> Vec<String> {
        // mosh-server refuses to start without a UTF-8 locale it knows,
        // and C.UTF-8 (the display's default) doesn't exist on macOS.
        let mut a: Vec<String> = ["env", "LANG=en_US.UTF-8", "LC_ALL=en_US.UTF-8", "mosh", "--predict=never"].map(String::from).into();
        if !self.server.is_empty() {
            a.push(format!("--server={}", self.server));
        }
        a.extend([self.target.clone(), "--".into(), command.into()]);
        a
    }
}

impl Proto for Mosh {
    fn name(&self) -> &'static str {
        "mosh"
    }

    fn owns_bulk(&self) -> bool {
        false
    }

    fn connect(&self) -> Result<Duration, Error> {
        let t0 = Instant::now();
        let d = Display::start(&self.argv("true"), 80, 24).map_err(Error::io)?;
        // Done when mosh exits after the command: the display's PTY closes.
        let exited = !d.wait_for(Duration::from_secs(30), |_, _| false);
        let el = t0.elapsed();
        d.close();
        if !exited || el >= Duration::from_secs(30) {
            return Err(Error("mosh connect: didn't finish within 30 s".into()));
        }
        Ok(el)
    }

    fn stream(&self, got: Arc<AtomicU64>) -> Result<Stream, Error> {
        let mut c = Command::new("ssh");
        c.args(["-o", "ControlPath=none", "--", &self.target, "cat /dev/urandom"]);
        ssh_stream(c, got)
    }

    fn terminal(&self) -> Result<Box<dyn Echo>, Error> {
        let d = Display::start(&self.argv("cat"), 80, 24).map_err(Error::io)?;
        // mosh clears the screen once connected; cat prints nothing.
        thread::sleep(Duration::from_secs(3));
        Ok(Box::new(MoshEcho { d: Some(d), line: String::new() }))
    }
}

struct MoshEcho {
    d: Option<Display>,
    line: String,
}

/// Letters per line before Enter, so the line never wraps.
const MOSH_LINE: usize = 30;

impl Echo for MoshEcho {
    fn key(&mut self, k: char) -> Duration {
        let d = self.d.as_ref().unwrap();
        if self.line.len() >= MOSH_LINE {
            d.send("\r");
            self.line.clear();
            d.wait_for(MISSED, |rs, cy| rs.get(cy).is_some_and(|l| l.is_empty()));
        }
        self.line.push(k);
        let t = Instant::now();
        d.send(&k.to_string());
        let want = self.line.clone();
        if d.wait_for(MISSED, |rs, cy| rs.get(cy).is_some_and(|l| l.ends_with(&want))) {
            t.elapsed()
        } else {
            MISSED
        }
    }

    fn close(mut self: Box<Self>) {
        if let Some(d) = self.d.take() {
            // Killing mosh-client would leave mosh-server on the host,
            // waiting for it to come back. An EOF ends cat, and mosh with
            // it.
            d.send("\r\x04");
            d.wait_for(Duration::from_secs(10), |_, _| false);
            d.close();
        }
    }
}

/// The p-th (0..1) of d.
fn percentile(d: &[Duration], p: f64) -> Option<Duration> {
    if d.is_empty() {
        return None;
    }
    let mut s = d.to_vec();
    s.sort();
    Some(s[(p * (s.len() - 1) as f64 + 0.5) as usize])
}

fn median_f(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(v[v.len() / 2])
}

fn ms(d: Option<Duration>) -> String {
    match d {
        None => "—".into(),
        Some(d) if d >= MISSED => format!(">{}", MISSED.as_millis()),
        Some(d) => d.as_millis().to_string(),
    }
}

/// The median over runs of each run's statistic.
fn over_runs(runs: &[Side], f: impl Fn(&Side) -> Option<Duration>) -> Option<Duration> {
    let mut v: Vec<Duration> = runs.iter().filter_map(f).collect();
    if v.is_empty() {
        return None;
    }
    v.sort();
    Some(v[v.len() / 2])
}

fn rate(runs: &[Side], f: impl Fn(&Side) -> Option<f64>) -> String {
    let mut v: Vec<f64> = runs.iter().filter_map(f).collect();
    median_f(&mut v).map_or("—".into(), |r| format!("{r:.2}"))
}

/// A Markdown table, a column per protocol, each cell the median over runs
/// of that run's statistic; then each run's echo p50s, to show the spread.
fn table(names: &[&str], all: &[Vec<Side>]) -> String {
    let runs = all.first().map_or(0, Vec::len);
    let mut b = format!("| median of {runs} run(s) | {} |\n|---|{}\n", names.join(" | "), "---:|".repeat(names.len()));
    type Row<'a> = (&'a str, Box<dyn Fn(&[Side]) -> String>);
    let rows: Vec<Row> = vec![
        ("connect + run p50 ms", Box::new(|r| ms(over_runs(r, |s| percentile(&s.connect, 0.5))))),
        ("echo idle p50 / p95 ms", Box::new(|r| format!("{} / {}", ms(over_runs(r, |s| percentile(&s.idle_echo, 0.5))), ms(over_runs(r, |s| percentile(&s.idle_echo, 0.95)))))),
        (
            "echo during a download p50 / p95 ms",
            Box::new(|r| format!("{} / {}", ms(over_runs(r, |s| percentile(&s.load_echo, 0.5))), ms(over_runs(r, |s| percentile(&s.load_echo, 0.95))))),
        ),
        ("download alone MB/s", Box::new(|r| rate(r, |s| s.bulk))),
        ("download during the echo MB/s", Box::new(|r| rate(r, |s| s.load_bulk))),
    ];
    for (name, f) in &rows {
        b += &format!("| {name} |");
        for runs in all {
            b += &format!(" {} |", f(runs));
        }
        b += "\n";
    }
    let failed: Vec<String> = names
        .iter()
        .zip(all)
        .filter_map(|(n, runs)| {
            let k: usize = runs.iter().map(|s| s.connect_failed).sum();
            (k > 0).then(|| format!("{n} {k}"))
        })
        .collect();
    if !failed.is_empty() {
        b += &format!("\ncold connects that failed and were tried again (not in the timings): {}\n", failed.join(", "));
    }
    b += "\nper run, echo during a download p50 ms:\n";
    for (n, runs) in names.iter().zip(all) {
        let v: Vec<String> = runs.iter().map(|s| ms(percentile(&s.load_echo, 0.5))).collect();
        b += &format!("- {n}: {}\n", v.join(" "));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn percentiles_and_medians() {
        let v = [d(5), d(1), d(3), d(2), d(4)];
        assert_eq!(percentile(&v, 0.5), Some(d(3)));
        assert_eq!(percentile(&v, 0.95), Some(d(5)));
        assert_eq!(percentile(&[], 0.5), None);
        let runs = [Side { idle_echo: vec![d(10)], ..Default::default() }, Side { idle_echo: vec![d(30)], ..Default::default() }, Side { idle_echo: vec![d(20)], ..Default::default() }];
        assert_eq!(over_runs(&runs, |s| percentile(&s.idle_echo, 0.5)), Some(d(20)));
    }

    // A protocol with no bulk of its own shows a dash, a missed echo shows
    // as past the limit, not as a time, and retried connects are counted.
    #[test]
    fn table_cells() {
        let s = Side { connect: vec![d(400)], connect_failed: 0, bulk: None, idle_echo: vec![d(120)], load_echo: vec![MISSED], load_bulk: Some(1.5) };
        let t = table(&["x"], &[vec![s.clone()]]);
        assert!(t.contains("| connect + run p50 ms | 400 |"), "{t}");
        assert!(t.contains("| download alone MB/s | — |"), "{t}");
        assert!(t.contains(">15000 / >15000"), "{t}");
        assert!(t.contains("| download during the echo MB/s | 1.50 |"), "{t}");
        assert!(!t.contains("tried again"), "{t}");
        let t = table(&["x", "y"], &[vec![s.clone()], vec![Side { connect_failed: 2, ..s }]]);
        assert!(t.contains("tried again (not in the timings): y 2\n"), "{t}");
    }

    // The ssh side against a local cat: each key's echo is the letter
    // coming back.
    #[test]
    fn echo_waits_for_the_letter() {
        let (tx, rx) = mpsc::channel();
        tx.send(b"\x1b[0m".to_vec()).unwrap();
        tx.send(b"q".to_vec()).unwrap();
        assert!(wait_echo(&rx, 'q', Instant::now()) < MISSED);
        drop(tx);
        assert_eq!(wait_echo(&rx, 'z', Instant::now()), MISSED);
    }
}
