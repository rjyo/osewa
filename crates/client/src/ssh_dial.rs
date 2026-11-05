//! Reaches a host's server through ssh, for hosts behind a bastion: each
//! dial runs `ssh -W`, so the user's ssh_config (ProxyJump, ProxyCommand)
//! decides the route, and the connection's TLS runs through it end to end.
//! A reconnect is a new ssh, which is why nothing here may ask the user
//! anything: the terminal is in raw mode, drawing a session.

use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::dial::{Io, TcpDial};

/// Bounds what is kept of ssh's stderr; the end says why it failed.
const SAID_MAX: usize = 1024;

/// A dial that opens host:port from `target`, as `target` sees it.
pub(crate) fn dial(target: String) -> TcpDial {
    Arc::new(move |addr| {
        let args = ["-o", "BatchMode=yes", "-W", &addr, "--", &target].map(String::from);
        Box::pin(async move { spawn("ssh", &args).map(|s| Box::new(s) as Box<dyn Io>) })
    })
}

/// Runs prog, whose stdin and stdout become the stream.
fn spawn(prog: &str, args: &[String]) -> io::Result<Stream> {
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SSH_ASKPASS_REQUIRE", "never")
        .kill_on_drop(true);
    // BatchMode doesn't reach the ssh a ProxyJump starts, which would ask
    // for a password on /dev/tty. Without a controlling terminal no hop can.
    unsafe {
        c.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = c.spawn()?;
    let (input, out, err) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    Ok(Stream { _child: child, input: input.expect("piped"), out: out.expect("piped"), err: Some(err.expect("piped")), said: Vec::new() })
}

/// A running ssh as a byte stream. Dropping it kills ssh.
struct Stream {
    _child: Child,
    input: ChildStdin,
    out: ChildStdout,
    err: Option<ChildStderr>,
    said: Vec<u8>,
}

impl Stream {
    /// Reads what ssh has said, ready when it will say no more. Every read
    /// of the stream does, and a connection is always reading: left alone,
    /// a chatty ssh would fill the pipe and block with the session behind it.
    fn poll_said(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while let Some(err) = self.err.as_mut() {
            let mut b = [0u8; 512];
            let mut buf = ReadBuf::new(&mut b);
            match ready!(Pin::new(err).poll_read(cx, &mut buf)) {
                Ok(()) if !buf.filled().is_empty() => {
                    self.said.extend_from_slice(buf.filled());
                    let extra = self.said.len().saturating_sub(SAID_MAX);
                    self.said.drain(..extra);
                }
                _ => self.err = None,
            }
        }
        Poll::Ready(())
    }

    /// Why ssh went away, once it has said all it will: TLS on its own could
    /// only report an unexpected end of stream.
    fn poll_why(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Error>> {
        ready!(self.poll_said(cx));
        Poll::Ready(said_error(&self.said))
    }
}

/// ssh's last words as an error, safe to print: they can quote the remote,
/// a pre-auth banner say.
fn said_error(said: &[u8]) -> Option<io::Error> {
    let text = String::from_utf8_lossy(said);
    let text: Vec<String> = text.lines().map(|l| l.chars().filter(|c| !c.is_control()).collect::<String>()).filter(|l| !l.trim().is_empty()).collect();
    (!text.is_empty()).then(|| io::Error::other(format!("ssh: {}", text.join("; "))))
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let s = self.get_mut();
        let before = buf.filled().len();
        let _ = s.poll_said(cx);
        ready!(Pin::new(&mut s.out).poll_read(cx, buf))?;
        if buf.filled().len() > before {
            return Poll::Ready(Ok(()));
        }
        match ready!(s.poll_why(cx)) {
            Some(e) => Poll::Ready(Err(e)),
            None => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        let s = self.get_mut();
        match ready!(Pin::new(&mut s.input).poll_write(cx, b)) {
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Poll::Ready(Err(ready!(s.poll_why(cx)).unwrap_or(e))),
            r => Poll::Ready(r),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().input).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().input).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sh(script: &str) -> io::Result<Stream> {
        spawn("sh", &["-c".to_string(), script.to_string()])
    }

    fn on_runtime<F: std::future::Future<Output = ()> + 'static>(f: impl FnOnce() -> F + Send + 'static) {
        crate::block_on(crate::rt::run_local(move || async move { f().await }));
    }

    #[test]
    fn carries_bytes_both_ways() {
        on_runtime(|| async {
            let mut s = sh("exec cat").unwrap();
            s.write_all(b"through").await.unwrap();
            let mut got = [0u8; 7];
            s.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"through");
        });
    }

    /// What ssh said is the error, not an unexpected end of stream, and
    /// nothing in it can drive the terminal the error is printed on.
    #[test]
    fn failure_says_why() {
        on_runtime(|| async {
            let mut s = sh("printf 'banner\\033[2J\\nPermission denied (publickey).\\n' >&2; exit 255").unwrap();
            let e = s.read(&mut [0u8; 8]).await.unwrap_err().to_string();
            assert!(e.contains("Permission denied"), "{e}");
            assert!(!e.contains('\x1b'), "{e:?}");
        });
    }

    /// A chatty ssh or ProxyCommand must not fill stderr's pipe and block
    /// with the session behind it.
    #[test]
    fn stderr_cannot_stall_the_stream() {
        on_runtime(|| async {
            let mut s = sh("yes noise | head -c 300000 >&2; echo done; exec cat").unwrap();
            let mut got = [0u8; 5];
            let read = crate::rt::timeout(std::time::Duration::from_secs(5), s.read_exact(&mut got)).await;
            assert!(matches!(read, Some(Ok(_))), "stalled behind stderr: {read:?}");
            assert_eq!(&got, b"done\n");
        });
    }

    #[test]
    fn silent_exit_is_an_end_of_stream() {
        on_runtime(|| async {
            let mut s = sh("exit 0").unwrap();
            assert_eq!(s.read(&mut [0u8; 8]).await.unwrap(), 0);
        });
    }

    #[test]
    fn has_no_controlling_terminal() {
        on_runtime(|| async {
            let mut s = sh("if : </dev/tty; then echo tty; else echo none; fi 2>/dev/null").unwrap();
            let mut got = String::new();
            s.read_to_string(&mut got).await.unwrap();
            assert_eq!(got.trim(), "none");
        });
    }

    #[test]
    fn said_is_bounded_to_its_end() {
        assert!(said_error(b"\n \n").is_none());
        assert_eq!(said_error(b"a\r\nb\n").unwrap().to_string(), "ssh: a; b");
    }
}
