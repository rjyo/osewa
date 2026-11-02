//! The client tests: a fake server for the protocol's server side, and
//! the client driven through its public API.

use std::future::Future;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use osewa_ghostty as ghostty;
use osewa_mirror::Mirror;
use osewa_wire as wire;

use crate::*;

mod fake;
use fake::*;

mod abandon;
mod catchup;
mod channel;
mod client;
mod dial;
mod flow;
mod link;
mod manual;
mod nomirror;
mod recover;
#[cfg(feature = "process")]
mod release;
mod robust;
mod trace;
mod visible;

/// The default wait.
const WAIT: Duration = Duration::from_secs(5);

/// Runs f to completion within WAIT, from a test thread.
fn run<F: Future>(f: F) -> F::Output {
    run_for(WAIT, f).expect("timed out")
}

/// Runs f for at most d: None if it didn't finish.
fn run_for<F: Future>(d: Duration, f: F) -> Option<F::Output> {
    block_on(rt::timeout(d, f))
}

/// Polls cond until it holds.
#[track_caller]
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn sleep(d: Duration) {
    std::thread::sleep(d)
}

/// Waits for an async signal (lost, done, suspect).
#[track_caller]
fn wait_for<F: Future>(what: &str, f: F) {
    assert!(run_for(WAIT, f).is_some(), "timed out waiting for {what}");
}

/// The mirror as this crate's Renderer: osewa-mirror implements the trait
/// of the non-test build of this crate, a different crate to the compiler.
struct Adapter(Mirror);

impl Renderer for Adapter {
    fn snapshot_begin(&mut self, b: &wire::SnapshotBegin) -> Result<(), RendererError> {
        Ok(self.0.snapshot_begin(b)?)
    }
    fn snapshot_data(&mut self, data: &[u8], history: bool) -> Result<(), RendererError> {
        Ok(self.0.snapshot_data(data, history)?)
    }
    fn snapshot_ready(&mut self) -> Result<(), RendererError> {
        Ok(self.0.snapshot_ready()?)
    }
    fn snapshot_end(&mut self) -> Result<(), RendererError> {
        Ok(self.0.snapshot_end()?)
    }
    fn output(&mut self, data: &[u8]) -> Result<(), RendererError> {
        Ok(self.0.output(data)?)
    }
    fn event(&mut self, e: &wire::EventMsg, live: bool) -> Result<(), RendererError> {
        Ok(self.0.event(e, live)?)
    }
    fn end(&mut self) {
        self.0.end()
    }
}

/// A session with a mirror, as most tests check what the stream rendered.
struct Mirrored {
    s: Session,
    m: Mirror,
}

impl Deref for Mirrored {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.s
    }
}

impl Drop for Mirrored {
    fn drop(&mut self) {
        self.s.close();
        self.m.close();
    }
}

impl Mirrored {
    #[track_caller]
    fn wait_text(&self, substr: &str) {
        if let Err(e) = run_for(WAIT, self.m.wait_text(substr)).unwrap_or(Err("timed out".into())) {
            panic!("waiting for {substr:?}: {e}\nscreen:\n{}", self.m.visible_text());
        }
    }

    fn try_wait_text(&self, d: Duration, substr: &str) -> Result<(), String> {
        run_for(d, self.m.wait_text(substr)).unwrap_or(Err("timed out".into()))
    }

    fn text(&self) -> String {
        self.m.text()
    }

    fn visible_text(&self) -> String {
        self.m.visible_text()
    }

    fn view<R>(&self, f: impl FnOnce(Option<&mut ghostty::Terminal>) -> R) -> R {
        self.m.view(f)
    }

    fn size(&self) -> (usize, usize) {
        self.view(|t| t.map(|t| t.size()).unwrap_or_default())
    }

    fn scrollback(&self) -> usize {
        self.view(|t| t.map(|t| t.scrollback_rows()).unwrap_or(0))
    }

    fn ready(&self) -> bool {
        self.view(|t| t.is_some())
    }
}

/// Attaches with a mirror built from mo (default size 40x10).
#[track_caller]
fn attach_with(c: &Conn, id: &str, mut opt: AttachOptions, mo: osewa_mirror::Options) -> Mirrored {
    if opt.cols == 0 {
        (opt.cols, opt.rows) = (40, 10);
    }
    let m = Mirror::new(mo);
    opt.renderer = Some(Box::new(Adapter(m.clone())));
    match run(c.attach(id, opt)) {
        Ok(s) => Mirrored { s, m },
        Err(e) => {
            m.close();
            panic!("attach: {e}");
        }
    }
}

#[track_caller]
fn attach(c: &Conn, id: &str, opt: AttachOptions) -> Mirrored {
    attach_with(c, id, opt, Default::default())
}

/// Attaches without a renderer, as an app with its own does.
#[track_caller]
fn attach_raw(c: &Conn, id: &str, opt: AttachOptions) -> Session {
    run(c.attach(id, opt)).unwrap()
}

/// Attaches and waits until input flows (the base INPUT_ACK is in).
#[track_caller]
fn ready(c: &Conn, id: &str, opt: AttachOptions) -> Mirrored {
    let s = attach(c, id, opt);
    s.send_text("").unwrap();
    run(s.flush()).unwrap();
    s
}

fn opts(cols: i64, rows: i64) -> AttachOptions {
    AttachOptions { cols, rows, ..Default::default() }
}

fn own() -> AttachOptions {
    AttachOptions { own: true, ..Default::default() }
}

/// A shared counter or log a callback can write from the applier thread.
fn shared<T: Default>() -> Arc<Mutex<T>> {
    Arc::new(Mutex::new(T::default()))
}

/// build_info is what `osewa version --json` and the FFI print, and what
/// moshi-hook compares: its keys must not drift.
#[test]
fn build_info_shape() {
    let v: serde_json::Value = serde_json::from_str(&build_info()).unwrap();
    assert_eq!(v["version"], wire::VERSION);
    assert_eq!(v["ghostty"], GHOSTTY_COMMIT);
    assert_eq!(v["snapshot_version"], SNAPSHOT_VERSION);
    assert!(v["osewa"].as_str().is_some_and(|s| !s.is_empty()), "{v}");
    assert_eq!(v.as_object().unwrap().len(), 4, "{v}");
}

/// The marker release.sh greps for is the build id `version --json` gives.
#[test]
fn build_id_marker_matches_build_info() {
    let v: serde_json::Value = serde_json::from_str(&build_info()).unwrap();
    let id = format!("p{}-s{}", v["version"], v["snapshot_version"]);
    let marker = std::str::from_utf8(&BUILD_ID_MARKER).unwrap().trim_end_matches('\0');
    assert_eq!(marker, format!("osewa-build-id:{id}"));
}

#[test]
fn ghostty_commit_matches() {
    assert_eq!(crate::GHOSTTY_COMMIT, ghostty::COMMIT);
    assert_eq!(crate::SNAPSHOT_VERSION, ghostty::SNAPSHOT_VERSION);
}
