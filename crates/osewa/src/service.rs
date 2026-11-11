//! `osewa service`: the server as a per-user service that
//! survives reboots, a systemd --user unit on Linux, a launchd agent on
//! macOS.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::cli::Out;
use crate::daemon::{log_path, open_log, pid_path, running_server, start_daemon, DAEMON_WAIT};
use crate::vtfilter::printable_lines;

const UNIT_NAME: &str = "osewa.service";
const LAUNCH_LABEL: &str = "dev.osewa.server";

/// Runs a service manager command.
pub(crate) type RunFn = Box<dyn FnMut(&str, &[&str]) -> Result<(), String>>;

pub(crate) struct Service {
    /// "linux", "darwin", or another OS name as the release binaries use.
    pub os_name: String,
    pub home: PathBuf,
    /// The osewa binary the service runs.
    pub exe: PathBuf,
    pub stdout: Out,
    pub run: RunFn,
}

fn run_quiet(name: &str, args: &[&str]) -> Result<(), String> {
    match Command::new(name).args(args).output() {
        Err(e) => Err(format!("{name} {}: {e}", args.join(" "))),
        Ok(o) if !o.status.success() => {
            let mut out = o.stdout;
            out.extend_from_slice(&o.stderr);
            Err(format!("{name} {}: {}: {}", args.join(" "), exit_status(o.status), String::from_utf8_lossy(&out).trim()))
        }
        Ok(_) => Ok(()),
    }
}

/// Words an exit status: "exit status N" or "signal: N".
fn exit_status(s: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(sig)) => format!("signal: {sig}"),
        _ => "exit status unknown".into(),
    }
}

impl Service {
    fn new(stdout: Out) -> Result<Service, String> {
        let home = osewa_paths::home();
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let os_name = osewa_client::login::local_platform().0.to_string();
        Ok(Service { os_name, home, exe, stdout, run: Box::new(run_quiet) })
    }

    fn say(&self, s: &str) {
        let _ = writeln!(self.stdout.clone(), "{s}");
    }

    pub fn unit_path(&self) -> PathBuf {
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => self.home.join(".config"),
        };
        base.join("systemd").join("user").join(UNIT_NAME)
    }

    pub fn plist_path(&self) -> PathBuf {
        self.home.join("Library").join("LaunchAgents").join(format!("{LAUNCH_LABEL}.plist"))
    }

    /// Writes the service for `osewa server <args>` and starts it.
    pub fn install(&mut self, args: &[String]) -> Result<(), String> {
        let mut fs = crate::cli::Flags::new(&[
            ("listen", crate::cli::Kind::Str),
            ("name", crate::cli::Kind::Str),
            ("no-compress", crate::cli::Kind::Bool),
            ("no-forward", crate::cli::Kind::Bool),
            ("no-exec", crate::cli::Kind::Bool),
            ("no-quic", crate::cli::Kind::Bool),
        ]);
        // One flag.Parse: no positional arguments.
        if fs.parse_once_all(args).is_err() {
            return Err(format!("bad server flags {}", quote_list(args)));
        }
        // A server started with --daemon holds the port; the service takes
        // over.
        stop_daemon()?;
        let mut argv = vec![self.exe.to_string_lossy().into_owned(), "server".into()];
        argv.extend(args.iter().cloned());
        match self.os_name.as_str() {
            "linux" => self.install_systemd(&argv),
            "darwin" => self.install_launchd(&argv),
            os => Err(format!("no service manager support on {os}; use `osewa server --daemon`")),
        }
    }

    fn install_systemd(&mut self, argv: &[String]) -> Result<(), String> {
        let quoted: Vec<String> = argv.iter().map(|a| osewa_wire::quote(a)).collect();
        // KillMode=process: the holders the server starts stay in its
        // cgroup (setsid doesn't leave it), and systemd's default kills the
        // whole cgroup on stop, restart, disable --now and whenever the
        // server exits, ending every session. Only the server goes.
        let unit = format!(
            "[Unit]\nDescription=osewa server (persistent terminals)\n\n[Service]\nExecStart={}\nKillMode=process\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
            quoted.join(" ")
        );
        write_private(&self.unit_path(), &unit)?;
        for args in [&["--user", "daemon-reload"][..], &["--user", "enable", "--now", UNIT_NAME]] {
            (self.run)("systemctl", args)?;
        }
        self.say(&format!("osewa service: installed systemd user unit {} (enabled, started)", self.unit_path().display()));
        self.say("osewa service: to keep it running while you're logged out: loginctl enable-linger $USER");
        Ok(())
    }

    fn install_launchd(&mut self, argv: &[String]) -> Result<(), String> {
        let items: String = argv.iter().map(|a| format!("\t\t<string>{}</string>\n", xml_escape(a))).collect();
        let log = xml_escape(&log_path().to_string_lossy());
        // ProcessType Interactive: launchd's default tier throttles the
        // server, and the shells it starts, below ones started over sshd.
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LAUNCH_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{items}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
        );
        // Made here, private: launchd would otherwise create the log's dir
        // 0744 and the log 0644 the first time it starts the agent.
        open_log().map_err(|e| format!("{}: {e}", log_path().display()))?;
        let path = self.plist_path();
        write_private(&path, &plist)?;
        // The GUI domain exists when the user is logged in at the Mac; over
        // ssh alone only the user domain may. Replace an older load either
        // way.
        let uid = unsafe { libc::getuid() };
        let p = path.to_string_lossy().into_owned();
        let mut err = String::new();
        for domain in [format!("gui/{uid}"), format!("user/{uid}")] {
            let _ = (self.run)("launchctl", &["bootout", &format!("{domain}/{LAUNCH_LABEL}")]);
            match (self.run)("launchctl", &["bootstrap", &domain, &p]) {
                Ok(()) => {
                    self.say(&format!("osewa service: installed launchd agent {p} (loaded in {domain}, started)"));
                    return Ok(());
                }
                Err(e) => err = e,
            }
        }
        Err(err)
    }

    pub fn uninstall(&mut self) -> Result<(), String> {
        match self.os_name.as_str() {
            "linux" => {
                let _ = (self.run)("systemctl", &["--user", "disable", "--now", UNIT_NAME]);
                remove(&self.unit_path())?;
                let _ = (self.run)("systemctl", &["--user", "daemon-reload"]);
                self.say(&format!("osewa service: removed {}", self.unit_path().display()));
            }
            "darwin" => {
                let uid = unsafe { libc::getuid() };
                let _ = (self.run)("launchctl", &["bootout", &format!("gui/{uid}/{LAUNCH_LABEL}")]);
                let _ = (self.run)("launchctl", &["bootout", &format!("user/{uid}/{LAUNCH_LABEL}")]);
                remove(&self.plist_path())?;
                self.say(&format!("osewa service: removed {}", self.plist_path().display()));
            }
            os => return Err(format!("no service manager support on {os}")),
        }
        Ok(())
    }

    /// Restarts the server with the binary now installed: the service in
    /// place (launchd's kickstart, systemd's restart), which keeps it loaded
    /// whatever becomes of this process, else a daemon (with default
    /// flags). Sessions live in their holders and survive.
    pub fn restart(&mut self) -> Result<(), String> {
        match self.os_name.as_str() {
            "darwin" => {
                let uid = unsafe { libc::getuid() };
                for domain in [format!("gui/{uid}"), format!("user/{uid}")] {
                    if (self.run)("launchctl", &["kickstart", "-k", &format!("{domain}/{LAUNCH_LABEL}")]).is_ok() {
                        self.say(&format!("osewa service: restarted {domain}/{LAUNCH_LABEL}"));
                        return Ok(());
                    }
                }
            }
            "linux" => {
                if (self.run)("systemctl", &["--user", "restart", UNIT_NAME]).is_ok() {
                    self.say(&format!("osewa service: restarted {UNIT_NAME}"));
                    return Ok(());
                }
            }
            _ => {}
        }
        if running_server().is_none() {
            return Err("no service and no server running".into());
        }
        stop_daemon()?;
        let (mut a, mut b) = (self.stdout.clone(), self.stdout.clone());
        if start_daemon(&self.exe, &[], &mut a, &mut b) != 0 {
            return Err(format!("the server didn't start again; see {}", log_path().display()));
        }
        Ok(())
    }

    /// Restarts from a process of its own session after this one returns:
    /// run by the server (osewa update's exec), a restart would otherwise
    /// end with the server it stops, as the server signals its execs'
    /// process groups as it closes, before it could start it again.
    fn restart_detached(&mut self) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        let log = open_log().map_err(|e| e.to_string())?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 1; exec \"$0\" service restart"]).arg(&self.exe).stdout(log).stderr(err);
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        self.say(&format!("osewa service: restarting in 1 s (pid {}, log {})", child.id(), log_path().display()));
        Ok(())
    }
}

/// `osewa service install [server flags]`, `osewa service uninstall` and
/// `osewa service restart [--detach]`.
pub(crate) fn service_main(args: &[String], stdout: Out, mut stderr: Out) -> i32 {
    if args.is_empty() || !matches!(args[0].as_str(), "install" | "uninstall" | "restart") {
        let _ = writeln!(
            stderr,
            "usage: osewa service install [--listen addr] [--name N] [--no-compress] [--no-forward] [--no-exec] [--no-quic] | uninstall | restart [--detach]"
        );
        return 2;
    }
    let r = Service::new(stdout).and_then(|mut sv| match args[0].as_str() {
        "install" => sv.install(&args[1..]),
        "uninstall" => sv.uninstall(),
        _ if args.get(1).is_some_and(|a| a == "--detach") => sv.restart_detached(),
        _ => sv.restart(),
    });
    match r {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(stderr, "osewa service: {}", printable_lines(&e));
            1
        }
    }
}

/// Stops a running server and waits for it to let go of the lock (and its
/// port). Sessions live in their holders and survive.
pub(crate) fn stop_daemon() -> Result<(), String> {
    let Some(pid) = running_server() else { return Ok(()) };
    if pid <= 0 {
        return Err(format!("a server is running but its pid is unknown ({})", pid_path().display()));
    }
    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("stopping server pid {pid}: {e}"));
        }
    }
    let deadline = Instant::now() + DAEMON_WAIT;
    loop {
        if running_server().is_none() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("server pid {pid} didn't stop"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn remove(p: &Path) -> Result<(), String> {
    match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

fn write_private(path: &Path, content: &str) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(d) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d).map_err(|e| e.to_string())?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(content.as_bytes()).map_err(|e| e.to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Formats args quoted, in brackets: ["a" "b"].
fn quote_list(args: &[String]) -> String {
    format!("[{}]", args.iter().map(|a| osewa_wire::quote(a)).collect::<Vec<_>>().join(" "))
}
