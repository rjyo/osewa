//! The command line: flags, dispatch, errors, login starting a server.
//! The escaper, UTF-8 splitting, input forwarding and viewer notice tests
//! are the attach UI's.

use std::io::Cursor;

use osewa_client::login::{BoxFuture, CheckFn, InstallFn, SshConfigFn, SshFn};
use osewa_client::{hosts, install};
use osewa_wire as wire;

use super::*;
use crate::cli::{Error, Flags, Kind};
use crate::commands::{device_name, idle_string, new_client_id, print_sessions};

#[test]
fn parse_args_interspersed() {
    let mut fs = Flags::new(&[("name", Kind::Str), ("addr", Kind::Str)]);
    let pos = fs.parse(&args(&["user@box", "--name", "n", "extra", "--addr=h:1"])).unwrap();
    assert!(pos == ["user@box", "extra"] && fs.str("name") == "n" && fs.str("addr") == "h:1", "pos={pos:?}");
    assert!(fs.parse(&args(&["--nope"])).is_err(), "want error for unknown flag");
    // The flag forms: -x, -x=v, bools without a value, -h for usage.
    let mut fs = Flags::new(&[("cwd", Kind::Str), ("v", Kind::Bool), ("env", Kind::Multi)]);
    let pos = fs.parse(&args(&["-cwd=/", "a", "-v", "--env", "A=1", "-env=B=2"])).unwrap();
    assert!(pos == ["a"] && fs.str("cwd") == "/" && fs.bool("v") && fs.all("env") == ["A=1", "B=2"], "pos={pos:?}");
    assert!(matches!(fs.parse(&args(&["-h"])), Err(Error::Usage)));
}

fn ok<T: Send + 'static>(v: T) -> BoxFuture<Result<T, osewa_client::Error>> {
    Box::pin(async move { Ok(v) })
}

fn no_check() -> CheckFn {
    Arc::new(|_, _| ok(()))
}

/// Stands in for `ssh target osewa enroll`, running the real enroll against
/// a host directory, and records the commands run.
fn fake_enroll(host_dir: &Path, motd: &'static str, commands: Arc<Mutex<Vec<String>>>) -> SshFn {
    let host_dir = host_dir.to_path_buf();
    Arc::new(move |_target, command, stdin| {
        assert!(command.contains("osewa enroll && "), "ssh runs {command:?}");
        commands.lock().unwrap().push(command.clone());
        let id = osewa_auth::Identity::load_or_create(&host_dir.join("server.key")).unwrap();
        let mut out = motd.as_bytes().to_vec();
        let r = osewa_auth::enroll(Cursor::new(stdin), &mut out, &id, &osewa_auth::Devices::new(host_dir.join("devices")), 7400);
        // What the host's server start prints.
        if command.ends_with("server --daemon") {
            out.extend_from_slice(b"osewa server: started (pid 4242, 127.0.0.1:7400, log ~/.local/state/osewa/server.log)\n");
        } else if command.ends_with("service install") {
            out.extend_from_slice(b"osewa service: installed systemd user unit ~/.config/systemd/user/osewa.service (enabled, started)\n");
        }
        Box::pin(async move { r.map(|_| out).map_err(|e| osewa_client::Error::Other(e.to_string())) })
    })
}

fn ssh_config(f: impl Fn(&str) -> Result<String, String> + Send + Sync + 'static) -> SshConfigFn {
    Arc::new(move |target| {
        let r = f(&target).map(String::into_bytes).map_err(osewa_client::Error::Other);
        Box::pin(async move { r })
    })
}

fn login_env(dir: &Path) -> (Env, Buf) {
    let (mut e, out, _) = env(dir, "");
    e.check = Some(no_check());
    (e, out)
}

#[test]
fn login() {
    let (dir, host_dir) = (TempDir::new(), TempDir::new());
    let (mut e, out) = login_env(dir.path());
    let commands = Arc::new(Mutex::new(Vec::new()));
    let installed = Arc::new(Mutex::new(Vec::<String>::new()));
    let result = Arc::new(Mutex::new(install::Outcome { installed: true, replaced: false }));
    e.ssh = Some(fake_enroll(host_dir.path(), "Welcome to box!\n", commands.clone()));
    let (inst, res) = (installed.clone(), result.clone());
    let install: InstallFn = Arc::new(move |target, binary_for| {
        inst.lock().unwrap().push(target);
        // A host like this one gets this very binary.
        let (os_name, arch_name) = osewa_client::login::local_platform();
        let b = binary_for(os_name, arch_name);
        let r = *res.lock().unwrap();
        Box::pin(async move {
            assert!(!b?.is_empty(), "empty binary");
            Ok(r)
        })
    });
    e.install = Some(install);
    e.ssh_config = Some(ssh_config(|t| Ok(format!("hostname {}\n", &t[t.find('@').map_or(0, |i| i + 1)..]))));
    let login = |a: &[&str]| block(e.login(&ctx(), &args(a))).map_err(|e| e.to_string());
    login(&["me@box.example", "--device", "laptop"]).unwrap();
    {
        let (i, c) = (installed.lock().unwrap(), commands.lock().unwrap());
        assert!(
            i.as_slice() == ["me@box.example"] && c[0] == "~/.local/bin/osewa enroll && ~/.local/bin/osewa server --daemon",
            "installed {i:?}, ran {c:?}"
        );
    }
    assert!(out.string().contains("installed osewa on me@box.example") && !out.string().contains("restarted"), "{}", out.string());
    // Only replacing a different build warns about a running server.
    *result.lock().unwrap() = install::Outcome { installed: true, replaced: true };
    out.reset();
    login(&["me@box.example", "--device", "laptop"]).unwrap();
    assert!(out.string().contains("keeps its old build until restarted"), "{}", out.string());
    *result.lock().unwrap() = install::Outcome::default();
    out.reset();
    login(&["me@box.example", "--device", "laptop"]).unwrap();
    assert!(!out.string().contains("osewa on"), "same build reported as installed: {:?}", out.string());
    installed.lock().unwrap().truncate(1);
    commands.lock().unwrap().truncate(1);

    let h = hosts::find(dir.path(), "box.example").unwrap();
    assert_eq!(h.addr, "box.example:7400");
    let server = osewa_auth::Identity::load_or_create(&host_dir.path().join("server.key")).unwrap();
    assert_eq!(h.server_key, osewa_auth::encode_key(&server.public()), "server key not stored");
    let me = osewa_auth::Identity::load_or_create(&hosts::key_path(dir.path())).unwrap();
    let devices = osewa_auth::Devices::new(host_dir.path().join("devices"));
    let dev = devices.allowed(&me.public()).expect("device not allowed");
    assert_eq!(dev.name, "laptop");
    assert!(out.string().contains("\"laptop\""), "{}", out.string());

    // Explicit name and addr; the same identity is reused.
    login(&["--name", "work", "box", "--addr", "10.0.0.1:9", "--no-install"]).unwrap();
    {
        let (i, c) = (installed.lock().unwrap(), commands.lock().unwrap());
        assert!(i.len() == 1 && c[1] == "osewa enroll && osewa server --daemon", "--no-install: installed {i:?}, ran {c:?}");
    }
    let h = hosts::find(dir.path(), "work").unwrap();
    assert_eq!(h.addr, "10.0.0.1:9");
    // One entry per key: logging in again re-enrolls under the new name.
    let devs = devices.list().unwrap();
    assert!(devs.len() == 1 && devs[0].key == osewa_auth::encode_key(&me.public()), "devices = {devs:?}");
}

fn no_install() -> InstallFn {
    Arc::new(|_, _| ok(install::Outcome::default()))
}

#[test]
fn login_errors() {
    let dir = TempDir::new();
    let (mut e, _) = login_env(dir.path());
    e.install = Some(no_install());
    assert!(matches!(block(e.login(&ctx(), &[])), Err(Error::Usage)));
    e.ssh = Some(Arc::new(|_, _, _| Box::pin(async { Err(osewa_client::Error::Other("boom".into())) })));
    let err = block(e.login(&ctx(), &args(&["box"]))).unwrap_err().to_string();
    assert!(err.contains("boom"), "{err}");
    e.ssh = Some(Arc::new(|_, _, _| ok(b"nothing here\n".to_vec())));
    assert!(block(e.login(&ctx(), &args(&["box"]))).is_err(), "want error without a response");
}

#[test]
fn run_usage() {
    let mut keep = Vec::new();
    let mut g = EnvGuard::new();
    // Away from a real server on this machine, which `server` would find.
    g.host_dirs(&mut keep);
    let dir = TempDir::new();
    let (code, _, err) = run(dir.path(), &[]);
    assert!(code == 2 && err.contains("usage"), "no args: {code} {err:?}");
    let (code, _, err) = run(dir.path(), &["bogus"]);
    assert!(code == 2 && err.contains("usage"), "unknown: {code}");
    let (code, out, _) = run(dir.path(), &["help"]);
    assert!(code == 0 && out.contains("attach"), "help: {code}");
    for a in [&["ls"][..], &["kill", "h"], &["attach"], &["new"], &["login"]] {
        let (code, _, err) = run(dir.path(), a);
        assert_eq!(code, 2, "{a:?}: {err}");
    }
    assert_eq!(run(dir.path(), &["holder", "--bogus"]).0, 2, "holder bad flag");
    assert_eq!(run(dir.path(), &["server", "--bogus"]).0, 2, "server bad flag");
}

#[test]
fn run_unknown_host() {
    let dir = TempDir::new();
    for a in [&["ls", "nope"][..], &["new", "nope", "--", "sh"], &["kill", "nope", "id"], &["attach", "nope"]] {
        let (code, _, err) = run(dir.path(), a);
        assert!(code == 1 && err.contains("unknown host"), "{a:?}: {code} {err:?}");
    }
}

#[test]
fn dial_bad_server_key() {
    let dir = TempDir::new();
    let (e, _, _) = env(dir.path(), "");
    let h = |key: &str| hosts::Host { addr: "127.0.0.1:1".into(), server_key: key.into(), ..Default::default() };
    assert!(block(e.dial(&ctx(), &h("bad"))).is_err(), "want key error");
    let key = osewa_auth::encode_key(&[0u8; 32]);
    let r = block(async { tokio::time::timeout(Duration::from_secs(1), e.dial(&ctx(), &h(&key))).await });
    assert!(matches!(r, Ok(Err(_))), "want dial error");
}

#[test]
fn login_resolves_ssh_alias() {
    let (dir, host_dir) = (TempDir::new(), TempDir::new());
    let (mut e, _) = login_env(dir.path());
    e.ssh = Some(fake_enroll(host_dir.path(), "", Arc::default()));
    e.install = Some(no_install());
    e.ssh_config = Some(ssh_config(|t| {
        assert_eq!(t, "me@box", "ssh -G");
        Ok("user me\nhostname 10.0.0.5\nport 22\n".into())
    }));
    block(e.login(&ctx(), &args(&["me@box"]))).unwrap();
    assert_eq!(hosts::find(dir.path(), "box").unwrap().addr, "10.0.0.5:7400");
    // ssh unavailable: fall back to the target's host part.
    e.ssh_config = Some(ssh_config(|_| Err("no ssh".into())));
    block(e.login(&ctx(), &args(&["--name", "b2", "me@box"]))).unwrap();
    assert_eq!(hosts::find(dir.path(), "b2").unwrap().addr, "box:7400", "fallback addr");
}

#[test]
fn client_id() {
    let (a, b) = (new_client_id(), new_client_id());
    assert!(a != b && a.starts_with(&format!("{}-", device_name())), "ids {a:?} {b:?}");
}

#[test]
fn print_sessions_columns() {
    let mut b = Vec::new();
    print_sessions(
        &mut b,
        &[
            wire::SessionInfo { id: "aa".into(), cols: 80, rows: 24, title: "vim".into(), owner: "laptop-0a1b".into(), attached: 2, ..Default::default() },
            wire::SessionInfo { id: "bb".into(), cols: 10, rows: 5, exited: true, exit_code: 3, idle_seconds: 3 * 3600 + 5 * 60, ..Default::default() },
        ],
    )
    .unwrap();
    let s = String::from_utf8(b).unwrap();
    let lines: Vec<&str> = s.trim().split('\n').collect();
    assert_eq!(lines.len(), 3, "{s:?}");
    for (i, want) in [
        &["ID", "TITLE", "SIZE", "OWNER", "ATTACHED", "IDLE", "STATUS", "STATE"][..],
        &["aa", "vim", "80x24", "laptop", "2", "-", "-", "running"],
        &["bb", "-", "10x5", "-", "0", "3h5m", "-", "exited", "3"],
    ]
    .iter()
    .enumerate()
    {
        let got: Vec<&str> = lines[i].split_whitespace().collect();
        assert_eq!(got, *want, "row {i}");
    }
    // Exactly the v0 layout: columns padded to the widest cell plus 2.
    assert_eq!(
        s,
        "ID  TITLE  SIZE   OWNER   ATTACHED  IDLE  STATUS  STATE\n\
         aa  vim    80x24  laptop  2         -     -       running\n\
         bb  -      10x5   -       0         3h5m  -       exited 3\n"
    );
    for (d, want) in [(45, "45s"), (12 * 60, "12m"), (50 * 3600, "2d2h")] {
        assert_eq!(idle_string(Duration::from_secs(d)), want, "{d}s");
    }
}

#[test]
fn term_size_not_a_terminal() {
    use std::os::fd::AsRawFd;
    let dir = TempDir::new();
    let f = std::fs::File::create(dir.path().join("x")).unwrap();
    assert_eq!(crate::tty::term_size(f.as_raw_fd()), (80, 24));
    assert!(!crate::tty::is_terminal(f.as_raw_fd()), "file is a terminal");
    assert!(crate::tty::make_raw(f.as_raw_fd()).is_err(), "raw mode on a file");
}

#[test]
fn login_install_fails() {
    let dir = TempDir::new();
    let (mut e, _) = login_env(dir.path());
    e.install = Some(Arc::new(|_, _| Box::pin(async { Err(osewa_client::Error::Other("no sh on host".into())) })));
    e.ssh = Some(Arc::new(|_, _, _| panic!("enrolled after a failed install")));
    let err = block(e.login(&ctx(), &args(&["box"]))).unwrap_err().to_string();
    assert!(err.contains("no sh"), "{err}");
}

#[test]
fn version() {
    let dir = TempDir::new();
    for a in ["version", "--version"] {
        let (code, out, _) = run(dir.path(), &[a]);
        assert_eq!(code, 0);
        let want = format!(
            "osewa {} (protocol {}, ghostty {}, snapshot v{})\n",
            crate::cli::VERSION,
            wire::VERSION,
            osewa_ghostty::COMMIT,
            osewa_ghostty::SNAPSHOT_VERSION
        );
        assert_eq!(out, want);
    }
    if option_env!("OSEWA_VERSION").is_none() {
        assert_eq!(crate::cli::VERSION, "dev");
    }
    let (code, out, _) = run(dir.path(), &["version", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(out, format!("{}\n", osewa_client::build_info()));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["ghostty"] == osewa_ghostty::COMMIT && v["snapshot_version"] == osewa_ghostty::SNAPSHOT_VERSION && v["version"] == wire::VERSION, "{v}");
}

/// login leaves a server running on the host and says how: a daemon by
/// default (with a hint that it won't survive a reboot), or a service.
#[test]
fn login_starts_server() {
    for (extra, command, said, hint) in [
        (&[][..], "osewa enroll && osewa server --daemon", "osewa server: started (pid 4242", true),
        (&["--service"][..], "osewa enroll && osewa service install", "osewa service: installed systemd user unit", false),
    ] {
        let (dir, host_dir) = (TempDir::new(), TempDir::new());
        let (mut e, out) = login_env(dir.path());
        let ran = Arc::new(Mutex::new(Vec::new()));
        e.ssh = Some(fake_enroll(host_dir.path(), "", ran.clone()));
        e.ssh_config = Some(ssh_config(|_| Ok("hostname box\n".into())));
        let mut a = args(&["--no-install", "box"]);
        a.extend(args(extra));
        block(e.login(&ctx(), &a)).unwrap();
        assert_eq!(ran.lock().unwrap().last().unwrap(), command, "{extra:?}");
        assert!(out.string().contains(said), "{extra:?}: host's word not relayed: {:?}", out.string());
        assert_eq!(out.string().contains("osewa login --service box"), hint, "{extra:?}: reboot hint: {:?}", out.string());
    }
}
