//! The account sessions run as, and the environment they start with: built
//! from the account (passwd), as sshd does, never inherited from whatever
//! started the server. A server started from launchd has a bare PATH; one
//! started from a terminal, multiplexer or agent carries that program's
//! variables (its nesting guards, its tokens, its $SHELL and cwd), and
//! none of it belongs in a session.

use std::ffi::{CStr, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The account this process runs as, from passwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    /// pw_dir as the system has it: a path need not be UTF-8.
    pub home: PathBuf,
    /// The login shell; /bin/sh when passwd names none.
    pub shell: String,
}

/// The PATH a session starts with, before the login shell's profile
/// extends it (ssh's default, give or take).
const BASE_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// Homebrew on Apple silicon, put first as `brew shellenv` does, when it
/// is installed: a non-POSIX login shell (fish) runs no path_helper.
#[cfg(target_os = "macos")]
const HOMEBREW: &[&str] = &["/opt/homebrew/bin", "/opt/homebrew/sbin"];
#[cfg(not(target_os = "macos"))]
const HOMEBREW: &[&str] = &[];

/// macOS's login locale when nobody chose one: launchd gives a server
/// none, and zsh and others mishandle multibyte input without it.
#[cfg(target_os = "macos")]
const DEFAULT_LANG: Option<&str> = Some("en_US.UTF-8");
#[cfg(not(target_os = "macos"))]
const DEFAULT_LANG: Option<&str> = None;

/// What passes through from the server's own environment: the locale and
/// time zone (as ssh's AcceptEnv LANG LC_* commonly does), and the
/// system's per-login settings.
fn passes(k: &str) -> bool {
    matches!(k, "LANG" | "LANGUAGE" | "TZ")
        || k.starts_with("LC_")
        || (cfg!(target_os = "linux") && k == "XDG_RUNTIME_DIR")
        || (cfg!(target_os = "macos") && k == "__CF_USER_TEXT_ENCODING")
}

/// Overrides the account's home (pw_dir) for sessions' HOME and default
/// cwd: for tests and sandboxes, so what a session runs never touches the
/// real home. Read once by the server and the holder at start (absolute
/// paths only), never by sessions, which don't get it. Nothing else of the
/// account can be overridden.
pub const HOME_OVERRIDE: &str = "OSEWA_HOME";

/// Client env entries with this prefix are dropped: osewa's own settings
/// are not a client's to give a session.
pub const RESERVED_PREFIX: &str = "OSEWA_";

/// The account, with home in place of pw_dir when given (OSEWA_HOME, as
/// home_override read it).
pub fn account(home: Option<&Path>) -> Account {
    let mut a = passwd_account();
    if let Some(h) = home {
        a.home = h.to_path_buf();
    }
    a
}

/// OSEWA_HOME from this process's environment: None when unset, an error
/// when set but not an absolute path.
pub fn home_override() -> Result<Option<PathBuf>, String> {
    match std::env::var_os(HOME_OVERRIDE) {
        None => Ok(None),
        Some(h) if Path::new(&h).is_absolute() => Ok(Some(h.into())),
        Some(h) => Err(format!("{HOME_OVERRIDE}={:?}: want an absolute path", Path::new(&h))),
    }
}

/// Set to 1 (cargo sets it for every test run, .cargo/config.toml), a
/// server, holder or exec refuses to start unless its sessions' home is a
/// sandbox: given (OSEWA_HOME, Config.home) and not the passwd home. A
/// harness that forgets sandboxing then fails instead of writing to the
/// real home. For tests only: it can only refuse.
pub const REQUIRE_SANDBOX: &str = "OSEWA_REQUIRE_SANDBOX";

/// Checks home against OSEWA_REQUIRE_SANDBOX, and that it is absolute.
pub fn check_home(home: Option<&Path>) -> Result<(), String> {
    if let Some(h) = home.filter(|h| !h.is_absolute()) {
        return Err(format!("sessions' home {:?}: want an absolute path", h));
    }
    if std::env::var_os(REQUIRE_SANDBOX).is_some_and(|v| v == "1") {
        match home {
            None => return Err(format!("{REQUIRE_SANDBOX}=1 and no sandbox home: set {HOME_OVERRIDE} (osewa_paths::sandbox) or Config.home")),
            Some(h) if h == passwd_account().home => return Err(format!("{REQUIRE_SANDBOX}=1 and the sessions' home {} is the real one", h.display())),
            Some(_) => {}
        }
    }
    Ok(())
}

/// A fresh private directory for this test process's sessions to use as
/// home, made once. Test harnesses hand it to every server, holder and
/// exec they start (Config.home in process, sandbox for a process), so
/// none sees the real home. Not for servers.
pub fn sandbox_home() -> PathBuf {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("osewa-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("osewa: sandbox home");
        std::fs::canonicalize(&d).expect("osewa: sandbox home")
    })
    .clone()
}

/// Gives a server or holder process a test starts the sandbox home. Every
/// test harness that starts one must (paths' tests check the workspace).
pub fn sandbox(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd.env(HOME_OVERRIDE, sandbox_home())
}

/// The most getpwuid_r gets to fill.
const PASSWD_BUF_MAX: usize = 1 << 20;

/// The passwd entry: None when there is none (a container's arbitrary
/// uid), an error when the lookup itself failed.
fn passwd() -> io::Result<Option<(String, PathBuf, String)>> {
    let mut size = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n > 0 => (n as usize).min(PASSWD_BUF_MAX),
        _ => 16 << 10,
    };
    loop {
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; size];
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let r = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
        if r == libc::ERANGE && size < PASSWD_BUF_MAX {
            size = (size * 4).min(PASSWD_BUF_MAX);
            continue;
        }
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        if out.is_null() {
            return Ok(None);
        }
        let bytes = |p: *const libc::c_char| if p.is_null() { &[][..] } else { unsafe { CStr::from_ptr(p) }.to_bytes() };
        let s = |p| String::from_utf8_lossy(bytes(p)).into_owned();
        return Ok(Some((s(pw.pw_name), PathBuf::from(OsStr::from_bytes(bytes(pw.pw_dir))), s(pw.pw_shell))));
    }
}

/// The account as passwd has it, OSEWA_HOME aside. Without an entry, or
/// when the lookup fails (said on stderr), what the environment says,
/// else / and /bin/sh.
pub fn passwd_account() -> Account {
    let (name, home, shell) = match passwd() {
        Ok(Some(e)) => e,
        Ok(None) => Default::default(),
        Err(e) => {
            eprintln!("osewa: passwd lookup for uid {}: {e}", unsafe { libc::getuid() });
            Default::default()
        }
    };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    Account {
        name: Some(name).filter(|n| !n.is_empty()).or_else(|| env("USER")).or_else(|| env("LOGNAME")).unwrap_or_default(),
        home: Some(home)
            .filter(|h| !h.as_os_str().is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from))
            .unwrap_or_else(|| "/".into()),
        shell: Some(shell).filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into()),
    }
}

impl Account {
    /// The command a session runs when the client names none: the login
    /// shell, as a login shell.
    pub fn login_shell(&self) -> Vec<String> {
        vec![self.shell.clone(), "-l".into()]
    }
}

/// The environment a session's process starts with: the account's (HOME,
/// USER, LOGNAME, SHELL, a base PATH, TMPDIR on macOS), the locale and
/// time zone from this process, then fixed (the holder's TERM), then the
/// client's `KEY=value` entries, which win, but for OSEWA_* ones. On macOS
/// LANG defaults to en_US.UTF-8 when nothing chose a locale. Nothing else
/// of this process's environment gets through.
pub fn session_env(acct: &Account, fixed: &[(&str, &str)], client: &[String]) -> Vec<(OsString, OsString)> {
    // By name first: a variable that won't pass may not even be UTF-8.
    let ours = std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(|k| passes(k) || k == "TMPDIR"));
    session_env_from(acct, ours, darwin_temp_dir(), |p| p.is_dir(), fixed, client)
}

/// session_env with its inputs given, for tests.
pub fn session_env_from(
    acct: &Account,
    ours: impl IntoIterator<Item = (OsString, OsString)>,
    darwin_tmp: Option<OsString>,
    exists: impl Fn(&Path) -> bool,
    fixed: &[(&str, &str)],
    client: &[String],
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = Vec::new();
    let mut set = |k: &str, v: &OsStr| match env.iter_mut().find(|(n, _)| n == k) {
        Some(e) => e.1 = v.to_os_string(),
        None => env.push((k.into(), v.to_os_string())),
    };
    set("HOME", acct.home.as_os_str());
    set("USER", acct.name.as_ref());
    set("LOGNAME", acct.name.as_ref());
    set("SHELL", acct.shell.as_ref());
    let mut path: Vec<&str> = HOMEBREW.iter().copied().filter(|d| exists(Path::new(d))).collect();
    path.push(BASE_PATH);
    set("PATH", path.join(":").as_ref());
    let mut tmp = darwin_tmp;
    let mut ours: Vec<(OsString, OsString)> = ours.into_iter().collect();
    ours.sort();
    let mut locale = false;
    for (k, v) in &ours {
        let Some(k) = k.to_str() else { continue };
        if v.is_empty() {
            continue;
        }
        if passes(k) {
            set(k, v);
            locale |= sets_locale(k);
        }
        // macOS's per-user temp dir, when confstr has none for us.
        if cfg!(target_os = "macos") && k == "TMPDIR" && tmp.is_none() {
            tmp = Some(v.clone());
        }
    }
    if let Some(t) = tmp.filter(|_| cfg!(target_os = "macos")) {
        set("TMPDIR", &t);
    }
    for (k, v) in fixed {
        set(k, v.as_ref());
    }
    for kv in client {
        if let Some((k, v)) = kv.split_once('=').filter(|(k, _)| !k.is_empty() && !k.starts_with(RESERVED_PREFIX)) {
            set(k, v.as_ref());
            locale |= sets_locale(k);
        }
    }
    if let (false, Some(lang)) = (locale, DEFAULT_LANG) {
        set("LANG", lang.as_ref());
    }
    env
}

/// Whether k chooses the character set: then no LANG is made up.
fn sets_locale(k: &str) -> bool {
    matches!(k, "LANG" | "LC_ALL" | "LC_CTYPE")
}

/// macOS's per-user temporary directory, as launchd gives a login.
#[cfg(target_os = "macos")]
fn darwin_temp_dir() -> Option<OsString> {
    let mut buf = vec![0 as libc::c_char; 1024];
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    let b = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_bytes();
    (!b.is_empty()).then(|| OsStr::from_bytes(b).to_os_string())
}

#[cfg(not(target_os = "macos"))]
fn darwin_temp_dir() -> Option<OsString> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> Account {
        Account { name: "jo".into(), home: "/home/jo".into(), shell: "/usr/bin/fish".into() }
    }

    fn get<'a>(env: &'a [(OsString, OsString)], k: &str) -> Option<&'a str> {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.to_str().unwrap())
    }

    fn os(kv: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        kv.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }

    /// Nothing of the server's environment gets through but the locale,
    /// the time zone and the system's per-login settings; the account's
    /// comes from passwd; the client's wins, but for OSEWA_*.
    #[test]
    fn session_env_is_the_accounts() {
        let ours = os(&[
            ("HERDR_ENV", "1"),
            ("FOO", "bar"),
            ("SHELL", "/bin/false"),
            ("HOME", "/tmp/elsewhere"),
            ("PATH", "/junk"),
            ("CLAUDE_CODE_TOKEN", "secret"),
            ("OSEWA_HOME", "/tmp/h"),
            ("LANG", "en_US.UTF-8"),
            ("LC_CTYPE", "UTF-8"),
            ("LANGUAGE", "en"),
            ("TZ", "Asia/Bangkok"),
            ("__CF_USER_TEXT_ENCODING", "0x1F5:0x0:0x0"),
            ("TMPDIR", "/var/folders/x/T/"),
            ("XDG_RUNTIME_DIR", "/run/user/501"),
        ]);
        let client = ["LANG=ja_JP.UTF-8".to_string(), "EDITOR=vi".into(), "junk".into(), "=x".into(), "OSEWA_HOME=/tmp/x".into(), "OSEWA_TRACE=/tmp".into()];
        let env = session_env_from(&acct(), ours, None, |_| true, &[("TERM", "xterm-256color")], &client);
        for gone in ["HERDR_ENV", "FOO", "CLAUDE_CODE_TOKEN", "OSEWA_HOME", "OSEWA_TRACE"] {
            assert_eq!(get(&env, gone), None, "{gone} leaked: {env:?}");
        }
        assert_eq!(get(&env, "HOME"), Some("/home/jo"));
        assert_eq!(get(&env, "USER"), Some("jo"));
        assert_eq!(get(&env, "LOGNAME"), Some("jo"));
        assert_eq!(get(&env, "SHELL"), Some("/usr/bin/fish"));
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "LANG"), Some("ja_JP.UTF-8"), "the client's wins");
        assert_eq!(get(&env, "LC_CTYPE"), Some("UTF-8"));
        assert_eq!(get(&env, "LANGUAGE"), Some("en"));
        assert_eq!(get(&env, "TZ"), Some("Asia/Bangkok"));
        assert_eq!(get(&env, "EDITOR"), Some("vi"));
        let path = get(&env, "PATH").unwrap();
        assert!(path.ends_with(BASE_PATH) && !path.contains("/junk"), "PATH {path}");
        if cfg!(target_os = "macos") {
            assert!(path.starts_with("/opt/homebrew/bin:/opt/homebrew/sbin:"), "PATH {path}");
            assert_eq!(get(&env, "TMPDIR"), Some("/var/folders/x/T/"), "TMPDIR falls back to ours");
            assert_eq!(get(&env, "__CF_USER_TEXT_ENCODING"), Some("0x1F5:0x0:0x0"));
            assert_eq!(get(&env, "XDG_RUNTIME_DIR"), None);
        } else {
            assert_eq!(get(&env, "TMPDIR"), None);
            assert_eq!(get(&env, "__CF_USER_TEXT_ENCODING"), None);
            assert_eq!(get(&env, "XDG_RUNTIME_DIR"), Some("/run/user/501"));
        }
        assert_eq!(env.iter().filter(|(k, _)| k == "LANG").count(), 1, "LANG twice");
    }

    /// With no locale chosen anywhere, macOS gets en_US.UTF-8; a choice by
    /// the server or the client (LANG, LC_ALL or LC_CTYPE) is kept.
    #[test]
    fn default_lang() {
        let env = session_env_from(&acct(), [], None, |_| false, &[], &[]);
        assert_eq!(get(&env, "PATH"), Some(BASE_PATH), "Homebrew only when installed");
        assert_eq!(get(&env, "LANG"), DEFAULT_LANG);
        let env = session_env_from(&acct(), os(&[("LC_ALL", "C")]), None, |_| false, &[], &[]);
        assert_eq!((get(&env, "LANG"), get(&env, "LC_ALL")), (None, Some("C")));
        let env = session_env_from(&acct(), [], None, |_| false, &[], &["LC_CTYPE=C.UTF-8".into()]);
        assert_eq!((get(&env, "LANG"), get(&env, "LC_CTYPE")), (None, Some("C.UTF-8")));
    }

    /// A variable that won't pass may be anything, UTF-8 or not; a home
    /// that isn't UTF-8 is kept as it is.
    #[test]
    fn not_utf8() {
        let bad = OsStr::from_bytes(b"\xff\xfe").to_os_string();
        let a = Account { home: PathBuf::from(OsStr::from_bytes(b"/home/\xff")), ..acct() };
        let env = session_env_from(&a, vec![(bad.clone(), bad.clone()), ("JUNK".into(), bad)], None, |_| false, &[], &[]);
        let home = env.iter().find(|(k, _)| k == "HOME").unwrap();
        assert_eq!(home.1.as_bytes(), b"/home/\xff");
        assert_eq!(env.len(), if DEFAULT_LANG.is_some() { 6 } else { 5 }, "{env:?}");
    }

    #[test]
    fn home_override_and_sandbox() {
        let real = passwd_account();
        let home = sandbox_home();
        let a = account(Some(&home));
        assert!(a.home == home && a.name == real.name && a.shell == real.shell, "{a:?}");
        assert_ne!(a.home, real.home);
        assert_eq!(account(None), real);
        let mut cmd = std::process::Command::new("true");
        sandbox(&mut cmd);
        assert!(cmd.get_envs().any(|(k, v)| k == HOME_OVERRIDE && v == Some(home.as_os_str())));
    }

    #[test]
    fn this_account() {
        let a = passwd_account();
        assert!(!a.name.is_empty() && a.home.is_absolute() && a.shell.starts_with('/'), "{a:?}");
        assert_eq!(a.login_shell(), [a.shell.clone(), "-l".into()]);
        if cfg!(target_os = "macos") {
            assert!(darwin_temp_dir().is_some_and(|t| t.as_bytes().starts_with(b"/")), "no per-user temp dir");
        }
    }
}
