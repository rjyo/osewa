//! `osewa server` and `osewa service` (login starting a server is in
//! main_test.rs).

use std::path::PathBuf;

use super::*;
use crate::cli::Out;
use crate::daemon::{listen_addr, lock_server, log_path, our_server_at, running_server, server_main, start_daemon};
use crate::service::Service;

fn free_port() -> String {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string()
}

/// A Service with a fake service manager that records what it ran.
fn service(os_name: &str, home: &Path, exe: &str, out: Buf, ran: Arc<Mutex<Vec<String>>>, fail: fn(&[&str]) -> Option<String>) -> Service {
    Service {
        os_name: os_name.into(),
        home: home.into(),
        exe: PathBuf::from(exe),
        stdout: Out::new(out),
        run: Box::new(move |name, a| {
            ran.lock().unwrap().push(format!("{name} {}", a.join(" ")));
            match fail(a) {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }),
    }
}

fn never(_: &[&str]) -> Option<String> {
    None
}

#[test]
fn server_lock() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    assert!(running_server().is_none(), "running before anything started");
    let lock = lock_server().unwrap();
    assert_eq!(running_server(), Some(std::process::id() as i32), "running");
    let err = lock_server().err().unwrap();
    assert!(err.contains("already running") && err.contains(&std::process::id().to_string()), "second lock: {err}");
    let errb = Buf::default();
    let c = server_main(&args(&["--listen", &free_port()]), Out::new(Buf::default()), Out::new(errb.clone()));
    assert!(c == 1 && errb.string().contains("already running"), "second server: {c} {:?}", errb.string());
    drop(lock);
    assert!(running_server().is_none(), "still running after unlock");
}

#[test]
fn listen_addrs() {
    for (a, want) in [
        ("", "127.0.0.1:7322"),
        ("--listen :9000", "127.0.0.1:9000"),
        ("--listen=10.0.0.2:1", "10.0.0.2:1"),
        ("--name x -listen 0.0.0.0:5", "127.0.0.1:5"),
        ("--listen [::]:6", "127.0.0.1:6"),
    ] {
        let a: Vec<String> = a.split_whitespace().map(String::from).collect();
        assert_eq!(listen_addr(&a), want, "{a:?}");
    }
}

/// --daemon starts one detached server, says so, and starts no second one;
/// a service install takes over from it.
#[test]
fn daemon() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let addr = free_port();
    let (out, errb) = (Buf::default(), Buf::default());
    let c = start_daemon(&osewa_bin(), &args(&["--listen", &addr]), &mut out.clone(), &mut errb.clone());
    assert_eq!(c, 0, "start: {}", errb.string());
    let s = out.string();
    let pid: i32 = s
        .split("started (pid ")
        .nth(1)
        .and_then(|r| r.split_once(", "))
        .filter(|(_, rest)| rest.starts_with(&addr))
        .and_then(|(p, _)| p.parse().ok())
        .unwrap_or_else(|| panic!("output {s:?}"));
    struct Kill(i32);
    impl Drop for Kill {
        fn drop(&mut self) {
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
    let _kill = Kill(pid);
    std::net::TcpStream::connect(&addr).unwrap();
    // It is detached: a session of its own.
    assert_eq!(unsafe { libc::getsid(pid) }, pid, "not in a session of its own");

    out.reset();
    let c = start_daemon(&osewa_bin(), &args(&["--listen", &addr]), &mut out.clone(), &mut errb.clone());
    assert!(c == 0 && out.string().contains(&format!("already running (pid {pid})")), "second start: {c} {:?}", out.string());

    // Installing the service stops the daemon first, so they don't fight
    // over the port.
    let home = TempDir::new();
    g.set("XDG_CONFIG_HOME", "");
    let ran = Arc::new(Mutex::new(Vec::new()));
    let mut sv = service("linux", home.path(), "/home/u/.local/bin/osewa", Buf::default(), ran.clone(), never);
    sv.install(&args(&["--listen", &addr])).unwrap();
    assert!(running_server().is_none(), "daemon still running after service install");
    assert_eq!(ran.lock().unwrap().len(), 2, "ran {:?}", ran.lock().unwrap());
}

/// A server that can't start is reported with its log.
#[test]
fn daemon_fails_to_start() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let errb = Buf::default();
    let c = start_daemon(&osewa_bin(), &args(&["--listen", &ln.local_addr().unwrap().to_string()]), &mut Buf::default(), &mut errb.clone());
    let e = errb.string();
    assert!(c == 1 && e.contains("address already in use"), "start on a taken port: {c} {e:?}");
    assert!(e.contains(&log_path().display().to_string()), "log not named: {e:?}");
}

#[test]
fn service_systemd() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    g.set("XDG_CONFIG_HOME", "");
    let home = TempDir::new();
    let (out, ran) = (Buf::default(), Arc::new(Mutex::new(Vec::new())));
    let mut sv = service("linux", home.path(), "/home/u/.local/bin/osewa", out.clone(), ran.clone(), never);
    sv.install(&args(&["--listen", ":7400", "--no-compress", "--no-forward", "--no-exec"])).unwrap();
    let unit = std::fs::read_to_string(home.path().join(".config/systemd/user/osewa.service")).unwrap();
    // KillMode=process, or stopping the server kills every holder in its
    // cgroup, and every session with it.
    assert_eq!(
        unit,
        "[Unit]\nDescription=osewa server (persistent terminals)\n\n[Service]\n\
         ExecStart=\"/home/u/.local/bin/osewa\" \"server\" \"--listen\" \":7400\" \"--no-compress\" \"--no-forward\" \"--no-exec\"\n\
         KillMode=process\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n"
    );
    assert_eq!(ran.lock().unwrap().join("; "), "systemctl --user daemon-reload; systemctl --user enable --now osewa.service");
    assert!(out.string().contains("enabled, started") && out.string().contains("enable-linger"), "output {:?}", out.string());
    ran.lock().unwrap().clear();
    sv.uninstall().unwrap();
    assert!(!home.path().join(".config/systemd/user/osewa.service").exists(), "unit left behind");
    assert_eq!(ran.lock().unwrap()[0], "systemctl --user disable --now osewa.service");
}

#[test]
fn service_launchd() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let home = TempDir::new();
    let (out, ran) = (Buf::default(), Arc::new(Mutex::new(Vec::new())));
    fn no_gui(a: &[&str]) -> Option<String> {
        (a.len() > 1 && a[0] == "bootstrap" && a[1].starts_with("gui/")).then(|| "no GUI session over ssh".into())
    }
    let mut sv = service("darwin", home.path(), "/Users/u/.local/bin/osewa", out.clone(), ran.clone(), no_gui);
    sv.install(&args(&["--name", "a&b"])).unwrap();
    let path = home.path().join("Library/LaunchAgents/dev.osewa.server.plist");
    let b = std::fs::read_to_string(&path).unwrap();
    let log = log_path().display().to_string();
    // Byte for byte the v0 agent, so a reinstall by any osewa is a no-op.
    let want = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>dev.osewa.server</string>
	<key>ProgramArguments</key>
	<array>
		<string>/Users/u/.local/bin/osewa</string>
		<string>server</string>
		<string>--name</string>
		<string>a&amp;b</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    );
    assert_eq!(b, want);
    for w in ["<string>/Users/u/.local/bin/osewa</string>", "<string>a&amp;b</string>", "<key>RunAtLoad</key>", &log] {
        assert!(b.contains(w), "plist lacks {w:?}:\n{b}");
    }
    // No GUI session (over ssh): it lands in the user domain.
    let uid = unsafe { libc::getuid() };
    let joined = ran.lock().unwrap().join("; ");
    assert!(
        joined.contains(&format!("launchctl bootstrap user/{uid} {}", path.display())) && out.string().contains(&format!("user/{uid}")),
        "ran {joined:?}, out {:?}",
        out.string()
    );
    sv.uninstall().unwrap();
    assert!(!path.exists(), "plist left behind");
}

/// The agent's log dir and log are made private before launchd starts it
/// (launchd makes them 0744 and 0644), and ones it already made are
/// tightened, as any update's restart (open_log) does: a wide state dir used
/// to fail that restart.
#[test]
fn service_launchd_private_log() {
    use std::os::unix::fs::PermissionsExt;
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let dir = crate::daemon::state_dir();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let home = TempDir::new();
    let mut sv = service("darwin", home.path(), "/x", Buf::default(), Arc::default(), never);
    sv.install(&[]).unwrap();
    assert_eq!((mode(&dir), mode(&log_path())), (0o700, 0o600), "fresh install");

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o744)).unwrap();
    std::fs::set_permissions(log_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(osewa_paths::ensure(&dir).is_err(), "0744 passed the strict check");
    sv.install(&[]).unwrap();
    assert_eq!((mode(&dir), mode(&log_path())), (0o700, 0o600), "reinstall over launchd's modes");

    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o744)).unwrap();
    std::fs::set_permissions(log_path(), std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(crate::daemon::open_log().unwrap());
    assert_eq!((mode(&dir), mode(&log_path())), (0o700, 0o600), "restart over launchd's modes");

    // A log someone left as a symlink while the dir was wide isn't followed.
    let target = TempDir::new();
    let victim = target.path().join("victim");
    std::fs::write(&victim, "").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(log_path()).unwrap();
    std::os::unix::fs::symlink(&victim, log_path()).unwrap();
    assert!(crate::daemon::open_log().is_err(), "followed a symlinked log");
    assert_eq!(mode(&victim), 0o644, "chmodded through the symlink");
    std::fs::remove_file(log_path()).unwrap();

    // A FIFO left as the log fails instead of hanging the open.
    let fifo = std::ffi::CString::new(log_path().as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(crate::daemon::open_log().is_err(), "opened a FIFO as the log");
    std::fs::remove_file(log_path()).unwrap();

    // A dir of ours missing its owner bits is made usable again.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    drop(crate::daemon::open_log().unwrap());
    assert_eq!(mode(&dir), 0o700, "0500 state dir");

    // Only a real dir is tightened: a symlink in its place still fails.
    std::fs::remove_dir_all(&dir).unwrap();
    let real = TempDir::new();
    std::os::unix::fs::symlink(real.path(), &dir).unwrap();
    assert!(crate::daemon::open_log().is_err(), "followed a symlinked state dir");
}

#[test]
fn service_errors() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let home = TempDir::new();
    let mut sv = service("plan9", home.path(), "/x", Buf::default(), Arc::default(), never);
    let err = sv.install(&[]).unwrap_err();
    assert!(err.contains("--daemon"), "{err}");
    assert!(sv.uninstall().is_err(), "uninstalled on plan9");
    sv.os_name = "linux".into();
    assert!(sv.install(&args(&["--bogus"])).is_err(), "bad flag accepted");
    sv.run = Box::new(|_, _| Err("systemd not running".into()));
    let err = sv.install(&[]).unwrap_err();
    assert!(err.contains("systemd not running"), "{err}");
    assert_eq!(crate::service::service_main(&[], Out::new(Buf::default()), Out::new(Buf::default())), 2);
}

/// Only this user's server counts as up: something else on the port (what
/// --daemon used to take for success while its child was failing) doesn't.
#[test]
fn our_server_at_port() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let id = osewa_auth::Identity::load_or_create(&osewa_auth::identity_path()).unwrap();
    let devices_dir = TempDir::new();
    let devices = osewa_auth::Devices::new(devices_dir.path().join("devices"));

    let plain = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let plain_addr = plain.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for c in plain.incoming() {
            drop(c);
        }
    });
    assert!(!our_server_at(&plain_addr), "a plain TCP listener counted as our server");

    let serve = |key: &osewa_auth::Identity| {
        let cfg = Arc::new(osewa_auth::server_config(key, &devices));
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = ln.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for c in ln.incoming().flatten() {
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    let mut c = c;
                    if let Ok(mut s) = rustls::ServerConnection::new(cfg) {
                        let _ = s.complete_io(&mut c);
                    }
                });
            }
        });
        addr
    };
    let other = osewa_auth::Identity::generate();
    assert!(!our_server_at(&serve(&other)), "another server's key counted as ours");
    assert!(our_server_at(&serve(&id)), "our own server not recognized");
}

/// restart kickstarts the loaded service in place (launchd, systemd), never
/// unloading it, so it comes back whatever becomes of the restarting
/// process; with no service and no server it says so.
#[test]
fn service_restart() {
    let (mut keep, mut g) = (Vec::new(), EnvGuard::new());
    g.host_dirs(&mut keep);
    let uid = unsafe { libc::getuid() };
    fn no_gui(a: &[&str]) -> Option<String> {
        (a.len() > 2 && a[2].contains("gui/")).then(|| "no GUI session over ssh".into())
    }
    for (os_name, want) in [
        ("darwin", format!("launchctl kickstart -k user/{uid}/dev.osewa.server")),
        ("linux", "systemctl --user restart osewa.service".to_string()),
    ] {
        let home = TempDir::new();
        let ran = Arc::new(Mutex::new(Vec::new()));
        let mut sv = service(os_name, home.path(), "", Buf::default(), ran.clone(), no_gui);
        sv.restart().unwrap();
        let r = ran.lock().unwrap();
        assert_eq!(r.last().unwrap(), &want, "{os_name}: ran {r:?}");
        assert!(!r.join(";").contains("bootout"), "{os_name}: restart unloaded the service: {r:?}");
    }
    let home = TempDir::new();
    let mut sv = service("linux", home.path(), "", Buf::default(), Arc::default(), |_| Some("no systemd".into()));
    let err = sv.restart().unwrap_err();
    assert!(err.contains("no service and no server"), "{err}");
}
