//! The command line (Go's cmd/osewa/main.go): dispatch, the client
//! commands' environment, flags, errors and their exit codes.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use osewa_client::login::{CheckFn, InstallFn, SshConfigFn, SshFn};
use tokio::sync::watch;

const USAGE: &str = r#"usage: osewa <command> [args]

host commands:
  server [--daemon] [--listen :7322] [--name N] [--no-compress] [--no-forward] [--no-exec] [--no-quic]
                                       serve sessions to enrolled devices; --daemon
                                       detaches (log in ~/.local/state/osewa/server.log);
                                       --no-forward and --no-exec refuse port forwarding
                                       and commands without a terminal
  service install [server flags] | uninstall
                                       run the server as a systemd/launchd user service
  enroll [--port N]                    add a device (run over ssh by login)
  holder ...                           one session's process (started by server)

client commands:
  login <ssh-target> [--name N] [--addr host:port] [--device N] [--no-install] [--service] [--no-check]
  ls <host>
  new <host> [-- argv...]
  attach [--take] [--quiet] [--predict auto|always|never] <host> [id]
                                       ~. detaches (after a newline); while viewing,
                                       Ctrl-T takes ownership
  kill <host> <id>
  exec [--cwd dir] [--env K=V]... <host> -- argv...
                                       run a command on the host without a terminal;
                                       stdin, stdout and stderr pass through, and osewa
                                       exits with its code
  update <host>                        put this build on a host over osewa (no ssh)
                                       and restart its server with it
  forward <host> [bind:]port[:host:hostport]...
                                       forward local ports through the host, as ssh -L
                                       (bind defaults to 127.0.0.1, the target to the
                                       host's localhost at the same port) until Ctrl-C
  version
"#;

/// Set at release time from OSEWA_VERSION.
pub(crate) const VERSION: &str = match option_env!("OSEWA_VERSION") {
    Some(v) => v,
    None => "dev",
};

/// A writer the commands share: stdout or stderr, or a test's buffer.
#[derive(Clone)]
pub(crate) struct Out(Arc<Mutex<Box<dyn Write + Send>>>);

impl Out {
    pub fn new(w: impl Write + Send + 'static) -> Out {
        Out(Arc::new(Mutex::new(Box::new(w))))
    }
}

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let mut w = self.0.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(b)?;
        w.flush()?;
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) type Stdin = Box<dyn Read + Send>;
/// Puts the terminal in raw mode and returns what restores it, or None
/// when there was nothing to do (stdin is no terminal).
pub(crate) type RawFn = Arc<dyn Fn() -> io::Result<Option<Box<dyn FnOnce() + Send>>> + Send + Sync>;

/// What the client commands run with (Go's env).
#[derive(Clone)]
pub(crate) struct Env {
    /// Taken by the one command that reads it (exec, attach).
    pub stdin: Arc<Mutex<Option<Stdin>>>,
    pub stdout: Out,
    pub stderr: Out,
    /// The client config directory (hosts.json, client.key).
    pub dir: PathBuf,
    /// Runs a command on an ssh target with stdin; None runs ssh.
    pub ssh: Option<SshFn>,
    /// Puts this build of osewa on a target; None is install::remote.
    pub install: Option<InstallFn>,
    /// `ssh -G target`; None runs ssh.
    pub ssh_config: Option<SshConfigFn>,
    /// How login checks a host; None dials it.
    pub check: Option<CheckFn>,
    /// Raw mode for attach; None leaves the terminal alone.
    pub raw: Option<RawFn>,
    /// Overrides the process's client id, for tests that need two.
    pub id: String,
    /// Config.carrier for every dial; None leaves it to OSEWA_CARRIER.
    pub carrier: Option<String>,
    /// What update runs on the host to restart its server; tests on this
    /// machine replace it, which would restart the real one.
    pub update_restart: String,
}

impl Env {
    /// This process's: stdin, stdout, stderr and the config directory.
    pub fn process() -> Env {
        let mut e = Env::new(io::stdin(), io::stdout(), io::stderr(), osewa_client::hosts::default_dir());
        // Raw mode when stdin is a terminal.
        e.raw = Some(Arc::new(|| if crate::tty::is_terminal(0) { crate::tty::make_raw(0).map(Some) } else { Ok(None) }));
        e
    }

    pub fn new(stdin: impl Read + Send + 'static, stdout: impl Write + Send + 'static, stderr: impl Write + Send + 'static, dir: PathBuf) -> Env {
        Env {
            stdin: Arc::new(Mutex::new(Some(Box::new(stdin)))),
            stdout: Out::new(stdout),
            stderr: Out::new(stderr),
            dir,
            ssh: None,
            install: None,
            ssh_config: None,
            check: None,
            raw: None,
            id: String::new(),
            carrier: None,
            update_restart: crate::update::UPDATE_RESTART.into(),
        }
    }

    pub fn take_stdin(&self) -> Option<Stdin> {
        self.stdin.lock().unwrap().take()
    }

    pub fn out(&self) -> Out {
        self.stdout.clone()
    }

    pub fn err(&self) -> Out {
        self.stderr.clone()
    }
}

/// Why a command failed.
#[derive(Debug)]
pub(crate) enum Error {
    /// Print usage and exit 2 (Go's errUsage and flag.ErrHelp).
    Usage,
    Client(osewa_client::Error),
    Msg(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage => f.write_str("usage"),
            Error::Client(e) => e.fmt(f),
            Error::Msg(s) => f.write_str(s),
        }
    }
}

impl From<osewa_client::Error> for Error {
    fn from(e: osewa_client::Error) -> Error {
        Error::Client(e)
    }
}

impl From<osewa_client::hosts::Error> for Error {
    fn from(e: osewa_client::hosts::Error) -> Error {
        Error::Client(e.into())
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Msg(e.to_string())
    }
}

impl From<String> for Error {
    fn from(s: String) -> Error {
        Error::Msg(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Error {
        Error::Msg(s.into())
    }
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;

/// Cancels a command, as Go's context: SIGINT and SIGTERM, or a test.
#[derive(Clone)]
pub(crate) struct Ctx {
    tx: Arc<watch::Sender<bool>>,
}

impl Ctx {
    pub fn new() -> Ctx {
        Ctx { tx: Arc::new(watch::Sender::new(false)) }
    }

    /// Cancelled on SIGINT or SIGTERM (Go's signal.NotifyContext). Needs a
    /// tokio runtime.
    pub fn signals() -> Ctx {
        let ctx = Ctx::new();
        let c = ctx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else { return };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            c.cancel();
        });
        ctx
    }

    /// Cancelled after d (tests).
    #[cfg(test)]
    pub fn timeout(d: Duration) -> Ctx {
        let ctx = Ctx::new();
        let c = ctx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(d);
            c.cancel();
        });
        ctx
    }

    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_done(&self) -> bool {
        *self.tx.borrow()
    }

    pub async fn done(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    /// Runs f unless and until the context is done: None if it was.
    pub async fn run<F: std::future::Future>(&self, f: F) -> Option<F::Output> {
        tokio::select! {
            biased;
            _ = self.done() => None,
            v = f => Some(v),
        }
    }
}

/// Error for a command interrupted by its context.
pub(crate) fn interrupted() -> Error {
    Error::Msg("interrupted".into())
}

/// Runs f with a deadline of d and the context: the error names which.
pub(crate) async fn bounded<T>(ctx: &Ctx, d: Duration, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    match ctx.run(tokio::time::timeout(d, f)).await {
        None => Err(interrupted()),
        Some(Err(_)) => Err(Error::Msg("context deadline exceeded".into())),
        Some(Ok(r)) => r,
    }
}

/// Runs osewa with args (without the program name).
pub(crate) fn run(args: &[String], env: Env) -> i32 {
    let Some((cmd, rest)) = args.split_first() else {
        let _ = write!(env.err(), "{USAGE}");
        return 2;
    };
    match cmd.as_str() {
        "server" => return crate::daemon::server_main(rest, env.out(), env.err()),
        "service" => return crate::service::service_main(rest, env.out(), env.err()),
        "holder" => return osewa_holder::main(rest),
        "enroll" => return osewa_auth::enroll_main(rest),
        "-h" | "--help" | "help" => {
            let _ = write!(env.out(), "{USAGE}");
            return 0;
        }
        "version" | "--version" => {
            let _ = writeln!(
                env.out(),
                "osewa {VERSION} (protocol {}, ghostty {}, snapshot v{})",
                osewa_wire::VERSION,
                osewa_ghostty::COMMIT,
                osewa_ghostty::SNAPSHOT_VERSION
            );
            return 0;
        }
        _ => {}
    }
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(env.err(), "osewa {cmd}: {e}");
            return 1;
        }
    };
    rt.block_on(async {
        let ctx = Ctx::signals();
        run_client(&ctx, cmd, rest, &env).await
    })
}

/// Runs a client command and turns its outcome into an exit code.
pub(crate) async fn run_client(ctx: &Ctx, cmd: &str, rest: &[String], env: &Env) -> i32 {
    let r: Result<i32> = match cmd {
        "login" => env.login(ctx, rest).await.map(|_| 0),
        "ls" => env.ls(ctx, rest).await.map(|_| 0),
        "new" => env.new_session(ctx, rest).await.map(|_| 0),
        "kill" => env.kill(ctx, rest).await.map(|_| 0),
        "attach" => crate::attach::attach(ctx, env, rest).await,
        "exec" => env.exec(ctx, rest).await,
        "forward" => env.forward(ctx, rest).await.map(|_| 0),
        "update" => env.update(ctx, rest).await.map(|_| 0),
        _ => Err(Error::Usage),
    };
    match r {
        Ok(code) => code,
        Err(Error::Usage) => {
            let _ = write!(env.err(), "{USAGE}");
            2
        }
        Err(Error::Client(e)) if e.is_incompatible() => {
            let _ = writeln!(env.err(), "osewa {cmd}: {}\nupdate osewa on the host or here", crate::vtfilter::printable_lines(&e.to_string()));
            1
        }
        // Errors can carry the host's words (messages, session lists).
        Err(e) => {
            let _ = writeln!(env.err(), "osewa {cmd}: {}", crate::vtfilter::printable_lines(&e.to_string()));
            1
        }
    }
}

/// A flag's kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Str,
    Bool,
    /// Repeatable, collecting every value (Go's multiFlag).
    Multi,
}

/// Flags as Go's flag package parses them: -x and --x, -x=v and -x v, bool
/// flags take no separate value, -h and -help ask for usage.
#[derive(Default)]
pub(crate) struct Flags {
    defs: HashMap<String, Kind>,
    values: HashMap<String, Vec<String>>,
}

impl Flags {
    pub fn new(defs: &[(&str, Kind)]) -> Flags {
        Flags { defs: defs.iter().map(|(n, k)| (n.to_string(), *k)).collect(), values: HashMap::new() }
    }

    pub fn str(&self, name: &str) -> String {
        self.values.get(name).and_then(|v| v.last()).cloned().unwrap_or_default()
    }

    pub fn bool(&self, name: &str) -> bool {
        self.str(name) == "true"
    }

    pub fn all(&self, name: &str) -> Vec<String> {
        self.values.get(name).cloned().unwrap_or_default()
    }

    /// One flag.Parse: flags up to the first argument or "--" (consumed).
    /// Returns the arguments left.
    fn parse_once(&mut self, args: &[String]) -> Result<Vec<String>> {
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a.len() < 2 || !a.starts_with('-') {
                break;
            }
            if a == "--" {
                i += 1;
                break;
            }
            let body = a.strip_prefix("--").unwrap_or(&a[1..]);
            if body.is_empty() || body.starts_with('-') || body.starts_with('=') {
                return Err(Error::Msg(format!("bad flag syntax: {a}")));
            }
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let Some(&kind) = self.defs.get(name) else {
                if name == "h" || name == "help" {
                    return Err(Error::Usage);
                }
                return Err(Error::Msg(format!("flag provided but not defined: -{name}")));
            };
            let v = match (kind, inline) {
                (Kind::Bool, Some(v)) => match v.as_str() {
                    "1" | "t" | "T" | "true" | "TRUE" | "True" => "true".into(),
                    "0" | "f" | "F" | "false" | "FALSE" | "False" => "false".into(),
                    _ => return Err(Error::Msg(format!("invalid boolean value {} for -{name}: parse error", osewa_wire::go_quote(&v)))),
                },
                (Kind::Bool, None) => "true".into(),
                (_, Some(v)) => v,
                (_, None) => {
                    i += 1;
                    args.get(i).cloned().ok_or_else(|| Error::Msg(format!("flag needs an argument: -{name}")))?
                }
            };
            let vals = self.values.entry(name.into()).or_default();
            if kind != Kind::Multi {
                vals.clear();
            }
            vals.push(v);
            i += 1;
        }
        Ok(args[i..].to_vec())
    }

    /// One flag.Parse that must use every argument.
    pub fn parse_once_all(&mut self, args: &[String]) -> Result<()> {
        if self.parse_once(args)?.is_empty() {
            Ok(())
        } else {
            Err(Error::Usage)
        }
    }

    /// Parses flags anywhere among the positional arguments, which one
    /// flag.Parse alone stops at, and returns the positional ones.
    pub fn parse(&mut self, args: &[String]) -> Result<Vec<String>> {
        let mut pos = Vec::new();
        let mut args = args.to_vec();
        loop {
            let mut rest = self.parse_once(&args)?;
            if rest.is_empty() {
                return Ok(pos);
            }
            pos.push(rest.remove(0));
            args = rest;
        }
    }
}
