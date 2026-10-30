use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Writes OSEWA_TRACE events (the e2e harness's latency breakdown): one JSON
/// line per event, each with a single write(2) on an O_APPEND file and no
/// buffering, so a killed holder loses nothing it traced. Callers hold an
/// Option and check it before building an event, so an untraced holder
/// pays nothing.
pub(crate) struct Tracer {
    f: File,
}

impl Tracer {
    pub fn open(dir: &str) -> std::io::Result<Option<Tracer>> {
        if dir.is_empty() {
            return Ok(None);
        }
        let path = Path::new(dir).join(format!("holder-{}.jsonl", std::process::id()));
        let f = OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
        Ok(Some(Tracer { f }))
    }

    /// Writes {"t":<unix ns>,"ev":ev, k:v...}.
    pub fn emit(&self, ev: &str, kv: &[(&str, Value)]) {
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut b = format!("{{\"t\":{t},\"ev\":{}", Value::from(ev));
        for (k, v) in kv {
            b.push(',');
            b.push_str(&Value::from(*k).to_string());
            b.push(':');
            b.push_str(&v.to_string());
        }
        b.push_str("}\n");
        let _ = (&self.f).write_all(b.as_bytes());
    }
}
