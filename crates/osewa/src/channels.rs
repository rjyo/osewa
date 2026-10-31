//! exec and forward.

use std::io::{Read, Write};
use std::time::Duration;

use osewa_client::{hosts, ExecOptions, Link};
use osewa_wire as wire;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::cli::{bounded, Ctx, Env, Error, Flags, Kind, Result};
use crate::commands::split_dashes;
use crate::vtfilter::{printable, printable_lines};

/// Bounds getting a connection and opening one forward.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(15);

impl Env {
    /// Runs `exec <host> [--cwd d] [--env K=V]... -- argv...`: a command on
    /// the host without a PTY, with stdin, stdout and stderr passed through.
    /// It returns the command's exit code.
    pub(crate) async fn exec(&self, ctx: &Ctx, args: &[String]) -> Result<i32> {
        let (args, argv) = split_dashes(args);
        let mut fs = Flags::new(&[("cwd", Kind::Str), ("env", Kind::Multi)]);
        let pos = fs.parse(&args)?;
        if pos.len() != 1 || argv.is_empty() {
            return Err(Error::Usage);
        }
        let (c, _) = self.connect(ctx, &pos, 1, 1).await?;
        let x = match ctx.run(c.exec(ExecOptions { argv, cwd: fs.str("cwd"), env: fs.all("env") })).await {
            Some(Ok(x)) => x,
            Some(Err(e)) => {
                c.close().await;
                return Err(e.into());
            }
            None => {
                c.close().await;
                return Ok(130);
            }
        };
        // Stdin may never end (a terminal); the command's end ends us anyway.
        if let Some(mut stdin) = self.take_stdin() {
            let w = x.stdin();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 32 << 10];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if osewa_client::block_on(w.write_all(&buf[..n])).is_err() {
                                return;
                            }
                        }
                    }
                }
                let _ = w.close();
            });
        } else {
            let _ = x.stdin().close();
        }
        let (stdout, stderr) = (x.stdout(), x.stderr());
        let copy = |r: osewa_client::ExecStdout, mut w: crate::cli::Out| async move {
            let mut buf = vec![0u8; 32 << 10];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 || w.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        };
        let copy_err = |r: osewa_client::ExecStderr, mut w: crate::cli::Out| async move {
            let mut buf = vec![0u8; 32 << 10];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 || w.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        };
        let copies = async {
            tokio::join!(copy(stdout, self.out()), copy_err(stderr, self.err()));
            x.wait().await
        };
        let code = ctx.run(copies).await;
        let r = match code {
            // Interrupted: the command ends, as a shell reports SIGINT.
            None => {
                let _ = x.close().await;
                Ok(130)
            }
            Some(r) => r.map(|c| c as i32).map_err(Error::from),
        };
        c.close().await;
        r
    }

    /// Runs `forward <host> spec...` until interrupted: each accepted local
    /// connection is forwarded through the host. A lost connection is
    /// redialed by the link; listening goes on.
    pub(crate) async fn forward(&self, ctx: &Ctx, args: &[String]) -> Result<()> {
        if args.len() < 2 {
            return Err(Error::Usage);
        }
        let specs = args[1..].iter().map(|a| parse_forward(a)).collect::<Result<Vec<_>>>()?;
        let h = hosts::find(&self.dir, &args[0])?;
        let l = Link::new(self.config(&h)?);
        let r = self.forward_on(ctx, &l, &h, specs).await;
        l.close().await;
        r
    }

    async fn forward_on(&self, ctx: &Ctx, l: &Link, h: &hosts::Host, specs: Vec<ForwardSpec>) -> Result<()> {
        bounded(ctx, FORWARD_TIMEOUT, async {
            let c = l.conn().await?;
            if !c.has(wire::FEATURE_FORWARD).await? {
                return Err(Error::Msg("the host's server doesn't allow forwarding (--no-forward)".into()));
            }
            Ok(())
        })
        .await?;
        let mut lns = Vec::new();
        for f in &specs {
            let ln = TcpListener::bind(&f.listen).await.map_err(|e| Error::Msg(format!("listen tcp {}: {e}", f.listen)))?;
            writeln!(
                self.out(),
                "forwarding {} -> {} on {}",
                ln.local_addr()?,
                join_host_port(&f.host, f.port),
                printable(&h.name)
            )?;
            lns.push(ln);
        }
        let mut tasks = tokio::task::JoinSet::new();
        for (ln, f) in lns.into_iter().zip(specs) {
            let (env, ctx, l) = (self.clone(), ctx.clone(), l.clone());
            tasks.spawn(async move { env.serve_forward(&ctx, &l, ln, f).await });
        }
        ctx.done().await;
        // Listening stops; open connections end with the context too.
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    async fn serve_forward(&self, ctx: &Ctx, l: &Link, ln: TcpListener, f: ForwardSpec) {
        let addr = ln.local_addr().map(|a| a.to_string()).unwrap_or_default();
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let Some(Ok((local, _))) = ctx.run(ln.accept()).await else { break };
            let (env, ctx, l, f, addr) = (self.clone(), ctx.clone(), l.clone(), f.clone(), addr.clone());
            conns.spawn(async move {
                if let Err(e) = pipe_forward(&ctx, &l, local, &f).await {
                    let _ = writeln!(env.err(), "osewa forward {addr}: {}", printable_lines(&e.to_string()));
                }
            });
        }
        drop(ln);
        while conns.join_next().await.is_some() {}
    }
}

/// Copies between local and a new forward until both sides are done,
/// passing half-closes along.
async fn pipe_forward(ctx: &Ctx, l: &Link, mut local: TcpStream, f: &ForwardSpec) -> Result<()> {
    let remote = bounded(ctx, FORWARD_TIMEOUT, async {
        let c = l.conn().await?;
        Ok(c.dial_forward(&f.host, f.port).await?)
    })
    .await?;
    let (mut lr, mut lw) = local.split();
    let (mut rr, mut rw) = (remote.clone(), remote.clone());
    let up = async {
        let _ = tokio::io::copy(&mut lr, &mut rw).await;
        let _ = remote.close_write();
    };
    let down = async {
        let _ = tokio::io::copy(&mut rr, &mut lw).await;
        let _ = lw.shutdown().await;
    };
    ctx.run(async { tokio::join!(up, down) }).await;
    remote.close();
    Ok(())
}

/// One `[bind:]localport[:host:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardSpec {
    /// host:port to listen on.
    pub listen: String,
    /// The target, as the server dials it.
    pub host: String,
    pub port: u16,
}

pub(crate) fn join_host_port(host: &str, port: impl std::fmt::Display) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Reads [bind:]localport[:host:port]; bind defaults to 127.0.0.1 and the
/// target to the server's localhost at the same port. IPv6 addresses go in
/// brackets.
pub(crate) fn parse_forward(s: &str) -> Result<ForwardSpec> {
    let bad = || Error::Msg(format!("bad forward {}: want [bind:]port[:host:hostport]", wire::go_quote(s)));
    let parts = split_colons(s);
    let (mut bind, mut host, mut hport) = ("127.0.0.1", "localhost", "");
    let lport = match parts.as_slice() {
        [l] => *l,
        [b, l] => {
            bind = b;
            l
        }
        [l, h, p] => {
            host = h;
            hport = p;
            l
        }
        [b, l, h, p] => {
            bind = b;
            host = h;
            hport = p;
            l
        }
        _ => return Err(bad()),
    };
    if hport.is_empty() {
        hport = lport;
    }
    let (Ok(lp), Ok(hp)) = (atoi(lport), atoi(hport)) else { return Err(bad()) };
    if !(0..=65535).contains(&lp) || !(1..=65535).contains(&hp) || host.is_empty() {
        return Err(bad());
    }
    let bind = bind.trim_matches(|c| c == '[' || c == ']');
    let host = host.trim_matches(|c| c == '[' || c == ']');
    Ok(ForwardSpec { listen: join_host_port(bind, lport), host: host.into(), port: hp as u16 })
}

/// A plain integer: an optional sign and decimal digits, nothing else.
fn atoi(s: &str) -> std::result::Result<i64, ()> {
    s.parse::<i64>().map_err(drop)
}

/// Splits on colons outside brackets.
fn split_colons(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ':' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}
