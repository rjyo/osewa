//! OSEWA_TRACE events (Go's client/trace.go): JSON lines in
//! `<dir>/client-<pid>.jsonl`, each with one write(2) on an O_APPEND file,
//! so a killed process loses nothing it traced. No tracer is tracing off:
//! callers check for one before building anything.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use osewa_wire as wire;
use serde_json::{json, Map, Value};

pub(crate) struct Tracer {
    f: File,
}

type Tracers = Mutex<Option<HashMap<String, Option<Arc<Tracer>>>>>;

/// The process's tracer for $OSEWA_TRACE, or None when it is unset or
/// can't be opened.
pub(crate) fn open() -> Option<Arc<Tracer>> {
    open_dir(&std::env::var("OSEWA_TRACE").ok().filter(|d| !d.is_empty())?)
}

/// The process's tracer for dir.
pub(crate) fn open_dir(dir: &str) -> Option<Arc<Tracer>> {
    let dir = dir.to_string();
    static TRACERS: Tracers = Mutex::new(None);
    let mut all = TRACERS.lock().unwrap();
    let all = all.get_or_insert_with(HashMap::new);
    if let Some(t) = all.get(&dir) {
        return t.clone();
    }
    let path = Path::new(&dir).join(format!("client-{}.jsonl", std::process::id()));
    let t = OpenOptions::new().append(true).create(true).mode(0o600).open(path).ok().map(|f| Arc::new(Tracer { f }));
    all.insert(dir, t.clone());
    t
}

impl Tracer {
    /// Writes {"t", "ev", fields...}.
    pub fn event(&self, ev: &str, fields: &[(&str, Value)]) {
        let mut m = Map::new();
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64);
        m.insert("t".into(), json!(t));
        m.insert("ev".into(), json!(ev));
        for (k, v) in fields {
            m.insert((*k).into(), v.clone());
        }
        let mut b = serde_json::to_vec(&m).unwrap_or_default();
        b.push(b'\n');
        let _ = (&self.f).write_all(&b);
    }

    /// Traces a frame as it is read, before it is queued or applied.
    pub fn frame_recv(&self, f: &wire::Frame) {
        let mut fields = vec![("stream", json!(f.stream)), ("type", json!(f.ty)), ("len", json!(f.payload.len()))];
        match f.ty {
            wire::TYPE_SNAPSHOT_BEGIN => {
                if let Ok(b) = f.decode::<wire::SnapshotBegin>() {
                    fields.push(("generation", json!(b.generation)));
                    fields.push(("history", json!(b.history.unwrap_or(true))));
                }
            }
            wire::TYPE_OUTPUT | wire::TYPE_EVENT => {
                if let Ok((end, _)) = wire::split_end(&f.payload) {
                    fields.push(("end", json!(end)));
                }
            }
            _ => {}
        }
        self.event("frame_recv", &fields);
    }

    /// Traces an INPUT frame once its write returned.
    pub fn input_sent(&self, b: &[u8]) {
        #[derive(serde::Deserialize)]
        struct Seq {
            #[serde(default)]
            seq: u64,
        }
        let Ok(f) = wire::Frame::unmarshal(b) else { return };
        if f.ty != wire::TYPE_INPUT {
            return;
        }
        if let Ok(s) = f.decode::<Seq>() {
            self.event("input_sent", &[("stream", json!(f.stream)), ("seq", json!(s.seq))]);
        }
    }
}
