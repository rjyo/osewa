//! The account sessions run as, and the environment they start with: built
//! from the account (passwd), as sshd does, never inherited from whatever
//! started the server. A server started from launchd has a bare PATH; one
//! started from a terminal, multiplexer or agent carries that program's
//! variables (its nesting guards, its tokens, its $SHELL and cwd), and
//! none of it belongs in a session.

use std::ffi::CStr;
use std::path::{Path, PathBuf};

/// The account this process runs as, from passwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
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

/// What passes through from the server's own environment: the locale (as
/// ssh's AcceptEnv LANG LC_* commonly does), and where the system put this
/// account's per-login directories.
fn passes(k: &str) -> bool {
    k == "LANG" || k.starts_with("LC_") || (cfg!(target_os = "linux") && k == "XDG_RUNTIME_DIR")
}

/// Overrides the account's home (pw_dir) for sessions' HOME and default
/// cwd: for tests and sandboxes, so what a session runs never touches the
/// real home. Nothing else of the account can be overridden.
pub const HOME_OVERRIDE: &str = "OSEWA_HOME";

/// This process's account, with OSEWA_HOME as its home when set. Without a
/// passwd entry (a container's arbitrary uid), what the environment says,
/// else root-ish defaults.
pub fn account() -> Account {
    let mut a = passwd_account();
    if let Some(h) = std::env::var_os(HOME_OVERRIDE).filter(|h| !h.is_empty()) {
        a.home = h.into();
    }
    a
}

/// Points OSEWA_HOME at a fresh private directory for the rest of this
/// process, once, and returns it: test harnesses call it before starting
/// any session, so none sees the real home. Not for servers.
pub fn sandbox_home() -> PathBuf {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("osewa-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("osewa: sandbox home");
        let d = std::fs::canonicalize(&d).expect("osewa: sandbox home");
        unsafe { std::env::set_var(HOME_OVERRIDE, &d) };
        d
    })
    .clone()
}

/// The account as passwd has it, OSEWA_HOME aside.
pub fn passwd_account() -> Account {
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 << 10];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    let s = |p: *const libc::c_char| if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() };
    let (name, home, shell) = if r == 0 && !out.is_null() { (s(pw.pw_name), s(pw.pw_dir), s(pw.pw_shell)) } else { Default::default() };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    Account {
        name: Some(name).filter(|n| !n.is_empty()).or_else(|| env("USER")).or_else(|| env("LOGNAME")).unwrap_or_default(),
        home: Some(home).filter(|h| !h.is_empty()).or_else(|| env("HOME")).unwrap_or_else(|| "/".into()).into(),
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
/// USER, LOGNAME, SHELL, a base PATH, TMPDIR on macOS), the locale from
/// this process, then fixed (the holder's TERM), then the client's
/// `KEY=value` entries, which win. Nothing else of this process's
/// environment gets through.
pub fn session_env(acct: &Account, fixed: &[(&str, &str)], client: &[String]) -> Vec<(String, String)> {
    session_env_from(acct, std::env::vars(), darwin_temp_dir(), |p| p.is_dir(), fixed, client)
}

/// session_env with its inputs given, for tests.
pub fn session_env_from(
    acct: &Account,
    ours: impl IntoIterator<Item = (String, String)>,
    darwin_tmp: Option<String>,
    exists: impl Fn(&Path) -> bool,
    fixed: &[(&str, &str)],
    client: &[String],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: &str| match env.iter_mut().find(|(n, _)| n == k) {
        Some(e) => e.1 = v.to_string(),
        None => env.push((k.to_string(), v.to_string())),
    };
    let home = acct.home.to_string_lossy();
    set("HOME", &home);
    set("USER", &acct.name);
    set("LOGNAME", &acct.name);
    set("SHELL", &acct.shell);
    let mut path: Vec<&str> = HOMEBREW.iter().copied().filter(|d| exists(Path::new(d))).collect();
    path.push(BASE_PATH);
    set("PATH", &path.join(":"));
    let mut tmp = darwin_tmp;
    let mut ours: Vec<(String, String)> = ours.into_iter().collect();
    ours.sort();
    for (k, v) in &ours {
        if passes(k) && !v.is_empty() {
            set(k, v);
        }
        // macOS's per-user temp dir, when confstr has none for us.
        if cfg!(target_os = "macos") && k == "TMPDIR" && tmp.is_none() && !v.is_empty() {
            tmp = Some(v.clone());
        }
    }
    if let Some(t) = tmp.filter(|_| cfg!(target_os = "macos")) {
        set("TMPDIR", &t);
    }
    for (k, v) in fixed {
        set(k, v);
    }
    for kv in client {
        if let Some((k, v)) = kv.split_once('=').filter(|(k, _)| !k.is_empty()) {
            set(k, v);
        }
    }
    env
}

/// macOS's per-user temporary directory, as launchd gives a login.
#[cfg(target_os = "macos")]
fn darwin_temp_dir() -> Option<String> {
    let mut buf = vec![0 as libc::c_char; 1024];
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
    if n == 0 || n > buf.len() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()).filter(|s| !s.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn darwin_temp_dir() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> Account {
        Account { name: "jo".into(), home: "/home/jo".into(), shell: "/usr/bin/fish".into() }
    }

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// Nothing of the server's environment gets through but the locale;
    /// the account's comes from passwd; the client's wins.
    #[test]
    fn session_env_is_the_accounts() {
        let ours = [
            ("HERDR_ENV", "1"),
            ("FOO", "bar"),
            ("SHELL", "/bin/false"),
            ("HOME", "/tmp/elsewhere"),
            ("PATH", "/junk"),
            ("CLAUDE_CODE_TOKEN", "secret"),
            ("LANG", "en_US.UTF-8"),
            ("LC_CTYPE", "UTF-8"),
            ("TMPDIR", "/var/folders/x/T/"),
            ("XDG_RUNTIME_DIR", "/run/user/501"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let client = ["LANG=ja_JP.UTF-8".to_string(), "EDITOR=vi".into(), "junk".into(), "=x".into()];
        let env = session_env_from(&acct(), ours, None, |_| true, &[("TERM", "xterm-256color")], &client);
        for gone in ["HERDR_ENV", "FOO", "CLAUDE_CODE_TOKEN"] {
            assert_eq!(get(&env, gone), None, "{gone} leaked: {env:?}");
        }
        assert_eq!(get(&env, "HOME"), Some("/home/jo"));
        assert_eq!(get(&env, "USER"), Some("jo"));
        assert_eq!(get(&env, "LOGNAME"), Some("jo"));
        assert_eq!(get(&env, "SHELL"), Some("/usr/bin/fish"));
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "LANG"), Some("ja_JP.UTF-8"), "the client's wins");
        assert_eq!(get(&env, "LC_CTYPE"), Some("UTF-8"));
        assert_eq!(get(&env, "EDITOR"), Some("vi"));
        let path = get(&env, "PATH").unwrap();
        assert!(path.ends_with(BASE_PATH) && !path.contains("/junk"), "PATH {path}");
        if cfg!(target_os = "macos") {
            assert!(path.starts_with("/opt/homebrew/bin:/opt/homebrew/sbin:"), "PATH {path}");
            assert_eq!(get(&env, "TMPDIR"), Some("/var/folders/x/T/"), "TMPDIR falls back to ours");
            assert_eq!(get(&env, "XDG_RUNTIME_DIR"), None);
        } else {
            assert_eq!(get(&env, "TMPDIR"), None);
            assert_eq!(get(&env, "XDG_RUNTIME_DIR"), Some("/run/user/501"));
        }
        assert_eq!(env.iter().filter(|(k, _)| k == "LANG").count(), 1, "LANG twice");
    }

    #[test]
    fn homebrew_only_when_installed() {
        let env = session_env_from(&acct(), [], None, |_| false, &[], &[]);
        assert_eq!(get(&env, "PATH"), Some(BASE_PATH));
        assert_eq!(get(&env, "LANG"), None, "no locale invented");
    }

    #[test]
    fn home_override() {
        let real = passwd_account();
        let home = sandbox_home();
        let a = account();
        assert!(a.home == home && a.name == real.name && a.shell == real.shell, "{a:?}");
        assert_ne!(a.home, real.home);
    }

    #[test]
    fn this_account() {
        let a = passwd_account();
        assert!(!a.name.is_empty() && a.home.is_absolute() && a.shell.starts_with('/'), "{a:?}");
        assert_eq!(a.login_shell(), [a.shell.clone(), "-l".into()]);
        if cfg!(target_os = "macos") {
            assert!(darwin_temp_dir().is_some_and(|t| t.starts_with('/')), "no per-user temp dir");
        }
    }
}
