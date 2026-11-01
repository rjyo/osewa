//! The server's stderr, written by a thread of its own. Everything the
//! server logs while serving (its own lines, holders' stderr) goes through
//! here: a write to a full stderr pipe blocks, and a blocked runtime
//! freezes every connection and timer. Past a bounded queue chunks are
//! dropped, and counted in a note once the writer catches up.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::OnceLock;

/// Chunks waiting for the writer thread.
const QUEUE: usize = 256;

static DROPPED: AtomicU64 = AtomicU64::new(0);

enum Msg {
    Bytes(Vec<u8>),
    /// Answered once everything queued before it is written.
    Flush(SyncSender<()>),
}

fn sender() -> Option<&'static SyncSender<Msg>> {
    static TX: OnceLock<Option<SyncSender<Msg>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<Msg>(QUEUE);
        let spawned = std::thread::Builder::new().name("osewa-server-log".into()).spawn(move || {
            let mut err = io::stderr();
            for m in rx {
                let dropped = DROPPED.swap(0, Ordering::Relaxed);
                if dropped > 0 {
                    let _ = writeln!(err, "osewa server: {dropped} chunks of log dropped (stderr too slow)");
                }
                match m {
                    Msg::Bytes(b) => {
                        let _ = err.write_all(&b);
                    }
                    Msg::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        spawned.ok().map(|_| tx)
    })
    .as_ref()
}

/// Queues bytes for stderr, or drops (and counts) them; never blocks.
pub(crate) fn bytes(chunk: Vec<u8>) {
    match sender().map(|tx| tx.try_send(Msg::Bytes(chunk))) {
        Some(Ok(())) => {}
        Some(Err(TrySendError::Full(_))) | Some(Err(TrySendError::Disconnected(_))) | None => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Waits, at most d, for what is queued to be written: for a server
/// about to exit, off the runtime.
pub(crate) fn flush(d: std::time::Duration) {
    let Some(tx) = sender() else { return };
    let (done, wait) = sync_channel(1);
    if tx.send(Msg::Flush(done)).is_ok() {
        let _ = wait.recv_timeout(d);
    }
}

/// One line for stderr.
pub(crate) fn line(s: &str) {
    let mut b = Vec::with_capacity(s.len() + 1);
    b.extend_from_slice(s.as_bytes());
    b.push(b'\n');
    bytes(b);
}

/// stderr as a Write that never blocks (LimitedLog's).
pub(crate) struct Stderr;

impl Write for Stderr {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        bytes(b.to_vec());
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Logs a formatted line through the writer thread.
macro_rules! log {
    ($($t:tt)*) => { $crate::log::line(&format!($($t)*)) };
}
pub(crate) use log;

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that can't keep up costs dropped chunks, never a blocked
    /// caller: queueing far past the bound returns at once.
    #[test]
    fn never_blocks() {
        let t0 = std::time::Instant::now();
        for _ in 0..(QUEUE * 50) {
            bytes(Vec::new());
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "queueing took {:?}", t0.elapsed());
        flush(std::time::Duration::from_secs(5));
    }
}
