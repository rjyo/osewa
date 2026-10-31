//! login, ls, new and kill, and connecting to a known host.

use std::io::Write;
use std::sync::OnceLock;
use std::time::Duration;

use osewa_client::{hosts, login, Conn, CreateOptions};
use osewa_wire as wire;

use crate::cli::{bounded, Ctx, Env, Error, Flags, Kind, Result};
use crate::status::status_text;
use crate::vtfilter::{printable, printable_lines};

/// Bounds connecting to a host.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// This machine's name, as the server shows it.
pub(crate) fn device_name() -> String {
    let mut b = [0u8; 256];
    if unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return String::new();
    }
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// A fresh client id: this device, and a random suffix for the process.
pub(crate) fn new_client_id() -> String {
    let b: [u8; 6] = rand::random();
    format!("{}-{}", device_name(), hex::encode(b))
}

/// Names this process as one app instance: fixed for its life and reused
/// on every resume, so ownership and input seq survive reconnects.
fn process_client_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(new_client_id)
}

/// The device part of a client id ("laptop-0a1b" is "laptop"), printable.
pub(crate) fn owner_name(id: &str) -> String {
    if id.is_empty() {
        return "nobody".into();
    }
    let id = match id.rfind('-') {
        Some(i) if i > 0 => &id[..i],
        _ => id,
    };
    printable(id)
}

impl Env {
    pub(crate) fn client_id(&self) -> String {
        if self.id.is_empty() {
            process_client_id().into()
        } else {
            self.id.clone()
        }
    }

    /// Enrolls this device on a host over ssh and remembers the host.
    pub(crate) async fn login(&self, _ctx: &Ctx, args: &[String]) -> Result<()> {
        let mut fs = Flags::new(&[
            ("name", Kind::Str),
            ("addr", Kind::Str),
            ("device", Kind::Str),
            ("no-install", Kind::Bool),
            ("service", Kind::Bool),
            ("no-check", Kind::Bool),
        ]);
        let pos = fs.parse(args)?;
        if pos.len() != 1 {
            return Err(Error::Usage);
        }
        let target = &pos[0];
        let as_service = fs.bool("service");
        let res = login::login(login::Options {
            target: target.clone(),
            name: fs.str("name"),
            addr: fs.str("addr"),
            device: fs.str("device"),
            dir: self.dir.clone(),
            no_install: fs.bool("no-install"),
            service: as_service,
            no_check: fs.bool("no-check"),
            ssh: self.ssh.clone(),
            ssh_config: self.ssh_config.clone(),
            install: self.install.clone(),
            binary_for: None,
            check: self.check.clone(),
        })
        .await?;
        let mut out = self.out();
        if res.replaced && as_service {
            // service install restarts it.
            writeln!(out, "updated osewa on {target}; its server restarted with the new build")?;
        } else if res.replaced {
            writeln!(out, "updated osewa on {target}; a server already running there keeps its old build until restarted")?;
        } else if res.installed {
            writeln!(out, "installed osewa on {target}")?;
        }
        writeln!(out, "enrolled as {} on {} ({})", wire::quote(&res.device), res.host.name, res.host.addr)?;
        // What the host said about its server.
        for line in &res.server {
            writeln!(out, "{}", printable_lines(line))?;
        }
        if !as_service {
            writeln!(out, "the server won't come back after {target} reboots; to make it a service: osewa login --service {target}")?;
        }
        Ok(())
    }

    /// The client config for a known host, as this device and process.
    pub(crate) fn config(&self, h: &hosts::Host) -> Result<osewa_client::Config> {
        let mut cfg = h.config(&self.dir, &self.client_id(), &device_name())?;
        if let Some(c) = &self.carrier {
            cfg.carrier = c.clone();
        }
        Ok(cfg)
    }

    /// Connects to a known host as this device.
    pub(crate) async fn dial(&self, ctx: &Ctx, h: &hosts::Host) -> Result<Conn> {
        let cfg = self.config(h)?;
        bounded(ctx, DIAL_TIMEOUT, async { Ok(Conn::dial(cfg).await?) }).await
    }

    /// Parses `<host> rest...` and dials the host.
    pub(crate) async fn connect(&self, ctx: &Ctx, args: &[String], min: usize, max: usize) -> Result<(Conn, Vec<String>)> {
        if args.len() < min || args.len() > max {
            return Err(Error::Usage);
        }
        let h = hosts::find(&self.dir, &args[0])?;
        let c = self.dial(ctx, &h).await?;
        Ok((c, args[1..].to_vec()))
    }

    pub(crate) async fn ls(&self, ctx: &Ctx, args: &[String]) -> Result<()> {
        let (c, _) = self.connect(ctx, args, 1, 1).await?;
        let list = ctx.run(c.list()).await;
        c.close().await;
        let list = list.ok_or_else(crate::cli::interrupted)??;
        print_sessions(&mut self.out(), &list)?;
        Ok(())
    }

    pub(crate) async fn new_session(&self, ctx: &Ctx, args: &[String]) -> Result<()> {
        let (args, argv) = split_dashes(args);
        let (c, _) = self.connect(ctx, &args, 1, 1).await?;
        let ((cols, rows), (width, height)) = (crate::tty::term_size(1), crate::tty::term_pixels(1));
        let id = ctx.run(c.create(CreateOptions { cols, rows, width, height, argv, ..Default::default() })).await;
        c.close().await;
        let id = id.ok_or_else(crate::cli::interrupted)??;
        writeln!(self.out(), "{id}")?;
        Ok(())
    }

    pub(crate) async fn kill(&self, ctx: &Ctx, args: &[String]) -> Result<()> {
        let (c, rest) = self.connect(ctx, args, 2, 2).await?;
        let r = ctx.run(c.kill(&rest[0])).await;
        c.close().await;
        Ok(r.ok_or_else(crate::cli::interrupted)??)
    }
}

/// Splits args at the first "--": what comes before, and the argv after.
pub(crate) fn split_dashes(args: &[String]) -> (Vec<String>, Vec<String>) {
    match args.iter().position(|a| a == "--") {
        Some(i) => (args[..i].to_vec(), args[i + 1..].to_vec()),
        None => (args.to_vec(), Vec::new()),
    }
}

/// Prints sessions as `osewa ls` does.
pub(crate) fn print_sessions(w: &mut impl Write, list: &[wire::SessionInfo]) -> std::io::Result<()> {
    let mut rows = vec![["ID", "TITLE", "SIZE", "OWNER", "ATTACHED", "IDLE", "STATUS", "STATE"].map(String::from).to_vec()];
    for s in list {
        let state = if s.exited { format!("exited {}", s.exit_code) } else { "running".into() };
        let owner = if s.owner.is_empty() { "-".into() } else { printable(&owner_name(&s.owner)) };
        let idle = if s.attached == 0 { idle_string(Duration::from_secs(s.idle_seconds.max(0) as u64)) } else { "-".into() };
        // Titles and ids come from the host; a title is whatever a program
        // there set. Control characters would run on our terminal.
        let mut title = printable(&s.title);
        if title.is_empty() {
            title = "-".into();
        }
        let status = s
            .status
            .as_ref()
            .map(|p| status_text(&p.state, &p.kind, &p.msg, p.progress))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "-".into());
        rows.push(vec![
            printable(&s.id),
            title,
            format!("{}x{}", s.cols, s.rows),
            owner,
            s.attached.to_string(),
            idle,
            status,
            state,
        ]);
    }
    w.write_all(tabulate(&rows).as_bytes())
}

/// Aligns rows of cells in columns, as `ls` output has always been laid
/// out (scripts parse it): every cell but a row's last is padded
/// to its column's widest, plus 2, counting characters.
fn tabulate(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut width = vec![0; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate().take(r.len().saturating_sub(1)) {
            width[i] = width[i].max(c.chars().count() + 2);
        }
    }
    let mut out = String::new();
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            out += c;
            if i + 1 < r.len() {
                out.extend(std::iter::repeat_n(' ', width[i] - c.chars().count()));
            }
        }
        out.push('\n');
    }
    out
}

/// Rounds an idle time to its largest units: 45s, 12m, 3h5m, 2d4h.
pub(crate) fn idle_string(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        _ if s < 60 => format!("{s}s"),
        _ if s < 3600 => format!("{}m", s / 60),
        _ if s < 24 * 3600 => format!("{}h{}m", s / 3600, (s / 60) % 60),
        _ => format!("{}d{}h", s / 3600 / 24, (s / 3600) % 24),
    }
}

/// Chooses what `attach <host>` attaches to: the only running session, or
/// a new one when there is none.
pub(crate) async fn pick_session(c: &Conn, cols: i64, rows: i64, (width, height): (i64, i64)) -> Result<String> {
    let list = c.list().await?;
    let running: Vec<_> = list.into_iter().filter(|s| !s.exited).collect();
    match running.len() {
        0 => Ok(c.create(CreateOptions { cols, rows, width, height, ..Default::default() }).await?),
        1 => Ok(running[0].id.clone()),
        _ => {
            let mut b = Vec::new();
            print_sessions(&mut b, &running)?;
            Err(Error::Msg(format!("several sessions, pick one:\n{}", String::from_utf8_lossy(&b))))
        }
    }
}
