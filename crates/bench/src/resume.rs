//! `osewa-bench resume`: cold resumes from inside a
//! container, next to the server on one network, so TCP handshakes run end
//! to end (a host's published port goes through a proxy that accepts at
//! once).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use osewa_client::{AttachOptions, Config, Conn};
use osewa_ghostty::{self as ghostty, Terminal};
use osewa_mirror::Mirror;

use crate::{json_f64s, Error};

/// Bounds one cold resume, dial to READY.
const RESUME_TIMEOUT: Duration = Duration::from_secs(20);

pub fn resume_main(args: &[String], w: &mut dyn Write) -> Result<(), Error> {
    let mut fs = osewa_flag::FlagSet::new("resume");
    fs.value("addr", "", "server host:port")
        .value("server-key", "", "server key, base64")
        .value("key", "", "client key (PEM)")
        .value("session", "", "session id")
        .value("cols", "80", "terminal width")
        .value("rows", "24", "terminal height")
        .value("runs", "30", "cold resumes")
        .value("want", "", "file with the expected visible screen");
    let p = crate::parse(&fs, args)?;
    let (cols, rows, runs) = (crate::int(&p, "cols")?, crate::int(&p, "rows")?, crate::int(&p, "runs")?);
    let (addr, session, key_path) = (p.str("addr"), p.str("session"), p.str("key"));
    if addr.is_empty() || session.is_empty() || key_path.is_empty() {
        return Err(Error("resume: -addr, -session and -key are required".into()));
    }
    let sk = osewa_auth::decode_key(&p.str("server-key")).map_err(|e| Error(e.to_string()))?;
    let id = osewa_auth::Identity::load_or_create(Path::new(&key_path)).map_err(|e| Error(e.to_string()))?;
    let want = match p.str("want") {
        w if w.is_empty() => String::new(),
        w => std::fs::read_to_string(&w).map_err(|e| Error(format!("open {w}: {}", crate::go_io_error(&e))))?,
    };
    let tls = Arc::new(osewa_auth::client_config(&id, sk));
    let (mut total, mut dial, mut mismatch) = (Vec::new(), Vec::new(), 0);
    for i in 0..runs {
        // Config has private fields: no struct update syntax from here.
        let mut cfg = Config::default();
        cfg.url = format!("wss://{addr}/osewa");
        cfg.tls = Some(tls.clone());
        cfg.client_id = format!("resume-{}-{i}", std::process::id());
        cfg.name = "osewa-bench".into();
        let (t, d, ok) = cold_resume(cfg, &session, cols, rows, &want)?;
        if !ok {
            mismatch += 1;
        }
        total.push(ms(t));
        dial.push(ms(d));
    }
    let out = format!("{{\"total_ms\":{},\"dial_ms\":{},\"mismatch\":{mismatch}}}\n", json_f64s(&total), json_f64s(&dial));
    w.write_all(out.as_bytes()).map_err(Error::io)
}

/// Milliseconds with microsecond precision, truncated, as the harness
/// reports them.
pub(crate) fn ms(d: Duration) -> f64 {
    d.as_micros() as f64 / 1000.0
}

/// Dials as a new client and times the first renderable screen.
fn cold_resume(cfg: Config, session: &str, cols: i64, rows: i64, want: &str) -> Result<(Duration, Duration, bool), Error> {
    let session = session.to_string();
    let want = want.to_string();
    let r = osewa_client::block_on(osewa_client::timeout(RESUME_TIMEOUT, async move {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let start = Instant::now();
        let conn = Conn::dial(cfg).await.map_err(|e| Error(e.to_string()))?;
        let dial = start.elapsed();
        let m = Mirror::new(osewa_mirror::Options {
            on_ready: Some(Box::new(move |t: &mut Terminal| {
                if let Some(tx) = tx.take() {
                    let _ = tx.send((start.elapsed(), visible_text(t)));
                }
            })),
            ..Default::default()
        });
        let res = async {
            let s = conn
                .attach(&session, AttachOptions { cols, rows, renderer: Some(Box::new(m.clone())), ..Default::default() })
                .await
                .map_err(|e| Error(e.to_string()))?;
            let got = rx.await.map_err(|_| Error("resume: no READY".into()))?;
            s.close();
            Ok((got.0, dial, want.is_empty() || got.1 == want))
        }
        .await;
        m.close();
        conn.close().await;
        res
    }));
    r.unwrap_or_else(|| Err(Error("resume: no READY: context deadline exceeded".into())))
}

/// The active area like the client's visible text: one line per row,
/// trailing blanks and trailing empty rows trimmed.
pub fn visible_text(t: &Terminal) -> String {
    let Ok(b) = t.format(ghostty::Format::Plain, false, false) else { return String::new() };
    let s = String::from_utf8_lossy(&b);
    let lines: Vec<&str> = s.split('\n').collect();
    let (_, h) = t.size();
    let skip = t.scrollback_rows().min(lines.len());
    let mut lines: Vec<&str> = lines[skip..].iter().take(h).map(|l| l.trim_end_matches(' ')).collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    lines.join("\n")
}
