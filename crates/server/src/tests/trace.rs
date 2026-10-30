//! Ported from server/trace_test.go.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use osewa_wire as wire;
use serde_json::Value;

use super::helpers::*;
use crate::trace::Tracer;

fn read_trace(path: &Path) -> Vec<HashMap<String, Value>> {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    s.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("line {l:?}: {e}"))).collect()
}

#[test]
fn trace() {
    let dir = TempDir::new();
    let trace_dir = dir.path().to_string_lossy().into_owned();
    run(async {
        let e = Env::with(|c| c.trace_dir = trace_dir.clone());
        let mut c = e.connect("phone").await;
        let id = c.create(&["/bin/cat"]).await;
        let r = c.attach(&id, 3, true).await;
        assert!(r.error.is_empty(), "attach {r:?}");
        let begin = c.until(|f| f.ty == wire::TYPE_SNAPSHOT_BEGIN && f.stream == 3).await;
        let sb: wire::SnapshotBegin = begin.decode().unwrap();
        c.until(|f| f.ty == wire::TYPE_SNAPSHOT_END && f.stream == 3).await;
        c.input(3, 7, "hi\r").await;
        c.output_ack(3, "hi").await;
        let r = c.call(wire::Request { session: id.clone(), ..req(wire::METHOD_KILL) }).await;
        assert!(r.error.is_empty(), "kill {r:?}");
        c.until(|f| f.ty == wire::TYPE_STREAM_END && f.stream == 3).await;

        // The writer traces a frame after its write returns, which can be
        // after the client has read it.
        let path = e.srv.trace.as_ref().unwrap().path();
        let mut evs = Vec::new();
        for _ in 0..500 {
            evs = read_trace(&path);
            if evs.len() >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        struct Want {
            ev: &'static str,
            ty: u8,
            gen: bool,
            seq: u64,
            hits: usize,
        }
        let mut want = [
            Want { ev: "relay_c2h", ty: wire::TYPE_INPUT, gen: false, seq: 7, hits: 0 },
            Want { ev: "relay_h2c", ty: wire::TYPE_SNAPSHOT_BEGIN, gen: true, seq: 0, hits: 0 },
            Want { ev: "relay_h2c", ty: wire::TYPE_SNAPSHOT_READY, gen: true, seq: 0, hits: 0 },
            Want { ev: "relay_h2c", ty: wire::TYPE_STREAM_END, gen: false, seq: 0, hits: 0 },
        ];
        for ev in &evs {
            assert!(ev.get("t").is_some() && ev["stream"] == 3, "event {ev:?}");
            for w in want.iter_mut() {
                if ev["ev"] != w.ev || ev["type"] != w.ty {
                    continue;
                }
                w.hits += 1;
                if w.gen {
                    assert_eq!(ev["generation"], sb.generation, "generation in {ev:?}");
                }
                if w.ev == "relay_h2c" {
                    assert!(ev.get("len").is_some(), "no len in {ev:?}");
                }
                if w.seq != 0 {
                    assert_eq!(ev["seq"], w.seq, "seq in {ev:?}");
                }
            }
        }
        for w in &want {
            assert_eq!(w.hits, 1, "{} type 0x{:02x}", w.ev, w.ty);
        }
        assert_eq!(evs.len(), want.len(), "OUTPUT and DATA aren't traced");
    });
}

// Set, the tracer creates nothing until the first event, so a server that
// traces nothing leaves no file; unset, there is no tracer.
#[test]
fn trace_off() {
    let dir = TempDir::new();
    let trace_dir = dir.path().to_string_lossy().into_owned();
    run(async {
        let e = Env::with(|c| c.trace_dir = trace_dir.clone());
        let mut c = e.connect("phone").await;
        assert!(c.list().await.error.is_empty());
        assert!(!e.srv.trace.as_ref().unwrap().opened());
        let off = Env::new();
        assert!(off.srv.trace.is_none());
    });
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "files written without events");
}

// A tracer that can't open its file stays quiet instead of failing.
#[test]
fn trace_open_fails() {
    let dir = TempDir::new();
    let tr = Tracer::new(&dir.path().join("missing").to_string_lossy()).unwrap();
    tr.event("x", &[("a", 1.into())]);
    assert!(!tr.opened(), "opened a file in a missing directory");
}

// Every line is one complete JSON object.
#[test]
fn trace_lines() {
    let dir = TempDir::new();
    let tr = Tracer::new(&dir.path().to_string_lossy()).unwrap();
    for g in 0..8 {
        for i in 0..200 {
            tr.event("e", &[("g", g.into()), ("i", i.into()), ("s", "a\"b\n".into())]);
        }
    }
    let evs = read_trace(&tr.path());
    assert_eq!(evs.len(), 1600);
    assert!(evs[0]["s"] == "a\"b\n" && evs[0]["ev"] == "e", "line {:?}", evs[0]);
    let t = evs[0]["t"].as_u64().unwrap() as u128;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    assert!(now - t < 60_000_000_000, "t {t}");
}
