//! `osewa update`: this build onto a host over osewa
//! itself, then the host's server restarted with it.

use std::io::Write;
use std::time::{Duration, Instant};

use osewa_client::{hosts, install, login, Conn, ExecOptions};

use crate::cli::{Ctx, Env, Error, Result};
use crate::vtfilter::printable_lines;

/// What update runs on the host to restart its server.
pub(crate) const UPDATE_RESTART: &str = "\"$HOME/.local/bin/osewa\" service restart --detach";

/// Runs a shell script on the host with stdin, and returns its trimmed
/// stdout; a non-zero exit is an error with its stderr.
async fn run_script(ctx: &Ctx, c: &Conn, stdin: Vec<u8>, script: &str) -> Result<String> {
    let x = c.exec(ExecOptions { argv: vec!["/bin/sh".into(), "-c".into(), script.into()], ..Default::default() }).await?;
    let w = x.stdin();
    let feed = async move {
        let _ = w.write_all(&stdin).await;
        let _ = w.close();
    };
    let (stdout, stderr) = (x.stdout(), x.stderr());
    let r = ctx
        .run(async {
            let (_, out, errb) = tokio::join!(feed, stdout.read_to_end(), stderr.read_to_end());
            (out, errb, x.wait().await)
        })
        .await;
    let Some((out, errb, code)) = r else {
        let _ = x.close().await;
        return Err(crate::cli::interrupted());
    };
    let code = code?;
    if code != 0 {
        let errb = String::from_utf8_lossy(&errb.unwrap_or_default()).trim().to_string();
        return Err(Error::Msg(format!("exit {code}: {}", printable_lines(&errb))));
    }
    Ok(String::from_utf8_lossy(&out.unwrap_or_default()).trim().to_string())
}

impl Env {
    /// Puts this build of osewa on a host over osewa itself, as login does
    /// over ssh, and restarts its server with it: no ssh needed, so a host
    /// can be updated where ssh can't reach it (a locked agent, a port only
    /// osewa has open). The restart goes from a process of its own session,
    /// which outlives the server it stops (service restart --detach).
    pub(crate) async fn update(&self, ctx: &Ctx, args: &[String]) -> Result<()> {
        if args.len() != 1 {
            return Err(Error::Usage);
        }
        let h = hosts::find(&self.dir, &args[0])?;
        let c = self.dial(ctx, &h).await?;
        let r = self.update_on(ctx, &c, &h).await;
        c.close().await;
        r?;

        // Back once a new connection is welcomed.
        let deadline = Instant::now() + Duration::from_secs(30);
        tokio::time::sleep(Duration::from_secs(1)).await;
        loop {
            if let Ok(nc) = self.dial(ctx, &h).await {
                let w = tokio::time::timeout(Duration::from_secs(5), nc.welcome()).await;
                nc.close().await;
                if let Ok(Ok(_)) = w {
                    writeln!(self.out(), "{}'s server restarted with it", h.name)?;
                    return Ok(());
                }
            }
            if ctx.is_done() {
                return Err(crate::cli::interrupted());
            }
            if Instant::now() > deadline {
                return Err(Error::Msg(format!(
                    "the server hasn't come back after 30 s; its log is ~/.local/state/osewa/server.log on {}",
                    h.name
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn update_on(&self, ctx: &Ctx, c: &Conn, h: &hosts::Host) -> Result<()> {
        let name = &h.name;
        let uname = run_script(ctx, c, Vec::new(), "uname -s; uname -m").await.map_err(|e| format!("update {name}: uname: {e}"))?;
        let f: Vec<&str> = uname.split_whitespace().collect();
        if f.len() != 2 {
            return Err(Error::Msg(format!("update {name}: uname said {}", osewa_wire::go_quote(&uname))));
        }
        let (os_name, arch_name) = install::platform(f[0], f[1]).map_err(|e| format!("update {name}: {e}"))?;
        let bin = login::local_binaries(os_name, arch_name)?;
        let want = hex::encode(ring::digest::digest(&ring::digest::SHA256, &bin));
        // The same checks as install's: written aside, checked, then moved
        // over the old build, so a server starting meanwhile never runs
        // half of one.
        let script = format!(
            "set -e; umask 022; bin=\"$HOME/{path}\"; mkdir -p \"$(dirname \"$bin\")\"; \
             tmp=\"$bin.update.$$\"; trap 'rm -f \"$tmp\"' EXIT; cat > \"$tmp\"; \
             sum=$( (sha256sum \"$tmp\" || shasum -a 256 \"$tmp\") 2>/dev/null | cut -d\" \" -f1); \
             [ \"$sum\" = \"{want}\" ] || {{ echo \"checksum $sum\" >&2; exit 1; }}; chmod 755 \"$tmp\"; mv -f \"$tmp\" \"$bin\"; echo \"$sum\"",
            path = install::PATH
        );
        let got = run_script(ctx, c, bin, &script).await.map_err(|e| format!("update {name}: copying osewa: {e}"))?;
        if got != want {
            return Err(Error::Msg(format!("update {name}: copied {got}, want {want}")));
        }
        writeln!(self.out(), "copied osewa ({os_name}/{arch_name}) to {name}")?;
        run_script(ctx, c, Vec::new(), &self.update_restart).await.map_err(|e| format!("update {name}: restart: {e}"))?;
        Ok(())
    }
}
