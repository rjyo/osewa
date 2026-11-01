//! Locates osewa's per-user directories. The layout is protocol v0's, shared
//! by every osewa on a host, so they find each other's sockets and keys.

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

mod account;

pub use account::{account, home_override, passwd_account, sandbox, sandbox_home, session_env, session_env_from, Account, HOME_OVERRIDE, RESERVED_PREFIX};

fn var(k: &str) -> Option<String> {
    env::var(k).ok().filter(|v| !v.is_empty())
}

/// Where holder sockets live: $XDG_RUNTIME_DIR/osewa, else
/// ~/.local/state/osewa/run. $OSEWA_RUN_DIR overrides both.
pub fn run_dir() -> PathBuf {
    if let Some(d) = var("OSEWA_RUN_DIR") {
        return d.into();
    }
    if let Some(d) = var("XDG_RUNTIME_DIR") {
        return Path::new(&d).join("osewa");
    }
    home().join(".local/state/osewa/run")
}

/// Holds keys, devices and known hosts: $OSEWA_CONFIG_DIR, else
/// $XDG_CONFIG_HOME/osewa, else ~/.config/osewa.
pub fn config_dir() -> PathBuf {
    if let Some(d) = var("OSEWA_CONFIG_DIR") {
        return d.into();
    }
    if let Some(d) = var("XDG_CONFIG_HOME") {
        return Path::new(&d).join("osewa");
    }
    home().join(".config/osewa")
}

/// Creates dir with mode 0700 if it doesn't exist, then checks that it is a
/// real directory owned by this user that nobody else can use. Holder
/// sockets and keys live in these directories: whoever can write one could
/// plant a socket and read every keystroke.
pub fn ensure(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    check_private(dir)
}

/// Errors unless dir is a directory (not a symlink) owned by the current
/// user with no group or other permissions.
pub fn check_private(dir: &Path) -> io::Result<()> {
    let st = fs::symlink_metadata(dir)?;
    let fail = |msg: String| Err(io::Error::new(io::ErrorKind::PermissionDenied, msg));
    if !st.is_dir() {
        return fail(format!("paths: {} is not a directory", dir.display()));
    }
    let uid = unsafe { libc::getuid() };
    if st.uid() != uid {
        return fail(format!("paths: {} is owned by uid {}, not {uid}", dir.display(), st.uid()));
    }
    let perm = st.permissions().mode() & 0o777;
    if perm & 0o077 != 0 {
        return fail(format!("paths: {} has mode {perm:o}; it must be private (chmod 700)", dir.display()));
    }
    Ok(())
}

/// The user's home directory, or / when there is none.
pub fn home() -> PathBuf {
    var("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The environment is process-wide; tests that change it take turns.
    static ENV: Mutex<()> = Mutex::new(());

    fn set(k: &str, v: &str) {
        unsafe { env::set_var(k, v) }
    }

    #[test]
    fn dirs() {
        let _g = ENV.lock().unwrap();
        set("OSEWA_RUN_DIR", "/x/run");
        assert_eq!(run_dir(), Path::new("/x/run"));
        set("OSEWA_RUN_DIR", "");
        set("XDG_RUNTIME_DIR", "/run/user/1");
        assert_eq!(run_dir(), Path::new("/run/user/1/osewa"));
        set("XDG_RUNTIME_DIR", "");
        set("HOME", "/h");
        assert_eq!(run_dir(), Path::new("/h/.local/state/osewa/run"));
        set("OSEWA_CONFIG_DIR", "/c");
        assert_eq!(config_dir(), Path::new("/c"));
        set("OSEWA_CONFIG_DIR", "");
        set("XDG_CONFIG_HOME", "/xdg");
        assert_eq!(config_dir(), Path::new("/xdg/osewa"));
        set("XDG_CONFIG_HOME", "");
        assert_eq!(config_dir(), Path::new("/h/.config/osewa"));
    }

    fn tmp(name: &str) -> PathBuf {
        let d = env::temp_dir().join(format!("osewa-paths-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ensure_makes_private_dirs() {
        let d = tmp("ensure").join("a/b");
        ensure(&d).unwrap();
        assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn rejects_shared_dirs() {
        let base = tmp("shared");
        let open = base.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure(&open).is_err(), "0755 dir accepted");
        let file = base.join("file");
        fs::write(&file, b"").unwrap();
        assert!(ensure(&file).is_err(), "file accepted");
        let private = base.join("priv");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(check_private(&link).is_err(), "symlink accepted");
        check_private(&private).unwrap();
        assert!(check_private(&base.join("missing")).is_err());
    }
}
