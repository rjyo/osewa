//! Puts osewa on a remote host over SSH, the way mosh relies on
//! mosh-server being there: it detects the host's OS and architecture,
//! keeps an osewa that is already the same build, and otherwise copies the
//! matching binary to ~/.local/bin/osewa.
//!
//! "Same build" means the same SHA-256. Version strings don't identify a
//! development build, and the protocol and snapshot format are only stable
//! within one build of libghostty, so anything else is replaced.

use std::process::Stdio;
use std::sync::Arc;

use ring::digest;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::Error;

/// Where [`remote`] installs osewa, relative to the remote home. Login
/// shells often leave ~/.local/bin off PATH for `ssh host cmd`, so callers
/// run `"~/" + PATH` rather than relying on PATH.
pub const PATH: &str = ".local/bin/osewa";

/// Marks the script's own lines, so shell startup noise (a .bashrc that
/// prints) can't be mistaken for them.
const PREFIX: &str = "osewa-install";

/// The osewa build for a host platform, named as the release binaries are
/// ("linux", "darwin"; "amd64", "arm64"): osewa-<os>-<arch>.
pub type BinaryFor = Arc<dyn Fn(&str, &str) -> Result<Vec<u8>, Error> + Send + Sync>;

/// The script that runs under sh -c on the host. It reports what is there,
/// then reads "<size> <sha256>" and that many bytes, or "0" to stop.
/// Everything happens over one SSH connection, so the user authenticates
/// once.
///
/// ssh hands the command to the user's login shell, so the script is one
/// line with no single quotes or backslashes: sh -c '<script>' then means
/// the same to sh, bash, zsh, fish and tcsh.
fn script() -> String {
    [
        "set -e;",
        // Whatever the account's umask, nobody else may write the
        // directory or the binary that will run as this user.
        "umask 022;",
        &format!("p={PREFIX};"),
        &format!("bin=\"$HOME/{PATH}\";"),
        r#"sum() { (sha256sum "$1" || shasum -a 256 "$1") 2>/dev/null | cut -d" " -f1; };"#,
        r#"for f in "$(command -v osewa 2>/dev/null || true)" "$bin"; do"#,
        r#"if [ -n "$f" ] && [ -f "$f" ] && [ -x "$f" ]; then echo "$p have $(sum "$f") $f"; fi;"#,
        "done;",
        r#"echo "$p host $(uname -s) $(uname -m)";"#,
        "read n want;",
        r#"[ "$n" = 0 ] && exit 0;"#,
        r#"mkdir -p "$(dirname "$bin")";"#,
        r#"tmp="$bin.tmp.$$";"#,
        // A function, so $tmp is quoted when the trap runs: a home with a
        // space must not turn into two paths to remove.
        r#"cleanup() { rm -f "$tmp"; };"#,
        "trap cleanup EXIT;",
        r#"head -c "$n" >"$tmp";"#,
        r#"got=$(sum "$tmp");"#,
        r#"size=$(wc -c <"$tmp" | tr -d " ");"#,
        // Without sha256sum or shasum only the size can be checked.
        r#"if [ "$size" != "$n" ] || { [ -n "$got" ] && [ "$got" != "$want" ]; }; then echo "$p bad $size $got"; exit 1; fi;"#,
        r#"chmod 755 "$tmp";"#,
        r#"mv -f "$tmp" "$bin";"#,
        r#"echo "$p done $bin""#,
    ]
    .join(" ")
}

/// What [`remote`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The binary was copied to ~/.local/bin/osewa.
    pub installed: bool,
    /// It was copied while another build was already on the host (at that
    /// path or on PATH). A server already running from it keeps the old
    /// build until restarted.
    pub replaced: bool,
}

/// Makes sure the host at ssh_target has the osewa that binary_for returns
/// for its platform. binary_for is called only once the platform is known.
/// It runs ssh on the client's runtime, so any executor can await it;
/// dropping the future kills ssh.
pub async fn remote(ssh_target: &str, binary_for: BinaryFor) -> Result<Outcome, Error> {
    remote_with(vec!["ssh".into()], ssh_target, binary_for).await
}

/// remote with the ssh client and its leading arguments; tests use a fake.
pub(crate) async fn remote_with(ssh: Vec<String>, ssh_target: &str, binary_for: BinaryFor) -> Result<Outcome, Error> {
    // A target like -oProxyCommand=... would be an ssh option that runs a
    // local command; "--" ends ssh's options.
    if ssh_target.is_empty() || ssh_target.starts_with('-') {
        return Err(Error::Other(format!("install: bad ssh target {}", osewa_wire::go_quote(ssh_target))));
    }
    let target = ssh_target.to_string();
    crate::rt::run_local(move || async move {
        let mut cmd = tokio::process::Command::new(&ssh[0]);
        cmd.args(&ssh[1..])
            .arg("--")
            .arg(&target)
            .arg(format!("sh -c '{}'", script()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| Error::Other(format!("install: {e}")))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let errb = async move {
            let mut b = Vec::new();
            let _ = stderr.read_to_end(&mut b).await;
            b
        };
        let main = async move {
            let mut out = BufReader::new(stdout);
            let res = converse(&mut out, stdin, &binary_for).await;
            // Drain so the script never blocks on a full pipe before it exits.
            let _ = tokio::io::copy(&mut out, &mut tokio::io::sink()).await;
            res
        };
        let (mut res, errb) = tokio::join!(main, errb);
        let status = child.wait().await;
        if res.is_ok() {
            match status {
                Ok(s) if s.success() => {}
                Ok(s) => res = Err(Error::Other(exit_status(s))),
                Err(e) => res = Err(e.into()),
            }
        }
        res.map_err(|e| {
            let msg = String::from_utf8_lossy(&errb);
            let msg = msg.trim();
            if msg.is_empty() {
                Error::Other(format!("install on {target}: {e}"))
            } else {
                Error::Other(format!("install on {target}: {e}: {msg}"))
            }
        })
    })
    .await
}

/// Words a failed exit as users see it from every osewa: "exit status N"
/// or "signal: N".
pub(crate) fn exit_status(s: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (s.code(), s.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(sig)) => format!("signal: {sig}"),
        _ => s.to_string(),
    }
}

/// Speaks the script's side protocol: reads what the host has, then sends
/// either "0" or the binary and reads the outcome.
async fn converse<R, W>(out: &mut R, mut input: W, binary_for: &BinaryFor) -> Result<Outcome, Error>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (have, sysname, machine) = probe(out).await?;
    let bin = match binary_from(&sysname, &machine, binary_for) {
        Ok(b) => b,
        Err(e) => {
            let _ = send(&mut input, b"0\n").await;
            return Err(e);
        }
    };
    let sum: String = digest::digest(&digest::SHA256, &bin).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    if have.contains(&sum) {
        send(&mut input, b"0\n").await?;
        return Ok(Outcome::default());
    }
    send(&mut input, format!("{} {sum}\n", bin.len()).as_bytes()).await?;
    send(&mut input, &bin).await?;
    // EOF ends head -c early if the connection breaks; the size check then
    // refuses the partial file.
    let _ = input.shutdown().await;
    drop(input);
    let f = script_line(out).await?;
    match f[0].as_str() {
        "done" => Ok(Outcome { installed: true, replaced: !have.is_empty() }),
        "bad" => Err(Error::Other(format!("copy arrived damaged ({})", f[1..].join(" ")))),
        _ => Err(Error::Other(format!("unexpected {} from host", osewa_wire::go_quote(&f.join(" "))))),
    }
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, b: &[u8]) -> Result<(), Error> {
    w.write_all(b).await?;
    w.flush().await?;
    Ok(())
}

/// Reads the script's report: the sums of osewa builds already there, then
/// `uname -s` and `uname -m`.
async fn probe<R: AsyncBufRead + Unpin>(out: &mut R) -> Result<(Vec<String>, String, String), Error> {
    let mut have = Vec::new();
    loop {
        let f = script_line(out).await?;
        match f[0].as_str() {
            "have" if f.len() >= 2 => have.push(f[1].clone()),
            "host" if f.len() == 3 => return Ok((have, f[1].clone(), f[2].clone())),
            _ => return Err(Error::Other(format!("unexpected {} from host", osewa_wire::go_quote(&f.join(" "))))),
        }
    }
}

/// The whole binary for the host's platform.
fn binary_from(sysname: &str, machine: &str, binary_for: &BinaryFor) -> Result<Vec<u8>, Error> {
    let (os_name, arch_name) = platform(sysname, machine)?;
    let bin = binary_for(os_name, arch_name)?;
    if bin.is_empty() {
        return Err(Error::Other(format!("empty binary for {os_name}/{arch_name}")));
    }
    Ok(bin)
}

/// The fields after the prefix of the script's next line, skipping
/// anything else the remote shell printed.
async fn script_line<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Vec<String>, Error> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line).await?;
        let s = String::from_utf8_lossy(&line);
        let f: Vec<String> = s.split_whitespace().map(str::to_string).collect();
        if f.len() >= 2 && f[0] == PREFIX {
            return Ok(f[1..].to_vec());
        }
        if n == 0 {
            return Err(Error::Other("host closed the connection".into()));
        }
    }
}

/// Maps `uname -s` and `uname -m` to the release binaries' os and arch.
pub fn platform(sysname: &str, machine: &str) -> Result<(&'static str, &'static str), Error> {
    let os_name = match sysname {
        "Linux" => "linux",
        "Darwin" => "darwin",
        _ => return Err(Error::Other(format!("unsupported OS {sysname}"))),
    };
    let arch_name = match machine {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        _ => return Err(Error::Other(format!("unsupported architecture {machine}"))),
    };
    Ok((os_name, arch_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::testutil::TempDir;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime};

    /// A fake SSH target: a home directory, and a bin directory put in
    /// front of PATH with a uname that reports the given platform.
    struct Host {
        _dir: TempDir,
        home: PathBuf,
        bin: PathBuf,
        ssh: PathBuf,
    }

    /// An ssh that runs the remote command locally, the way sshd does:
    /// through the login shell with -c. pre runs before it, like a noisy
    /// shell startup file.
    fn fake_ssh(sysname: &str, machine: &str, shell: &str, pre: &str) -> Host {
        let dir = TempDir::new();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let ssh = dir.path().join("ssh");
        let h = Host { home, bin, ssh, _dir: dir };
        h.script("uname", &format!(r#"case "$1" in -s) echo {sysname} ;; -m) echo {machine} ;; esac"#));
        let body = format!(
            r#"#!/bin/sh
[ "$1" = -- ] || {{ echo "ssh: options not ended before the target" >&2; exit 255; }}
target=$2
shift 2
[ "$target" = unreachable ] && {{ echo "ssh: connect to host unreachable: Connection refused" >&2; exit 255; }}
{pre}
HOME={} PATH={}:/usr/bin:/bin exec {shell} -c "$*"
"#,
            osewa_wire::go_quote(h.home.to_str().unwrap()),
            osewa_wire::go_quote(h.bin.to_str().unwrap()),
        );
        write_exec(&h.ssh, &body);
        h
    }

    fn write_exec(p: &Path, body: &str) {
        fs::write(p, body).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    impl Host {
        fn script(&self, name: &str, body: &str) {
            write_exec(&self.bin.join(name), &format!("#!/bin/sh\n{body}\n"));
        }

        fn installed(&self) -> PathBuf {
            self.home.join(PATH)
        }

        fn remote(&self, target: &str, b: &Binaries) -> Result<Outcome, Error> {
            crate::block_on(crate::rt::timeout(
                Duration::from_secs(30),
                remote_with(vec![self.ssh.to_str().unwrap().into()], target, b.get()),
            ))
            .expect("install timed out")
        }

        fn leftovers(&self) -> Vec<String> {
            let dir = self.installed().parent().unwrap().to_path_buf();
            fs::read_dir(dir)
                .map(|d| {
                    d.filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.starts_with("osewa.tmp.")).collect()
                })
                .unwrap_or_default()
        }
    }

    /// Serves fixed content per platform and records what was asked.
    #[derive(Default)]
    struct Binaries {
        asked: Arc<Mutex<Vec<String>>>,
        err: Option<String>,
    }

    impl Binaries {
        fn get(&self) -> BinaryFor {
            let (asked, err) = (self.asked.clone(), self.err.clone());
            Arc::new(move |os_name: &str, arch_name: &str| {
                asked.lock().unwrap().push(format!("{os_name}/{arch_name}"));
                match &err {
                    Some(e) => Err(Error::Other(e.clone())),
                    None => Ok(content(os_name, arch_name).into_bytes()),
                }
            })
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    fn content(os_name: &str, arch_name: &str) -> String {
        // Big enough that a pipe can't hold it all.
        format!("#!/bin/sh\necho osewa {os_name}/{arch_name}\n#{}\n", "x".repeat(300 << 10))
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn install_fresh() {
        let h = fake_ssh("Linux", "aarch64", "/bin/sh", "");
        let b = Binaries::default();
        let res = h.remote("dev@box", &b).unwrap();
        assert_eq!(res, Outcome { installed: true, replaced: false });
        assert_eq!(b.asked(), ["linux/arm64"]);
        assert!(read(&h.installed()) == content("linux", "arm64"), "installed binary differs");
        assert_eq!(fs::metadata(h.installed()).unwrap().permissions().mode() & 0o777, 0o755);
        let out = std::process::Command::new(h.installed()).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "osewa linux/arm64\n");
        assert!(h.leftovers().is_empty(), "temp files left: {:?}", h.leftovers());
    }

    #[test]
    fn install_keeps_same_build() {
        let h = fake_ssh("Darwin", "arm64", "/bin/sh", "");
        fs::create_dir_all(h.installed().parent().unwrap()).unwrap();
        write_exec(&h.installed(), &content("darwin", "arm64"));
        let old = SystemTime::now() - Duration::from_secs(3600);
        let f = fs::File::options().write(true).open(h.installed()).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let b = Binaries::default();
        assert_eq!(h.remote("mac", &b).unwrap(), Outcome::default());
        assert_eq!(fs::metadata(h.installed()).unwrap().modified().unwrap(), old, "same build was rewritten");
    }

    #[test]
    fn install_replaces_other_build() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        fs::create_dir_all(h.installed().parent().unwrap()).unwrap();
        write_exec(&h.installed(), "#!/bin/sh\necho old\n");
        let b = Binaries::default();
        assert_eq!(h.remote("box", &b).unwrap(), Outcome { installed: true, replaced: true });
        assert!(read(&h.installed()) == content("linux", "amd64"), "old build kept");
    }

    // An osewa elsewhere on PATH (a package, /usr/local/bin) that is the
    // same build is used as is.
    #[test]
    fn install_keeps_same_build_on_path() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        write_exec(&h.bin.join("osewa"), &content("linux", "amd64"));
        let b = Binaries::default();
        assert_eq!(h.remote("box", &b).unwrap(), Outcome::default());
        assert!(!h.installed().exists(), "installed despite a matching osewa on PATH");
    }

    // A different build elsewhere on PATH counts as replaced: a server may
    // be running from it.
    #[test]
    fn install_other_build_on_path() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        write_exec(&h.bin.join("osewa"), "#!/bin/sh\necho old\n");
        let b = Binaries::default();
        assert_eq!(h.remote("box", &b).unwrap(), Outcome { installed: true, replaced: true });
        assert!(read(&h.installed()) == content("linux", "amd64"), "not installed");
    }

    #[test]
    fn install_unsupported_platform() {
        for (s, m) in [("FreeBSD", "amd64"), ("Linux", "riscv64")] {
            let h = fake_ssh(s, m, "/bin/sh", "");
            let b = Binaries::default();
            let err = h.remote("box", &b).unwrap_err();
            assert!(err.to_string().contains("unsupported"), "{s} {m}: {err}");
            assert!(b.asked().is_empty(), "asked for a binary: {:?}", b.asked());
            assert!(!h.installed().exists(), "installed something");
        }
    }

    #[test]
    fn install_binary_error() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        let b = Binaries { err: Some("no build for linux/amd64".into()), ..Default::default() };
        let err = h.remote("box", &b).unwrap_err();
        assert!(err.to_string().contains("no build"), "{err}");
        assert!(!h.installed().exists(), "installed something");
    }

    // A shell startup file that prints must not confuse the exchange.
    #[test]
    fn install_noisy_shell() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", r#"echo "Welcome to box"; echo "have fun""#);
        let b = Binaries::default();
        h.remote("box", &b).unwrap();
        assert!(read(&h.installed()) == content("linux", "amd64"), "not installed");
    }

    // A copy that arrives short is refused and the old binary stays,
    // whether the host can hash it or only measure it.
    #[test]
    fn install_damaged_copy_hash() {
        damaged(false);
    }

    #[test]
    fn install_damaged_copy_size() {
        damaged(true);
    }

    fn damaged(no_hash: bool) {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        if no_hash {
            h.script("sha256sum", "exit 127");
            h.script("shasum", "exit 127");
        }
        fs::create_dir_all(h.installed().parent().unwrap()).unwrap();
        write_exec(&h.installed(), "old");
        // A head that loses the last byte, as a broken connection would.
        h.script("head", "exec /usr/bin/head -c $(($2 - 1))");
        let b = Binaries::default();
        let err = h.remote("box", &b).unwrap_err();
        assert!(err.to_string().contains("damaged"), "{err}");
        assert_eq!(read(&h.installed()), "old", "damaged copy replaced the old binary");
        assert!(h.leftovers().is_empty(), "temp files left: {:?}", h.leftovers());
    }

    // Without sha256sum or shasum, a whole copy is still accepted on its size.
    #[test]
    fn install_without_hash_tool() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        h.script("sha256sum", "exit 127");
        h.script("shasum", "exit 127");
        let b = Binaries::default();
        h.remote("box", &b).unwrap();
        assert!(read(&h.installed()) == content("linux", "amd64"), "not installed");
    }

    #[test]
    fn install_ssh_error() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        let b = Binaries::default();
        let err = h.remote("unreachable", &b).unwrap_err();
        assert!(err.to_string().contains("Connection refused"), "{err}");
    }

    #[test]
    fn install_cancel() {
        // Like a real ssh hung on a dead network: the process itself waits.
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "exec sleep 30");
        let b = Binaries::default();
        let start = Instant::now();
        let ssh = vec![h.ssh.to_str().unwrap().to_string()];
        let res = crate::block_on(crate::rt::timeout(Duration::from_millis(200), remote_with(ssh, "box", b.get())));
        assert!(res.is_none() || res.unwrap().is_err(), "no error after cancel");
        assert!(start.elapsed() < Duration::from_secs(10), "cancel did not stop ssh");
    }

    // ssh passes the command through the user's login shell, whatever it is.
    #[test]
    fn install_login_shells() {
        for sh in ["bash", "zsh", "fish", "tcsh"] {
            let Some(path) = which(sh) else {
                eprintln!("{sh} not installed, skipped");
                continue;
            };
            let h = fake_ssh("Linux", "x86_64", path.to_str().unwrap(), "");
            let b = Binaries::default();
            h.remote("box", &b).unwrap_or_else(|e| panic!("{sh}: {e}"));
            assert!(read(&h.installed()) == content("linux", "amd64"), "{sh}: not installed");
        }
    }

    fn which(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    #[test]
    fn install_rejects_option_target() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        for target in ["-oProxyCommand=touch /tmp/pwned", "-F/dev/null", ""] {
            let b = Binaries::default();
            let err = h.remote(target, &b).unwrap_err();
            assert!(err.to_string().contains("bad ssh target"), "{target:?}: {err}");
        }
    }

    // A home with a space must not make the cleanup trap remove other paths.
    #[test]
    fn install_home_with_space() {
        let mut h = fake_ssh("Linux", "x86_64", "/bin/sh", "");
        let parent = h.home.parent().unwrap().to_path_buf();
        let spaced = parent.join("my home");
        fs::rename(&h.home, &spaced).unwrap();
        // What an unquoted "rm -f $tmp" would split off as its first word.
        let victim = parent.join("my");
        fs::write(&victim, "keep").unwrap();
        let body = read(&h.ssh).replace(
            &format!("HOME={}", osewa_wire::go_quote(h.home.to_str().unwrap())),
            &format!("HOME={}", osewa_wire::go_quote(spaced.to_str().unwrap())),
        );
        write_exec(&h.ssh, &body);
        h.home = spaced;

        let b = Binaries::default();
        h.remote("box", &b).unwrap();
        assert!(read(&h.installed()) == content("linux", "amd64"), "not installed");
        assert!(victim.exists(), "cleanup removed a path outside the install");
    }

    // A permissive umask on the host must not leave ~/.local/bin or the
    // binary writable by others.
    #[test]
    fn install_ignores_lax_umask() {
        let h = fake_ssh("Linux", "x86_64", "/bin/sh", "umask 000");
        let b = Binaries::default();
        h.remote("box", &b).unwrap();
        let inst = h.installed();
        for p in [inst.parent().unwrap(), inst.parent().unwrap().parent().unwrap(), &inst] {
            let m = fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(m & 0o022, 0, "{}: {m:o}", p.display());
        }
    }

    #[test]
    fn script_is_shell_neutral() {
        assert!(!script().contains(['\'', '\\', '\n']), "script must have no single quotes, backslashes or newlines");
    }

    #[test]
    fn platforms() {
        for (s, m, os, arch) in [
            ("Linux", "x86_64", "linux", "amd64"),
            ("Linux", "aarch64", "linux", "arm64"),
            ("Linux", "arm64", "linux", "arm64"),
            ("Darwin", "arm64", "darwin", "arm64"),
            ("Darwin", "x86_64", "darwin", "amd64"),
        ] {
            assert_eq!(platform(s, m).unwrap(), (os, arch), "{s} {m}");
        }
    }

    #[test]
    fn unexpected_output() {
        for out in [format!("{PREFIX} what\n"), format!("{PREFIX} host Linux x86_64\n{PREFIX} what\n"), String::new()] {
            let b = Binaries::default();
            let mut r = BufReader::new(out.as_bytes());
            let res = crate::block_on(converse(&mut r, tokio::io::sink(), &b.get()));
            assert!(res.is_err(), "{out:?}: no error");
        }
    }
}
