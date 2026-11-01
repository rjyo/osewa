//! The background runtime the network runs on (see the crate docs), and
//! the executor-agnostic primitives the handles wait on.

use std::future::{poll_fn, Future};
use std::pin::pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::AbortHandle;

type LocalJob = Box<dyn FnOnce() + Send>;

struct Rt {
    handle: Handle,
    local: mpsc::UnboundedSender<LocalJob>,
}

fn rt() -> &'static Rt {
    static RT: OnceLock<Rt> = OnceLock::new();
    RT.get_or_init(|| {
        let (tx, mut rx) = mpsc::unbounded_channel::<LocalJob>();
        let (htx, hrx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("osewa-client".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("osewa: client runtime");
                htx.send(rt.handle().clone()).expect("osewa: client runtime");
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    while let Some(job) = rx.recv().await {
                        job();
                    }
                });
            })
            .expect("osewa: client thread");
        Rt { handle: hrx.recv().expect("osewa: client runtime"), local: tx }
    })
}

/// Runs f on the background runtime.
pub(crate) fn spawn<F: Future<Output = ()> + Send + 'static>(f: F) -> AbortHandle {
    rt().handle.spawn(f).abort_handle()
}

/// Runs a !Send future, made on the runtime thread, there: carriers are
/// `!Send`.
pub(crate) fn spawn_local<M, F>(make: M)
where
    M: FnOnce() -> F + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    let job: LocalJob = Box::new(move || {
        tokio::task::spawn_local(make());
    });
    // The runtime thread never exits, so the receiver is always there.
    let _ = rt().local.send(job);
}

/// Runs a !Send future on the runtime thread and returns its output. If
/// the returned future is dropped, the work is cancelled.
pub(crate) async fn run_local<M, F, T>(make: M) -> T
where
    M: FnOnce() -> F + Send + 'static,
    F: Future<Output = T> + 'static,
    T: Send + 'static,
{
    let (mut tx, rx) = oneshot::channel();
    spawn_local(move || async move {
        let f = make();
        tokio::select! {
            v = f => { let _ = tx.send(v); }
            _ = tx.closed() => {}
        }
    });
    rx.await.expect("osewa: client runtime dropped a task")
}

/// Sleeps on the background runtime, so any executor can wait.
pub async fn sleep(d: Duration) {
    let (tx, rx) = oneshot::channel::<()>();
    let h = spawn(async move {
        tokio::time::sleep(d).await;
        let _ = tx.send(());
    });
    let _ = rx.await;
    h.abort();
}

/// Runs f for at most d, on any executor: None if d passed first.
pub async fn timeout<F: Future>(d: Duration, f: F) -> Option<F::Output> {
    let mut f = pin!(f);
    let mut t = pin!(sleep(d));
    poll_fn(|cx| {
        if let Poll::Ready(v) = f.as_mut().poll(cx) {
            return Poll::Ready(Some(v));
        }
        if t.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future to completion on this thread, for blocking callers. Never
/// call it from a callback of this library (they run on its own threads).
pub fn block_on<F: Future>(f: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park();
    }
}

/// A flag that is set once and can be waited for.
pub(crate) struct Latch(watch::Sender<bool>);

impl Default for Latch {
    fn default() -> Latch {
        Latch(watch::Sender::new(false))
    }
}

impl Latch {
    /// Sets the flag; true if this call set it.
    pub fn set(&self) -> bool {
        self.0.send_if_modified(|v| !std::mem::replace(v, true))
    }

    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    pub fn wait(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.0.subscribe();
        async move {
            let _ = rx.wait_for(|v| *v).await;
        }
    }
}

/// Wakes waiters on every change, however many there are. Take a
/// Watcher before checking state, under the same lock as the change.
pub(crate) struct Changed(watch::Sender<u64>);

impl Default for Changed {
    fn default() -> Changed {
        Changed(watch::Sender::new(0))
    }
}

pub(crate) struct Watcher(watch::Receiver<u64>);

impl Changed {
    pub fn notify(&self) {
        self.0.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn watch(&self) -> Watcher {
        Watcher(self.0.subscribe())
    }

    pub fn public(&self) -> Watch {
        Watch(self.0.subscribe())
    }
}

/// Hears a handle's changes ([`crate::Link::watch`],
/// [`crate::Session::watch`]), on any executor: take it, read the state,
/// then wait for the next change and read again. Changes in between are
/// coalesced, never lost.
pub struct Watch(watch::Receiver<u64>);

impl Watch {
    /// Waits for a change since the last call (or since the Watch was
    /// taken). False once the handle is gone and nothing will change.
    pub async fn changed(&mut self) -> bool {
        self.0.changed().await.is_ok()
    }
}

impl Watcher {
    /// Waits for a change after this Watcher was taken.
    pub async fn changed(mut self) {
        let _ = self.0.changed().await;
    }
}
