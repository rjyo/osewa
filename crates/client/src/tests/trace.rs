//! OSEWA_TRACE events.

use serde::Deserialize;

use super::*;
use crate::hosts::testutil::TempDir;

#[derive(Deserialize, Debug, Clone, Default)]
pub(super) struct Traced {
    #[serde(default)]
    pub t: i64,
    #[serde(default)]
    pub ev: String,
    pub stream: Option<u32>,
    #[serde(rename = "type")]
    pub ty: Option<u8>,
    pub len: Option<usize>,
    pub generation: Option<u32>,
    pub history: Option<bool>,
    pub end: Option<u64>,
    pub seq: Option<u64>,
}

pub(super) fn read_trace(dir: &std::path::Path) -> Vec<Traced> {
    let b = std::fs::read_to_string(dir.join(format!("client-{}.jsonl", std::process::id()))).unwrap();
    b.lines()
        .map(|l| {
            let e: Traced = serde_json::from_str(l).unwrap_or_else(|_| panic!("not a JSON line: {l:?}"));
            assert!(e.t != 0 && !e.ev.is_empty(), "line without t or ev: {l:?}");
            e
        })
        .collect()
}

pub(super) fn find(evs: &[Traced], ev: &str, m: impl Fn(&Traced) -> bool) -> Option<Traced> {
    evs.iter().find(|e| e.ev == ev && m(e)).cloned()
}

fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64
}

#[test]
fn trace_events() {
    let dir = TempDir::new();
    let f = Fake::new();
    let id = f.create(40, 10);
    f.output(&id, "before\r\n");
    let mut cfg = f.config("a");
    cfg.knobs.trace_dir = Some(dir.path().into());
    let c = run(Conn::dial(cfg)).unwrap();
    run(c.welcome()).unwrap();
    let start = now_ns();
    let s = attach(&c, &id, own());
    s.wait_text("before");
    s.send_text("x").unwrap();
    run(s.flush()).unwrap();
    f.output(&id, "after");
    s.wait_text("after");
    f.screen_snapshot(&id);
    eventually("screen ready", || find(&read_trace(dir.path()), "ready", |e| e.generation == Some(1)).is_some());

    let evs = read_trace(dir.path());
    let stream = |e: &Traced| e.stream == Some(1);
    let is = |ty: u8| move |e: &Traced| e.stream == Some(1) && e.ty == Some(ty);
    let begin = find(&evs, "frame_recv", |e| is(wire::TYPE_SNAPSHOT_BEGIN)(e) && e.generation == Some(0));
    assert!(
        begin.as_ref().is_some_and(|b| b.history == Some(true) && b.len.is_some_and(|l| l > 0) && b.t >= start),
        "SNAPSHOT_BEGIN generation 0: {begin:?}"
    );
    let screen = find(&evs, "frame_recv", |e| is(wire::TYPE_SNAPSHOT_BEGIN)(e) && e.generation == Some(1));
    assert!(screen.as_ref().is_some_and(|b| b.history == Some(false)), "catch-up screen BEGIN: {screen:?}");
    let end = f.with(&id, |fs| fs.next);
    assert!(find(&evs, "frame_recv", |e| is(wire::TYPE_OUTPUT)(e) && e.end == Some(end)).is_some(), "no frame_recv for OUTPUT ending at {end}");
    assert!(find(&evs, "ready", |e| stream(e) && e.generation == Some(0)).is_some(), "no ready for generation 0");
    let sent = find(&evs, "input_sent", |e| stream(e) && e.seq.is_some_and(|s| s >= 1)).expect("no input_sent");
    let ack = find(&evs, "input_ack", |e| stream(e) && e.seq >= sent.seq);
    assert!(ack.as_ref().is_some_and(|a| a.t >= sent.t), "input_ack {ack:?} after input_sent {sent:?}");
}

/// Unset, there is no tracer and no file: tracing costs nothing.
#[test]
fn trace_off_by_default() {
    if std::env::var_os("OSEWA_TRACE").is_some_and(|v| !v.is_empty()) {
        eprintln!("skipping: OSEWA_TRACE is set");
        return;
    }
    let f = Fake::new();
    let id = f.create(40, 10);
    let c = f.dial("a");
    assert!(c.0.tr.is_none(), "tracer without OSEWA_TRACE");
    let s = attach(&c, &id, Default::default());
    f.output(&id, "x");
    s.wait_text("x");
}
