//! Updates from osewa's GitHub releases, as moshi-hook's do.
//!
//! `osewa set auto-update auto|ask|off` picks the mode; ask is the
//! default, because an update runs what it downloads: on a remote-access
//! tool, a compromised release shouldn't reach every machine unattended.
//! `osewa update` (no host) installs the latest release over this binary.
//! A released build checks for a newer release at most once a day, after
//! a client command, in a background process; a server checks hourly.
//!
//! Auto installs on its own only a release that interoperates with this
//! build (the same protocol and snapshot version): a server moving to
//! another one would strand every session it holds and lock out clients
//! still on this one. Such a release, and any release in ask mode, is
//! announced instead: after client commands, and in the server log.
//! Development builds (no OSEWA_VERSION) never check on their own.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use osewa_client::release::{self, Release};

use crate::cli::{Env, Out, Result};

/// How often a client checks.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// A server's first check waits out its start, so one that crash-loops
/// doesn't hammer GitHub.
const SERVER_FIRST: Duration = Duration::from_secs(120);
const SERVER_EVERY: Duration = Duration::from_secs(3600);
/// Spreads hosts that started together apart.
const SERVER_JITTER_SECS: u64 = 600;
/// How often a server due to restart looks again for open forwards and
/// execs, which a restart would end.
const IDLE_POLL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Auto,
    Ask,
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "auto" => Some(Mode::Auto),
            "ask" => Some(Mode::Ask),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Ask => "ask",
            Mode::Off => "off",
        }
    }
}

fn settings_path() -> PathBuf {
    osewa_paths::config_dir().join("settings.json")
}

fn read_json(p: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Written whole beside p, then renamed over it.
fn write_json(p: &Path, v: &serde_json::Map<String, serde_json::Value>) -> io::Result<()> {
    let dir = p.parent().ok_or_else(|| io::Error::other("no parent directory"))?;
    osewa_paths::ensure(dir)?;
    let tmp = dir.join(format!(".{}.{}", p.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(v).map_err(io::Error::other)?)?;
        std::fs::rename(&tmp, p)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// $OSEWA_AUTO_UPDATE, else the setting, else ask.
pub(crate) fn mode() -> Mode {
    if let Some(m) = std::env::var("OSEWA_AUTO_UPDATE").ok().and_then(|v| Mode::parse(&v)) {
        return m;
    }
    read_json(&settings_path()).get("auto_update").and_then(|v| v.as_str()).and_then(Mode::parse).unwrap_or(Mode::Ask)
}

const SET_USAGE: &str = "usage: osewa set [auto-update [auto|ask|off]]";

/// `osewa set [auto-update [auto|ask|off]]`: shows or changes a setting.
pub(crate) fn set_main(args: &[String], mut out: Out, mut err: Out) -> i32 {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        [] | ["auto-update"] => {
            let _ = writeln!(out, "auto-update {}", mode().name());
            0
        }
        ["auto-update", m] => {
            let Some(m) = Mode::parse(m) else {
                let _ = writeln!(err, "{SET_USAGE}");
                return 2;
            };
            let p = settings_path();
            let mut v = read_json(&p);
            v.insert("auto_update".into(), m.name().into());
            if let Err(e) = write_json(&p, &v) {
                let _ = writeln!(err, "osewa set: {}: {e}", p.display());
                return 1;
            }
            let _ = writeln!(out, "auto-update {}", m.name());
            0
        }
        _ => {
            let _ = writeln!(err, "{SET_USAGE}");
            2
        }
    }
}

/// What the last check found.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Cache {
    /// Unix seconds of the last check started.
    pub checked: u64,
    pub latest: Option<Release>,
}

fn cache_path() -> PathBuf {
    crate::daemon::state_dir().join("update.json")
}

pub(crate) fn load_cache() -> Cache {
    std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_cache(c: &Cache) -> io::Result<()> {
    let v = serde_json::to_value(c).map_err(io::Error::other)?;
    crate::daemon::ensure_state_dir()?;
    write_json(&cache_path(), v.as_object().expect("Cache is an object"))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The line to print after a client command, given what the last check
/// found; None to stay quiet.
pub(crate) fn notice(current: Option<&str>, mode: Mode, latest: &Release) -> Option<String> {
    let cur = current?;
    if mode == Mode::Off || !latest.newer_than(Some(cur)) {
        return None;
    }
    if !latest.compatible() {
        return Some(format!(
            "osewa {} is out (this is {cur}). It can't talk to osewa {cur}, so update this machine and your hosts together: osewa update, then osewa update <host>",
            latest.osewa
        ));
    }
    // Auto installs it in the background.
    (mode == Mode::Ask).then(|| format!("osewa {} is out (this is {cur}): osewa update", latest.osewa))
}

/// After a client command: the notice, and a background check when one
/// is due. Quiet unless stderr is a terminal (Env::notices).
pub(crate) fn after_command(env: &Env) {
    if !env.notices {
        return;
    }
    let Some(cur) = release::current() else { return };
    let mode = mode();
    if mode == Mode::Off {
        return;
    }
    let mut c = load_cache();
    if let Some(n) = c.latest.as_ref().and_then(|l| notice(Some(cur), mode, l)) {
        let _ = writeln!(env.err(), "{n}");
    }
    if now().saturating_sub(c.checked) >= CHECK_EVERY.as_secs() {
        // Stamped before it runs: a check that fails waits a day too.
        c.checked = now();
        if save_cache(&c).is_ok() {
            spawn_background();
        }
    }
}

/// `osewa update --background`, detached: this command doesn't wait for
/// the network.
fn spawn_background() {
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = Command::new(exe);
    cmd.args(["update", "--background"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// This program's path, symlinks resolved: what an update replaces.
fn exe() -> Result<PathBuf> {
    let e = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&e).unwrap_or(e))
}

/// The version the binary at path reports, by running it.
fn installed_version(path: &Path) -> Option<String> {
    let out = Command::new(path).args(["version", "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("osewa")?.as_str().map(str::to_string)
}

/// Puts bin, osewa `version`, at path: written beside it, run once to see
/// that it is that version and runs here, then renamed over it, so nothing
/// ever runs half of one.
pub(crate) fn install(path: &Path, bin: &[u8], version: &str) -> Result<()> {
    let dir = path.parent().ok_or("no directory to install into")?;
    let tmp = dir.join(format!(".osewa.update.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let r = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp)
            .map_err(|e| format!("can't write to {}: {e}", dir.display()))?;
        f.write_all(bin)?;
        drop(f);
        match installed_version(&tmp) {
            Some(v) if v == version => {}
            got => return Err(format!("the downloaded osewa reports {got:?}, not {version}").into()),
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("replacing {}: {e}", path.display()))?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Downloads and installs latest over this program, unless that is what
/// is installed there already (another osewa got to it first).
fn install_latest(latest: &Release, fetch: &release::Fetch) -> Result<(PathBuf, bool)> {
    let path = exe()?;
    if installed_version(&path).as_deref() == Some(latest.osewa.as_str()) {
        return Ok((path, false));
    }
    let (os, arch) = osewa_client::login::local_platform();
    let bin = release::binary(fetch, &latest.osewa, os, arch)?;
    install(&path, &bin, &latest.osewa)?;
    Ok((path, true))
}

/// `osewa update` with no host, or `--background` from after_command.
pub(crate) fn update_self(env: &Env, background: bool) -> Result<()> {
    let fetch = release::fetch();
    let latest = release::latest(&fetch)?;
    let _ = save_cache(&Cache { checked: now(), latest: Some(latest.clone()) });
    let cur = release::current();
    if background {
        // Only what auto may install on its own; after_command announces
        // the rest.
        if mode() != Mode::Auto || cur.is_none() || !latest.compatible() || !latest.newer_than(cur) {
            return Ok(());
        }
        install_latest(&latest, &fetch)?;
        // The local server's own check restarts it.
        return Ok(());
    }
    let mut out = env.out();
    if !latest.newer_than(cur) {
        writeln!(out, "osewa {} is the latest release", cur.unwrap_or("?"))?;
        return Ok(());
    }
    let (path, _) = install_latest(&latest, &fetch)?;
    writeln!(out, "installed osewa {} at {} (was {})", latest.osewa, path.display(), cur.unwrap_or("a development build"))?;
    if crate::daemon::running_server().is_none() {
        return Ok(());
    }
    if !latest.compatible() {
        writeln!(
            out,
            "it can't attach sessions the server here started before it, so the server keeps the old build until you restart it: osewa service restart"
        )?;
        return Ok(());
    }
    let st = Command::new(&path).args(["service", "restart"]).stdin(Stdio::null()).status()?;
    if !st.success() {
        return Err("restarting the server here failed; see above".into());
    }
    Ok(())
}

/// Starts a released server's hourly check, unless it runs in a terminal
/// (by hand: a restart would turn it into a daemon) or for a test.
pub(crate) fn spawn_server_updater(stderr: &mut impl Write) {
    let Some(cur) = release::current() else { return };
    if matches!(osewa_paths::home_override(), Ok(Some(_)) | Err(_)) {
        return;
    }
    if crate::tty::is_terminal(2) {
        let _ = writeln!(stderr, "osewa server: not updating itself, as it runs in a terminal; osewa update does it by hand");
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(SERVER_FIRST);
        let mut told = String::new();
        loop {
            if mode() != Mode::Off {
                if let Err(e) = server_check(cur, &mut told) {
                    eprintln!("osewa server: update check: {e}");
                }
            }
            let jitter = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0) % SERVER_JITTER_SECS;
            std::thread::sleep(SERVER_EVERY + Duration::from_secs(jitter));
        }
    });
}

/// One server check: installs a compatible newer release in auto mode,
/// then restarts into it once no forward or exec is open; announces
/// anything else once per release.
fn server_check(cur: &str, told: &mut String) -> Result<()> {
    let fetch = release::fetch();
    let latest = release::latest(&fetch)?;
    let _ = save_cache(&Cache { checked: now(), latest: Some(latest.clone()) });
    if !latest.newer_than(Some(cur)) {
        return Ok(());
    }
    let mode = mode();
    if !latest.compatible() || mode != Mode::Auto {
        if *told != latest.osewa {
            let why = if latest.compatible() { "auto-update is ask" } else { "it can't attach this server's sessions" };
            eprintln!("osewa server: osewa {} is out ({why}); install it with osewa update", latest.osewa);
            *told = latest.osewa.clone();
        }
        return Ok(());
    }
    let (path, fresh) = install_latest(&latest, &fetch)?;
    if fresh {
        eprintln!("osewa server: installed osewa {} at {}", latest.osewa, path.display());
    }
    while osewa_server::open_channels() > 0 {
        std::thread::sleep(IDLE_POLL);
    }
    eprintln!("osewa server: restarting into osewa {}", latest.osewa);
    // From a process of its own session: this one is what it stops.
    let st = Command::new(&path).args(["service", "restart", "--detach"]).stdin(Stdio::null()).status()?;
    if !st.success() {
        return Err(format!("service restart --detach: {st}").into());
    }
    Ok(())
}
