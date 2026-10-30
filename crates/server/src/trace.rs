//! OSEWA_TRACE events: the e2e harness's echo-latency breakdown.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use osewa_wire as wire;
use serde_json::Value;

/// Appends events to <dir>/server-<pid>.jsonl. The server holds an Option,
/// so every call site costs one check when tracing is off.
pub(crate) struct Tracer {
    dir: PathBuf,
    f: OnceCell<Option<File>>,
}

impl Tracer {
    pub fn new(dir: &str) -> Option<Tracer> {
        (!dir.is_empty()).then(|| Tracer { dir: dir.into(), f: OnceCell::new() })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(format!("server-{}.jsonl", std::process::id()))
    }

    /// Writes one line, {"t":…,"ev":…} plus kv pairs, with one write(2) on
    /// an O_APPEND file: no buffer, so a killed server keeps its last
    /// events. The file is opened on the first event, so a server that
    /// traces nothing leaves no file.
    pub fn event(&self, ev: &str, kv: &[(&str, Value)]) {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let f = self.f.get_or_init(|| match OpenOptions::new().create(true).append(true).mode(0o600).open(self.path()) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("osewa server: OSEWA_TRACE: {e}");
                None
            }
        });
        let Some(f) = f else { return };
        let mut b = format!("{{\"t\":{t},\"ev\":{}", Value::from(ev));
        for (k, v) in kv {
            b.push(',');
            b.push_str(&Value::from(*k).to_string());
            b.push(':');
            b.push_str(&v.to_string());
        }
        b.push_str("}\n");
        let _ = (&*f).write_all(b.as_bytes());
    }

    #[cfg(test)]
    pub fn opened(&self) -> bool {
        matches!(self.f.get(), Some(Some(_)))
    }

    /// Records a frame the writer put on the connection. Only the snapshot
    /// markers and STREAM_END are traced; OUTPUT and snapshot data are too
    /// many. READY carries no generation, so gens remembers each stream's
    /// last BEGIN.
    pub fn sent(&self, b: &[u8], gens: &mut HashMap<u32, u32>) {
        let Ok(f) = wire::Frame::unmarshal(b) else { return };
        let base = |f: &wire::Frame| vec![("stream", Value::from(f.stream)), ("type", Value::from(f.ty)), ("len", Value::from(b.len()))];
        match f.ty {
            wire::TYPE_SNAPSHOT_BEGIN => {
                let Ok(sb) = f.decode::<wire::SnapshotBegin>() else { return };
                gens.insert(f.stream, sb.generation);
                let mut kv = base(&f);
                kv.push(("generation", sb.generation.into()));
                self.event("relay_h2c", &kv);
            }
            wire::TYPE_SNAPSHOT_READY => {
                let mut kv = base(&f);
                kv.push(("generation", gens.get(&f.stream).copied().unwrap_or(0).into()));
                self.event("relay_h2c", &kv);
            }
            wire::TYPE_STREAM_END => {
                gens.remove(&f.stream);
                self.event("relay_h2c", &base(&f));
            }
            _ => {}
        }
    }

    /// Records frames written to a holder socket, after the flush.
    pub fn forwarded(&self, fs: &[wire::Frame]) {
        for f in fs {
            if f.ty != wire::TYPE_INPUT {
                continue;
            }
            let Ok(input) = f.decode::<wire::Input>() else { continue };
            self.event("relay_c2h", &[("stream", f.stream.into()), ("type", f.ty.into()), ("seq", input.seq.into())]);
        }
    }
}
