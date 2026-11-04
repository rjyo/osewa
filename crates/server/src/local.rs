//! Holder calls over the run directory's sockets: what the server does for
//! session.list, session.kill and session.reconcile, also usable without a
//! server (`osewa local`, which moshi-hook runs on the host).

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use osewa_wire as wire;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::client::valid_id;
use crate::{incompatible, read_frame, refused, Tunables};

/// How young a tagged session reconcile leaves alone: its creator may not
/// have recorded the tag yet (a card still connecting).
pub const RECONCILE_GRACE: Duration = Duration::from_secs(120);

/// The most tags a reconcile takes.
pub const MAX_RECONCILE_TAGS: usize = 1024;

/// The holders in one run directory.
pub struct Holders<'a> {
    pub run_dir: &'a Path,
    pub tune: &'a Tunables,
}

impl Holders<'_> {
    fn socket_path(&self, id: &str) -> PathBuf {
        osewa_holder::socket_path(self.run_dir, id)
    }

    /// Connects to a session's holder and introduces client_id.
    pub async fn dial(&self, id: &str, client_id: &str) -> io::Result<UnixStream> {
        let t = self.tune;
        let path = self.socket_path(id);
        // Worded as v0 servers word it ("dial unix ..."), which clients show.
        let mut conn = match tokio::time::timeout(t.dial_timeout, UnixStream::connect(&path)).await {
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, format!("dial unix {}: i/o timeout", path.display()))),
            Ok(Err(e)) => {
                let msg = format!("dial unix {}: connect: {}", path.display(), osewa_execproc::error_text(&e));
                return Err(io::Error::new(e.kind(), msg));
            }
            Ok(Ok(c)) => c,
        };
        let hello = wire::Hello { version: wire::VERSION, client_id: client_id.into(), ..Default::default() };
        let mut b = Vec::new();
        wire::json(wire::TYPE_HELLO, 0, &hello).encode_to(&mut b)?;
        tokio::time::timeout(t.holder_timeout, conn.write_all(&b))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write holder: i/o timeout"))??;
        Ok(conn)
    }

    /// Makes one request on a short-lived holder connection.
    pub async fn call(&self, id: &str, client_id: &str, mut req: wire::Request) -> io::Result<wire::Response> {
        let mut conn = self.dial(id, client_id).await?;
        req.client_id = client_id.into();
        let call = async {
            let mut b = Vec::new();
            wire::json(wire::TYPE_REQUEST, 0, &req).encode_to(&mut b)?;
            conn.write_all(&b).await?;
            let mut rd = BufReader::new(&mut conn);
            loop {
                let f = read_frame(&mut rd).await?;
                if f.ty != wire::TYPE_RESPONSE {
                    continue;
                }
                let resp: wire::Response = f.decode()?;
                if resp.id == req.id {
                    return Ok(resp);
                }
            }
        };
        tokio::time::timeout(self.tune.holder_timeout, call)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "holder: i/o timeout"))?
    }

    /// The session's info from its holder, marked incompatible when the
    /// holder is another build than this one.
    pub async fn info(&self, id: &str, client_id: &str) -> io::Result<Option<wire::SessionInfo>> {
        let req = wire::Request { id: 1, method: wire::METHOD_INFO.into(), ..Default::default() };
        let r = self.call(id, client_id, req).await?;
        if !r.error.is_empty() {
            return Ok(None);
        }
        Ok(r.sessions.into_iter().next().map(|mut s| {
            s.incompatible = incompatible(s.build.as_ref());
            s
        }))
    }

    /// Every session whose holder answers, oldest first. Sockets that
    /// refuse are unlinked once proven stale.
    pub async fn list(&self, client_id: &str) -> io::Result<Vec<wire::SessionInfo>> {
        let mut ids = Vec::new();
        for e in std::fs::read_dir(self.run_dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".sock").filter(|id| valid_id(id)) {
                ids.push((id.to_string(), e.path()));
            }
        }
        let calls = ids.iter().map(|(id, path)| async move {
            match self.info(id, client_id).await {
                Err(e) => {
                    if refused(&e) {
                        self.remove_if_stale(path).await;
                    }
                    None
                }
                Ok(s) => s,
            }
        });
        let mut sessions: Vec<wire::SessionInfo> = futures_join_all(calls).await.into_iter().flatten().collect();
        sessions.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        Ok(sessions)
    }

    /// Ends a session: its program gets SIGHUP (SIGKILL after a grace), and
    /// the holder goes soon after.
    pub async fn kill(&self, id: &str, client_id: &str) -> io::Result<wire::Response> {
        let req = wire::Request { id: 1, method: wire::METHOD_KILL.into(), session: id.into(), ..Default::default() };
        self.call(id, client_id, req).await
    }

    /// Kills creator's tagged sessions whose tag isn't in keep, but none
    /// younger than grace, and returns them. Untagged sessions, and other
    /// clients', are never touched.
    pub async fn reconcile(&self, client_id: &str, creator: &str, keep: &[String], grace: Duration) -> io::Result<Vec<wire::SessionInfo>> {
        let keep: HashSet<&str> = keep.iter().map(String::as_str).collect();
        let now = unix_now();
        let mut gone = Vec::new();
        for s in self.list(client_id).await? {
            if s.creator != creator || s.tag.is_empty() || keep.contains(s.tag.as_str()) {
                continue;
            }
            if now.saturating_sub(s.created) < grace.as_secs() as i64 {
                continue;
            }
            match self.kill(&s.id, client_id).await {
                Ok(r) if r.error.is_empty() || r.error == wire::ERR_NOT_FOUND => gone.push(s),
                Err(e) if e.kind() == io::ErrorKind::NotFound => gone.push(s),
                _ => {}
            }
        }
        Ok(gone)
    }

    /// Unlinks a socket that refused a connection, but only if it is older
    /// than any holder still starting up and refuses again after a pause.
    /// Unlinking a live holder's socket would orphan it.
    pub async fn remove_if_stale(&self, path: &Path) {
        let t = self.tune;
        let young = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(|m| SystemTime::now().duration_since(m).unwrap_or_default() < t.stale_age)
            .unwrap_or(true);
        if young {
            return;
        }
        tokio::time::sleep(t.stale_retry).await;
        match tokio::time::timeout(t.dial_timeout, UnixStream::connect(path)).await {
            Ok(Err(e)) if refused(&e) => {
                let _ = std::fs::remove_file(path);
            }
            _ => {}
        }
    }
}

/// Seconds since the epoch, as SessionInfo.created counts them.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Runs futures concurrently on this task and collects their outputs in
/// order.
async fn futures_join_all<F: std::future::Future>(fs: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut fs: Vec<_> = fs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = (0..fs.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in fs.iter_mut().enumerate() {
            if out[i].is_some() {
                continue;
            }
            match f.as_mut().poll(cx) {
                std::task::Poll::Ready(v) => out[i] = Some(v),
                std::task::Poll::Pending => pending = true,
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().map(|o| o.expect("polled to completion")).collect()
}

/// `osewa local <ls|kill|reconcile>`: the run directory's sessions,
/// without a server or an enrolled device, for tools on the host
/// (moshi-hook) acting for a device. JSON on stdin and stdout:
///
/// - `ls`: prints {"sessions":[...]}.
/// - `kill <id>`: prints {}.
/// - `reconcile`: reads {"creator":"<client_id>","keep":["<tag>",...]}
///   (and "grace_seconds", default RECONCILE_GRACE), prints
///   {"sessions":[...the killed...]}.
pub fn main(args: &[String], stdin: impl io::Read, mut out: impl io::Write) -> Result<(), String> {
    let run_dir = osewa_paths::run_dir();
    let tune = Tunables::default();
    let h = Holders { run_dir: &run_dir, tune: &tune };
    const CLIENT: &str = "osewa-local";
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let local = tokio::task::LocalSet::new();
    let resp = match args {
        [cmd] if cmd == "ls" => {
            let sessions = local.block_on(&rt, h.list(CLIENT)).map_err(|e| format!("ls: {e}"))?;
            wire::Response { sessions, ..Default::default() }
        }
        [cmd, id] if cmd == "kill" => {
            if !valid_id(id) {
                return Err(format!("kill: bad session id {}", wire::quote(id)));
            }
            let r = local.block_on(&rt, h.kill(id, CLIENT)).map_err(|e| format!("kill: {e}"))?;
            if !r.error.is_empty() && r.error != wire::ERR_NOT_FOUND {
                return Err(format!("kill: {}: {}", r.error, r.message));
            }
            wire::Response::default()
        }
        [cmd] if cmd == "reconcile" => {
            let mut b = Vec::new();
            stdin.take(wire::MAX_FRAME as u64).read_to_end(&mut b).map_err(|e| format!("reconcile: {e}"))?;
            let v: serde_json::Value = serde_json::from_slice(&b).map_err(|e| format!("reconcile: {e}"))?;
            let creator = v.get("creator").and_then(|c| c.as_str()).unwrap_or_default().to_string();
            let keep: Vec<String> = match v.get("keep") {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(k) => serde_json::from_value(k.clone()).map_err(|e| format!("reconcile: keep: {e}"))?,
            };
            let grace = match v.get("grace_seconds") {
                None | Some(serde_json::Value::Null) => RECONCILE_GRACE,
                Some(g) => Duration::from_secs(g.as_u64().ok_or("reconcile: grace_seconds must be a whole number")?),
            };
            let req = (creator, keep, grace);
            let (creator, keep, grace) = req;
            if creator.is_empty() {
                return Err("reconcile: creator is required".into());
            }
            if keep.len() > MAX_RECONCILE_TAGS {
                return Err(format!("reconcile: more than {MAX_RECONCILE_TAGS} tags"));
            }
            let sessions = local
                .block_on(&rt, h.reconcile(CLIENT, &creator, &keep, grace))
                .map_err(|e| format!("reconcile: {e}"))?;
            wire::Response { sessions, ..Default::default() }
        }
        _ => return Err("usage: osewa local ls | kill <id> | reconcile (JSON {creator, keep} on stdin)".into()),
    };
    let j = serde_json::to_string(&resp).map_err(|e| e.to_string())?;
    writeln!(out, "{j}").map_err(|e| e.to_string())
}
