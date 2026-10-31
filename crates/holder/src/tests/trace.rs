//! OSEWA_TRACE events. Tracing off costs nothing to check: an untraced
//! holder holds no tracer at all (PtyInput::push_input checks for one
//! first).

use std::path::Path;
use std::time::Duration;

use osewa_wire as wire;
use serde_json::Value;

use super::helpers::*;
use super::main::{holder_command, Proc};
use crate::{socket_path, Config};

type TraceEvent = serde_json::Map<String, Value>;

/// An event name, fields it must have with their values, and fields that
/// must be positive numbers.
type Check<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);

fn read_trace(path: &Path) -> Vec<TraceEvent> {
    let b = std::fs::read_to_string(path).unwrap_or_default();
    let mut evs = Vec::new();
    for line in b.lines() {
        let ev: TraceEvent = serde_json::from_str(line).unwrap_or_else(|e| panic!("line {line:?}: {e}"));
        let t = ev.get("t").and_then(Value::as_i64);
        let name = ev.get("ev").and_then(Value::as_str).unwrap_or("");
        assert!(t.is_some() && !name.is_empty(), "line without t or ev: {line:?}");
        evs.push(ev);
    }
    evs
}

/// A decoded JSON value as text: strings bare, absent as <nil>, the rest
/// as JSON.
fn sprint(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "<nil>".into(),
    }
}

/// The first event named ev whose fields include want.
fn find<'a>(evs: &'a [TraceEvent], ev: &str, want: &[(&str, &str)]) -> Option<&'a TraceEvent> {
    evs.iter().find(|e| sprint(e.get("ev")) == ev && want.iter().all(|(k, v)| sprint(e.get(*k)) == *v))
}

#[test]
fn trace_events() {
    let dir = TempDir::new();
    let h = start(Config {
        trace_dir: dir.0.to_str().unwrap().into(),
        argv: sh(r#"stty -echo; echo READY; read l; echo "L:$l"; read go; seq 1 400000; sleep 30"#),
        ..Default::default()
    });
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
    c.wait_text("READY");
    c.text_input(1, "x\r");
    c.wait_text("L:x");
    // Stop crediting and flood: the stream falls behind.
    c.no_credit = true;
    c.text_input(2, "\r");
    c.wait_for("catch-up", |c| c.gens.len() > 1);

    let path = dir.0.join(format!("holder-{}.jsonl", std::process::id()));
    let cut = ["position", "encode_ns", "bytes"];
    let checks: [Check; 8] = [
        ("input_read", &[("client_id", "dev1"), ("stream", "7"), ("seq", "1")], &[]),
        ("input_applied", &[("client_id", "dev1"), ("seq", "1"), ("bytes", "2")], &[]),
        ("screen_cut", &[("stream", "7"), ("generation", "0"), ("reason", "attach"), ("after_seq", "0")], &cut),
        ("begin_written", &[("stream", "7"), ("generation", "0")], &[]),
        ("ready_written", &[("stream", "7"), ("generation", "0")], &[]),
        ("catchup", &[("stream", "7"), ("on", "true")], &[]),
        ("screen_cut", &[("stream", "7"), ("generation", "1"), ("reason", "behind"), ("after_seq", "2")], &cut),
        ("begin_written", &[("stream", "7"), ("generation", "1")], &[]),
    ];
    let mut evs = Vec::new();
    let all = eventually(WAIT, || {
        evs = read_trace(&path);
        checks.iter().all(|(ev, want, _)| find(&evs, ev, want).is_some())
    });
    if !all {
        let missing: Vec<_> = checks.iter().filter(|(ev, want, _)| find(&evs, ev, want).is_none()).collect();
        panic!("missing: {missing:?}");
    }
    for (ev, want, fields) in checks {
        let e = find(&evs, ev, want).unwrap();
        for f in fields {
            let n = e.get(*f).and_then(Value::as_i64);
            assert!(n.is_some_and(|n| n > 0), "{ev} {f} = {:?}", e.get(*f));
        }
    }
}

/// Through main: OSEWA_TRACE names the directory, holder-<pid>.jsonl the
/// file; unset, nothing is written.
#[test]
fn trace_env() {
    for on in [true, false] {
        let (dir, trace_dir) = (TempDir::new(), TempDir::new());
        let d = dir.0.to_str().unwrap();
        let mut cmd = holder_command(&["--run", d, "--id", "tr", "--", "/bin/sh", "-c", "echo hi; sleep 30"]);
        cmd.env("OSEWA_TRACE", if on { trace_dir.0.to_str().unwrap() } else { "" });
        let mut p = Proc::spawn(cmd);
        let line = p.line();
        assert_eq!(line, "ready\n", "holder said {line:?}");
        let mut c = Client::dial(&socket_path(&dir.0, "tr"), "dev1");
        c.attach(wire::Request::default());
        c.wait_text("hi");
        let path = trace_dir.0.join(format!("holder-{}.jsonl", p.child.id()));
        if !on {
            std::thread::sleep(Duration::from_millis(100));
            let ents: Vec<_> = std::fs::read_dir(&trace_dir.0).unwrap().collect();
            assert!(ents.is_empty(), "untraced holder wrote {ents:?}");
            continue;
        }
        let traced = eventually(WAIT, || std::fs::read_to_string(&path).unwrap_or_default().contains(r#""ev":"ready_written""#));
        assert!(traced, "no trace from the holder process");
        read_trace(&path);
    }
}
