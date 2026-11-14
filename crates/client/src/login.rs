//! Enrolls this device on a host over SSH, as `osewa login` does: installs
//! osewa there if asked, enrolls the device key, leaves a server running,
//! checks that this device reaches it, and saves the host to hosts.json,
//! so any client on this machine (the CLI, app-hook) can dial it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use osewa_auth::{EnrollRequest, EnrollResponse};
use osewa_wire as wire;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::hosts::{self, Host};
use crate::install::{self, BinaryFor};
use crate::Error;

/// A boxed future, for the functions Options can replace.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// Runs command on target with stdin and returns its stdout.
pub type SshFn = Arc<dyn Fn(String, String, Vec<u8>) -> BoxFuture<Result<Vec<u8>, Error>> + Send + Sync>;
/// Returns `ssh -G target`.
pub type SshConfigFn = Arc<dyn Fn(String) -> BoxFuture<Result<Vec<u8>, Error>> + Send + Sync>;
/// Puts osewa on target.
pub type InstallFn = Arc<dyn Fn(String, BinaryFor) -> BoxFuture<Result<install::Outcome, Error>> + Send + Sync>;
/// Dials the host as it will be saved, from the config directory.
pub type CheckFn = Arc<dyn Fn(PathBuf, Host) -> BoxFuture<Result<(), Error>> + Send + Sync>;

/// Configures [`login`]. Only `target` is required.
#[derive(Clone, Default)]
pub struct Options {
    /// The ssh target: host, user@host or an ssh_config alias.
    pub target: String,
    /// The host's name in hosts.json; default the target's host.
    pub name: String,
    /// The server's host:port; default the host ssh resolves the target
    /// to, and the port the server enrolled with.
    pub addr: String,
    /// Names this device to the host; default the hostname.
    pub device: String,
    /// The config directory; default [`hosts::default_dir`].
    pub dir: PathBuf,
    /// Uses the osewa already on the host's PATH.
    pub no_install: bool,
    /// Runs the server as a systemd/launchd user service, which survives
    /// reboots, instead of a detached daemon.
    pub service: bool,
    /// Saves the host without first dialing it from here (it is reachable
    /// only later, say over a VPN not up yet).
    pub no_check: bool,
    /// Connects through `ssh -W` from now on, for a host only ssh reaches.
    /// `addr` is then the server as the host sees it, default its loopback.
    /// Without this, a target ssh_config sends through a proxy still falls
    /// back to it when dialing the host directly fails.
    pub via_ssh: bool,

    /// Default [`run_ssh`].
    pub ssh: Option<SshFn>,
    /// Default `ssh -G`.
    pub ssh_config: Option<SshConfigFn>,
    /// Default [`install::remote`].
    pub install: Option<InstallFn>,
    /// Finds the osewa build to install for a host platform; default
    /// [`local_binaries`].
    pub binary_for: Option<BinaryFor>,
    /// Default [`check_host`] for up to 10 s. A replacement bounds its own
    /// time.
    pub check: Option<CheckFn>,
}

/// What a login did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The host as saved.
    pub host: Host,
    pub device: String,
    /// osewa was put on the host; `replaced`: over another build, which a
    /// server already running there keeps until restarted.
    pub installed: bool,
    pub replaced: bool,
    /// What the host said about its server ("osewa server: ..." and "osewa
    /// service: ..." lines). It comes from the host: escape it before
    /// showing it on a terminal.
    pub server: Vec<String>,
}

/// Bounds the default check: a service's server may take a moment to
/// listen after it is started.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Enrolls this device on o.target and saves the host.
pub async fn login(mut o: Options) -> Result<Outcome, Error> {
    check_target(&o.target)?;
    if o.dir.as_os_str().is_empty() {
        o.dir = hosts::default_dir();
    }
    let ssh_host = o.target[o.target.rfind('@').map_or(0, |i| i + 1)..].to_string();
    if o.name.is_empty() {
        o.name = ssh_host;
    }
    if o.device.is_empty() {
        o.device = hostname();
    }
    let mut res = Outcome { device: o.device.clone(), ..Default::default() };
    let ssh = o.ssh.clone().unwrap_or_else(|| Arc::new(|t, c, i| Box::pin(async move { run_ssh(&t, &c, i).await })));

    let id = osewa_auth::Identity::load_or_create(&hosts::key_path(&o.dir)).map_err(|e| Error::Other(e.to_string()))?;
    let req = wire::marshal(&EnrollRequest { name: o.device.clone(), key: osewa_auth::encode_key(&id.public()) });
    let mut osewa = "osewa".to_string();
    if !o.no_install {
        let inst = o.install.clone().unwrap_or_else(|| Arc::new(|t, b| Box::pin(async move { install::remote(&t, b).await })));
        let bin = o.binary_for.clone().unwrap_or_else(|| Arc::new(local_binaries));
        let r = inst(o.target.clone(), bin).await?;
        res.installed = r.installed;
        res.replaced = r.replaced;
        // Non-interactive ssh often leaves ~/.local/bin off PATH.
        osewa = format!("~/{}", install::PATH);
    }
    // Enroll and leave a server running, in one ssh session (one password
    // or 2FA prompt): a daemon now, or a service that survives reboots.
    let start = if o.service { format!("{osewa} service install") } else { format!("{osewa} server --daemon") };
    let command = format!("{osewa} enroll && {start}");
    let mut stdin = req;
    stdin.push(b'\n');
    let out = ssh(o.target.clone(), command.clone(), stdin)
        .await
        .map_err(|e| Error::Other(format!("ssh {} {command}: {e}", o.target)))?;
    let resp = parse_enroll(&out)?;
    let loopback = join_host_port("127.0.0.1", &resp.port.to_string());
    let chosen_addr = !o.addr.is_empty();
    let (host, proxied) = ssh_route(o.ssh_config.as_ref(), &o.target).await;
    if !chosen_addr {
        o.addr = if o.via_ssh { loopback.clone() } else { join_host_port(&host, &resp.port.to_string()) };
    }
    res.host = Host { name: o.name.clone(), addr: o.addr.clone(), server_key: resp.server_key, ssh: o.target.clone(), quic_port: 0, via_ssh: o.via_ssh };
    // A host saved but unreachable is worse than none: clients (app-hook)
    // would take it for the way to the host and wait on it.
    if !o.no_check {
        let mut checked = check(&o, &res.host).await;
        // ssh needing a proxy to get there says a direct dial never will.
        if checked.is_err() && proxied && !o.via_ssh && !chosen_addr {
            let through = Host { addr: loopback, via_ssh: true, ..res.host.clone() };
            checked = check(&o, &through).await;
            if checked.is_ok() {
                res.host = through;
            }
        }
        if let Err(e) = checked {
            let port = o.addr.rsplit_once(':').map_or("", |(_, p)| p);
            // The port reached a server, just not the one enrolled with: on
            // a host with several users, most likely another user's.
            if e.to_string().contains("does not match pin") {
                return Err(Error::Other(format!(
                    "the osewa server on {} is running, but a different osewa server answers at {} \
                     (another user's on that host?); log in again with --addr set to this user's server, \
                     or update osewa on {0}, whose enroll picks a free port; the host was not saved",
                    o.target, o.addr
                )));
            }
            if o.via_ssh || proxied {
                return Err(Error::Other(format!(
                    "the osewa server on {} is running, but it can't be reached through ssh ({e}): connecting runs \
                     ssh with no prompts, so it needs a key or agent, for every hop, and TCP forwarding allowed on {0}; \
                     the host was not saved",
                    o.target
                )));
            }
            return Err(Error::Other(format!(
                "the osewa server on {} is running, but {} can't be reached from here ({e}): \
                 open TCP port {port} to this device (the host's firewall, a cloud security group), \
                 log in again with --addr set to an address that reaches it, \
                 or with --via-ssh to connect through ssh; the host was not saved",
                o.target, o.addr
            )));
        }
    }
    hosts::save(&o.dir, res.host.clone())?;
    for line in String::from_utf8_lossy(&out).split_inclusive('\n') {
        if line.starts_with("osewa server:") || line.starts_with("osewa service:") {
            res.server.push(line.trim_end_matches(['\r', '\n']).to_string());
        }
    }
    Ok(res)
}

async fn check(o: &Options, h: &Host) -> Result<(), Error> {
    match &o.check {
        Some(check) => check(o.dir.clone(), h.clone()).await,
        None => check_host(&o.dir, h, CHECK_TIMEOUT).await,
    }
}

fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn hostname() -> String {
    let mut b = [0u8; 256];
    if unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return String::new();
    }
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// Connects to h as this device and completes the handshake, trying again
/// every 500 ms for up to `within`: the server login just started may not
/// be listening yet. Any executor can await it.
pub async fn check_host(dir: &Path, h: &Host, within: Duration) -> Result<(), Error> {
    let cfg = h.config(dir, "osewa-login", "login")?;
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        // An ssh through a bastion can take longer than a retry's worth.
        let bound = if h.via_ssh { left } else { left.min(Duration::from_secs(3)) };
        let attempt = crate::rt::timeout(bound, async {
            let c = crate::Conn::dial(cfg.clone()).await?;
            let w = c.welcome().await;
            c.close().await;
            w.map(|_| ())
        })
        .await
        .unwrap_or_else(|| Err(Error::Io(Arc::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out")))));
        let Err(e) = attempt else { return Ok(()) };
        if Instant::now() + Duration::from_millis(500) >= deadline {
            return Err(e);
        }
        crate::rt::sleep(Duration::from_millis(500)).await;
    }
}

/// Refuses an ssh target ssh would take for an option, such as
/// -oProxyCommand=..., which runs a local command.
fn check_target(target: &str) -> Result<(), Error> {
    if target.is_empty() || target.starts_with('-') {
        return Err(Error::Other(format!("bad ssh target {}", wire::quote(target))));
    }
    Ok(())
}

/// Runs command on target with the ssh binary, stderr to ours (for
/// password and host key prompts), and returns its stdout. It runs on the
/// client's runtime, so any executor can await it.
pub async fn run_ssh(target: &str, command: &str, stdin: Vec<u8>) -> Result<Vec<u8>, Error> {
    check_target(target)?;
    let args = vec!["--".to_string(), target.to_string(), command.to_string()];
    output("ssh", args, stdin, true).await
}

async fn run_ssh_config(target: &str) -> Result<Vec<u8>, Error> {
    check_target(target)?;
    output("ssh", vec!["-G".into(), "--".into(), target.into()], Vec::new(), false).await
}

/// Runs a program and returns its stdout; a failed exit is an error.
async fn output(prog: &'static str, args: Vec<String>, stdin: Vec<u8>, stderr: bool) -> Result<Vec<u8>, Error> {
    crate::rt::run_local(move || async move {
        let mut child = tokio::process::Command::new(prog)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if stderr { Stdio::inherit() } else { Stdio::piped() })
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().expect("piped");
        let write = async move {
            let _ = input.write_all(&stdin).await;
        };
        let (_, out) = tokio::join!(write, child.wait_with_output());
        let out = out?;
        if !out.status.success() {
            return Err(Error::Other(install::exit_status(out.status)));
        }
        Ok(out.stdout)
    })
    .await
}

/// Resolves target the way ssh does (Host aliases, HostName), since the
/// server address must be dialable without ssh. Falls back to the target's
/// host part when ssh can't say.
/// The host ssh resolves target to, and whether ssh_config sends it through
/// a proxy.
async fn ssh_route(cfg: Option<&SshConfigFn>, target: &str) -> (String, bool) {
    let mut host = target[target.rfind('@').map_or(0, |i| i + 1)..].to_string();
    let out = match cfg {
        Some(cfg) => cfg(target.to_string()).await,
        None => run_ssh_config(target).await,
    };
    let Ok(out) = out else { return (host, false) };
    let mut proxied = false;
    for line in String::from_utf8_lossy(&out).lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let value = value.trim();
        match key {
            "hostname" if !value.is_empty() => host = value.to_string(),
            "proxyjump" | "proxycommand" if !value.is_empty() && value != "none" => proxied = true,
            _ => {}
        }
    }
    (host, proxied)
}

/// This machine's platform, named as the release binaries are.
pub fn local_platform() -> (&'static str, &'static str) {
    let os_name = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch_name = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        a => a,
    };
    (os_name, arch_name)
}

/// Finds an osewa build for a host: osewa-<os_name>-<arch_name> next to this
/// program or in $OSEWA_DIST, else this program itself when it is osewa
/// for the host's platform (a dev build of the CLI).
pub fn local_binaries(os_name: &str, arch_name: &str) -> Result<Vec<u8>, Error> {
    let e = match local_binaries_with(os_name, arch_name, is_osewa) {
        Ok(b) => return Ok(b),
        Err(e) => e,
    };
    // A released build can fetch its own release's build for another
    // platform: install.sh puts only this machine's on it.
    let Some(v) = crate::release::current() else { return Err(e) };
    crate::release::binary(&crate::release::fetch(), v, os_name, arch_name)
        .map_err(|r| Error::Other(format!("{e}; and downloading osewa {v} for {os_name}/{arch_name} failed: {r}")))
}

fn local_binaries_with(os_name: &str, arch_name: &str, is_osewa: fn(&Path) -> bool) -> Result<Vec<u8>, Error> {
    let name = format!("osewa-{os_name}-{arch_name}");
    let me = std::env::current_exe().ok();
    let mut dirs: Vec<PathBuf> = me.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    if let Some(d) = std::env::var_os("OSEWA_DIST").filter(|d| !d.is_empty()) {
        dirs.push(d.into());
    }
    for d in dirs {
        if let Ok(b) = std::fs::read(d.join(&name)) {
            return Ok(b);
        }
    }
    if let Some(me) = me {
        if (os_name, arch_name) == local_platform() && is_osewa(&me) {
            return Ok(std::fs::read(me)?);
        }
    }
    Err(Error::Other(format!(
        "no osewa build for {os_name}/{arch_name}: put {name} next to this program or in $OSEWA_DIST"
    )))
}

/// Only the osewa binary may install itself; another program using this
/// library (app-hook, a test binary such as osewa_client-<hash>) is not
/// osewa.
fn is_osewa(path: &Path) -> bool {
    let base = path.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
    base == "osewa" || base.starts_with("osewa-") || base.starts_with("osewa.") // osewa-<os>-<arch>, osewa.test
}

/// Finds the enroll response among whatever else the remote shell printed
/// (a motd, warnings).
pub fn parse_enroll(out: &[u8]) -> Result<EnrollResponse, Error> {
    // Lenient, as v0 peers decode: missing fields are zero.
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Resp {
        server_key: String,
        port: i64,
        version: i32,
    }
    let mut found = None;
    for line in out.split(|&c| c == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Ok(r) = wire::decode_json::<Resp>(line) {
            if !r.server_key.is_empty() {
                found = Some(r);
            }
        }
    }
    let Some(r) = found else {
        return Err(Error::Other(format!("no enroll response in {}", wire::quote(&String::from_utf8_lossy(out)))));
    };
    osewa_auth::decode_key(&r.server_key).map_err(|e| Error::Other(e.to_string()))?;
    if r.version != wire::VERSION {
        return Err(Error::Other(format!("host speaks protocol {}, want {}", r.version, wire::VERSION)));
    }
    Ok(EnrollResponse { server_key: r.server_key, port: r.port, version: r.version })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::testutil::TempDir;
    use std::sync::Mutex;

    // OSEWA_DIST is process-wide; tests that change it take turns.
    static ENV: Mutex<()> = Mutex::new(());

    fn line(r: &EnrollResponse) -> Vec<u8> {
        let mut b = serde_json::to_vec(r).unwrap();
        b.push(b'\n');
        b
    }

    fn zero_key() -> String {
        osewa_auth::encode_key(&[0u8; 32])
    }

    #[test]
    fn parse_enroll_response() {
        let key = zero_key();
        let r = |server_key: &str, port, version| EnrollResponse { server_key: server_key.into(), port, version };
        assert!(parse_enroll(&line(&r("!!", 1, 0))).is_err(), "bad key accepted");
        assert!(parse_enroll(&line(&r(&key, 1, wire::VERSION + 1))).is_err(), "wrong version accepted");
        let mut out = b"{}\nnoise\n".to_vec();
        out.extend(line(&r(&key, 5, 0)));
        assert_eq!(parse_enroll(&out).unwrap().port, 5);
    }

    #[test]
    fn binary_for() {
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dist = TempDir::new();
        unsafe { std::env::set_var("OSEWA_DIST", dist.path()) };
        let err = local_binaries("plan9", "mips").unwrap_err();
        assert!(err.to_string().contains("osewa-plan9-mips"), "{err}");
        std::fs::write(dist.path().join("osewa-plan9-mips"), "built").unwrap();
        assert_eq!(local_binaries("plan9", "mips").unwrap(), b"built");
        // The local platform falls back to this binary, if it is osewa: a
        // program that only uses this library must not install itself.
        let (os_name, arch_name) = local_platform();
        assert!(local_binaries(os_name, arch_name).is_err(), "the test binary installed itself as osewa");
        let b = local_binaries_with(os_name, arch_name, |_| true).unwrap();
        unsafe { std::env::remove_var("OSEWA_DIST") };
        assert!(!b.is_empty(), "empty");
    }

    // An ssh target that ssh would read as an option is refused.
    #[test]
    fn ssh_target_is_not_an_option() {
        for target in ["-oProxyCommand=false", "-G"] {
            let err = crate::block_on(run_ssh(target, "osewa enroll", Vec::new())).unwrap_err();
            assert!(err.to_string().contains("bad ssh target"), "run_ssh({target:?}): {err}");
            let err = crate::block_on(run_ssh_config(target)).unwrap_err();
            assert!(err.to_string().contains("bad ssh target"), "run_ssh_config({target:?}): {err}");
        }
        let dir = TempDir::new();
        let o = Options { target: "-oProxyCommand=x".into(), no_install: true, dir: dir.path().into(), ..Default::default() };
        assert!(crate::block_on(login(o)).is_err(), "login accepted an option as target");
    }

    #[test]
    fn is_osewa_names() {
        for (p, want) in [
            ("/x/osewa", true),
            ("osewa-linux-amd64", true),
            ("/b/osewa.test", true),
            ("/b/moshi-hook", false),
            ("/t/deps/osewa_client-0123abcd", false),
        ] {
            assert_eq!(is_osewa(Path::new(p)), want, "is_osewa({p:?})");
        }
    }

    fn ok<T: Send + 'static>(v: T) -> BoxFuture<Result<T, Error>> {
        Box::pin(async move { Ok(v) })
    }

    fn fail<T: Send + 'static>(msg: &str) -> BoxFuture<Result<T, Error>> {
        let e = Error::Other(msg.into());
        Box::pin(async move { Err(e) })
    }

    // Login installs, enrolls and starts the server in one ssh session,
    // then saves the host at the address ssh resolves.
    #[test]
    fn login_flow() {
        let tmp = TempDir::new();
        let dir = tmp.path().to_path_buf();
        let key = zero_key();
        let commands = Arc::new(Mutex::new(Vec::<String>::new()));
        let enrolled = Arc::new(Mutex::new(EnrollRequest::default()));
        let (cmds, enr, k) = (commands.clone(), enrolled.clone(), key.clone());
        let mut o = Options {
            target: "me@box".into(),
            dir: dir.clone(),
            device: "laptop".into(),
            service: true,
            install: Some(Arc::new(|target, _bin| {
                assert_eq!(target, "me@box", "install target");
                ok(install::Outcome { installed: true, replaced: true })
            })),
            ssh: Some(Arc::new(move |_target, command, stdin| {
                cmds.lock().unwrap().push(command);
                *enr.lock().unwrap() = serde_json::from_slice(&stdin).unwrap();
                let resp = EnrollResponse { server_key: k.clone(), port: 7400, version: 0 };
                let mut out = b"motd\n".to_vec();
                out.extend(serde_json::to_vec(&resp).unwrap());
                out.extend(b"\nosewa service: installed\r\n");
                ok(out)
            })),
            ssh_config: Some(Arc::new(|_| ok(b"user me\nhostname box.lan\n".to_vec()))),
            ..Default::default()
        };
        let checked = Arc::new(Mutex::new(Vec::<Host>::new()));
        let (ch, d) = (checked.clone(), dir.clone());
        o.check = Some(Arc::new(move |cd, h| {
            assert_eq!(cd, d, "checked with dir");
            ch.lock().unwrap().push(h);
            ok(())
        }));
        let res = crate::block_on(login(o.clone())).unwrap();
        assert_eq!(
            *commands.lock().unwrap(),
            ["~/.local/bin/osewa enroll && ~/.local/bin/osewa service install"],
            "commands"
        );
        {
            let e = enrolled.lock().unwrap();
            assert!(e.name == "laptop" && !e.key.is_empty(), "enrolled {e:?}");
        }
        let want = Host { name: "box".into(), addr: "box.lan:7400".into(), server_key: key, ssh: "me@box".into(), ..Default::default() };
        assert_eq!(
            res,
            Outcome {
                host: want.clone(),
                device: "laptop".into(),
                installed: true,
                replaced: true,
                server: vec!["osewa service: installed".into()]
            }
        );
        assert_eq!(hosts::find(&dir, "box").unwrap(), want);
        assert_eq!(*checked.lock().unwrap(), [want]);

        // Without ssh's answer the target's host is the address; no install
        // runs the osewa on PATH as a daemon.
        o.ssh_config = Some(Arc::new(|_| fail("no ssh")));
        (o.no_install, o.service, o.name, o.install) = (true, false, "b2".into(), None);
        commands.lock().unwrap().clear();
        let res = crate::block_on(login(o.clone())).unwrap();
        assert_eq!(commands.lock().unwrap()[0], "osewa enroll && osewa server --daemon");
        assert!(res.host.addr == "box:7400" && !res.installed, "result {res:?}");
        o.addr = "elsewhere:1".into();
        assert_eq!(crate::block_on(login(o.clone())).unwrap().host.addr, "elsewhere:1");

        // A server this device can't reach: nothing saved, and the error
        // says what to open.
        (o.name, o.addr) = ("closed".into(), "box.lan:7322".into());
        o.check = Some(Arc::new(|_, _| fail("connection refused")));
        let err = crate::block_on(login(o.clone())).unwrap_err().to_string();
        assert!(
            err.contains("connection refused") && err.contains("open TCP port 7322") && err.contains("--addr"),
            "{err}"
        );
        assert!(hosts::find(&dir, "closed").is_err(), "unreachable host saved");
        // #11: another user's server on the port isn't a closed port.
        o.check = Some(Arc::new(|_, _| fail("auth: server key does not match pin")));
        let err = crate::block_on(login(o.clone())).unwrap_err().to_string();
        assert!(err.contains("a different osewa server answers at box.lan:7322") && !err.contains("open TCP port"), "{err}");
        // --no-check saves it anyway.
        o.no_check = true;
        crate::block_on(login(o)).unwrap();
        hosts::find(&dir, "closed").unwrap();
    }

    #[test]
    fn login_failures() {
        let tmp = TempDir::new();
        let mut o =
            Options { target: "box".into(), dir: tmp.path().into(), no_install: true, ..Default::default() };
        o.ssh = Some(Arc::new(|_, _, _| fail("boom")));
        let err = crate::block_on(login(o.clone())).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        o.ssh = Some(Arc::new(|_, _, _| ok(b"nothing\n".to_vec())));
        assert!(crate::block_on(login(o.clone())).is_err(), "no response accepted");
        o.no_install = false;
        o.install = Some(Arc::new(|_, _| fail("no sh")));
        let err = crate::block_on(login(o)).unwrap_err();
        assert!(err.to_string().contains("no sh"), "{err}");
        assert!(crate::block_on(login(Options::default())).is_err(), "empty target");
        // A key dir that can't be used.
        let o = Options { target: "box".into(), dir: "/dev/null/x".into(), no_install: true, ..Default::default() };
        assert!(crate::block_on(login(o)).is_err(), "bad dir");
    }

    /// A login to a host whose server answers on port 7400, where the
    /// check passes only for hosts `reachable` accepts, with `ssh_g` as
    /// ssh -G's answer.
    fn route_login(dir: &Path, ssh_g: &'static str, reachable: fn(&Host) -> bool) -> Options {
        Options {
            target: "box".into(),
            dir: dir.to_path_buf(),
            device: "laptop".into(),
            no_install: true,
            ssh: Some(Arc::new(|_, _, _| {
                let resp = EnrollResponse { server_key: zero_key(), port: 7400, version: 0 };
                ok(serde_json::to_vec(&resp).unwrap())
            })),
            ssh_config: Some(Arc::new(move |_| ok(ssh_g.as_bytes().to_vec()))),
            check: Some(Arc::new(move |_, h| if reachable(&h) { ok(()) } else { fail("unreachable") })),
            ..Default::default()
        }
    }

    const PROXIED: &str = "hostname inner.lan\nproxyjump bastion\n";
    const DIRECT: &str = "hostname inner.lan\nproxyjump none\n";

    #[test]
    fn via_ssh_is_asked_for() {
        let tmp = TempDir::new();
        let res = crate::block_on(login(Options { via_ssh: true, ..route_login(tmp.path(), DIRECT, |h| h.via_ssh) })).unwrap();
        assert!(res.host.via_ssh && res.host.addr == "127.0.0.1:7400", "{:?}", res.host);
        // --addr is then the server as the host sees it.
        let o = Options { via_ssh: true, addr: "10.0.0.5:7322".into(), ..route_login(tmp.path(), DIRECT, |h| h.via_ssh) };
        assert_eq!(crate::block_on(login(o)).unwrap().host.addr, "10.0.0.5:7322");
    }

    /// Behind a proxy a direct dial still goes first, since one that works
    /// keeps QUIC, and failing it falls back to ssh.
    #[test]
    fn a_proxied_target_falls_back_to_ssh() {
        let tmp = TempDir::new();
        let res = crate::block_on(login(route_login(tmp.path(), PROXIED, |h| h.via_ssh))).unwrap();
        assert!(res.host.via_ssh && res.host.addr == "127.0.0.1:7400", "{:?}", res.host);
        assert!(hosts::find(tmp.path(), "box").unwrap().via_ssh, "saved without via_ssh");

        let res = crate::block_on(login(route_login(tmp.path(), PROXIED, |h| !h.via_ssh))).unwrap();
        assert!(!res.host.via_ssh && res.host.addr == "inner.lan:7400", "{:?}", res.host);
    }

    #[test]
    fn nothing_else_falls_back_to_ssh() {
        let tmp = TempDir::new();
        // ssh goes straight there: a closed port is the likely reason.
        let e = crate::block_on(login(route_login(tmp.path(), DIRECT, |h| h.via_ssh))).unwrap_err().to_string();
        assert!(e.contains("open TCP port 7400") && e.contains("--via-ssh"), "{e}");
        // The address was chosen: it isn't ours to replace.
        let o = Options { addr: "chosen:1".into(), ..route_login(tmp.path(), PROXIED, |h| h.via_ssh) };
        assert!(crate::block_on(login(o)).is_err(), "--addr fell back");
        // Neither way works.
        let e = crate::block_on(login(route_login(tmp.path(), PROXIED, |_| false))).unwrap_err().to_string();
        assert!(e.contains("through ssh") && e.contains("key or agent"), "{e}");
        assert!(hosts::find(tmp.path(), "box").is_err(), "an unreachable host was saved");
    }

    #[test]
    fn ssh_hostnames() {
        let cfg = |out: &'static str| -> SshConfigFn { Arc::new(move |_| ok(out.as_bytes().to_vec())) };
        let route = |out, target| crate::block_on(ssh_route(Some(&cfg(out)), target));
        assert_eq!(route("hostname \n", "u@alias"), ("alias".to_string(), false));
        assert_eq!(route("port 22\nhostname real\n", "alias"), ("real".to_string(), false));
        assert!(!route("hostname real\nproxyjump none\nproxycommand none\n", "a").1);
        assert!(route("hostname real\nproxyjump bastion\n", "a").1);
        assert!(route("hostname real\nproxycommand nc %h %p\n", "a").1);
    }
}
