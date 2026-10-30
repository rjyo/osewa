//! nomirror_test.go

use super::*;

/// Plays an app that renders with its own libghostty: it restores the raw
/// snapshot and applies output itself.
#[derive(Clone)]
pub(super) struct App(Arc<Mutex<AppState>>);

#[derive(Default)]
pub(super) struct AppState {
    pub log: Vec<String>,
    pub screen: Vec<u8>,
    pub history: Vec<u8>,
    pub term: Option<Term>,
    /// The snapshot in progress brings history.
    pub has_history: bool,
}

impl App {
    pub fn new() -> App {
        App(Arc::default())
    }

    pub fn options(&self) -> AttachOptions {
        let (a1, a2, a3, a4, a5) = (self.0.clone(), self.0.clone(), self.0.clone(), self.0.clone(), self.0.clone());
        AttachOptions {
            cols: 40,
            rows: 5,
            on_snapshot_begin: Some(Box::new(move |b| {
                let mut a = a1.lock().unwrap();
                a.log.push("begin".into());
                // A new BEGIN drops whatever an abandoned snapshot left.
                a.screen.clear();
                a.history.clear();
                a.has_history = b.history.unwrap_or(true);
            })),
            on_snapshot_data: Some(Box::new(move |b, history| {
                let mut a = a2.lock().unwrap();
                if history {
                    a.history.extend_from_slice(b);
                } else {
                    a.screen.extend_from_slice(b);
                }
            })),
            on_ready: Some(Box::new(move || a3.lock().unwrap().log.push("ready".into()))),
            on_snapshot_end: Some(Box::new(move || {
                let mut a = a4.lock().unwrap();
                a.log.push("end".into());
                // Decode as an app does: READY, then history only if the
                // snapshot has some (a catch-up screen doesn't).
                let Ok(mut dec) = ghostty::Decoder::new() else { return };
                dec.feed(&a.screen);
                let Ok(t) = dec.ready(Default::default()) else { return };
                if a.has_history {
                    dec.feed(&a.history);
                    if dec.finish().is_err() {
                        return;
                    }
                }
                a.term = Some(Term(t));
            })),
            on_output: Some(Box::new(move |b| {
                let mut a = a5.lock().unwrap();
                a.log.push("output".into());
                if let Some(t) = a.term.as_mut() {
                    t.0.write(b);
                }
            })),
            ..Default::default()
        }
    }

    pub fn text(&self) -> String {
        self.0.lock().unwrap().term.as_ref().map(|t| t.0.text()).unwrap_or_default()
    }

    pub fn log(&self) -> Vec<String> {
        self.0.lock().unwrap().log.clone()
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, AppState> {
        self.0.lock().unwrap()
    }
}

#[test]
fn no_mirror() {
    let f = Fake::new();
    let id = f.create(40, 5);
    for _ in 0..30 {
        f.output(&id, "history line\r\n");
    }
    let a = App::new();
    let s = attach_raw(&f.dial("a"), &id, a.options());
    eventually("snapshot", || a.text().contains("history line"));
    f.output(&id, "live output");
    eventually("output", || a.text().contains("live output"));

    assert_eq!(a.log().join(" "), "begin ready end output");
    let want = f.with(&id, |fs| fs.term.0.text());
    assert_eq!(a.text(), want, "app screen");

    // Positions, acks and resume work without a mirror.
    let next = f.with(&id, |fs| fs.next);
    eventually("ack", || f.with(&id, |fs| fs.acked) == next);
    f.drop_all();
    wait_for("lost", s.lost());
    f.output(&id, " more");
    run(s.resume(&f.dial("a"))).unwrap();
    eventually("resumed output", || a.text().contains("live output more"));
    assert!(f.lock().attaches.last().unwrap().resumed, "did not resume from the position");

    // A catch-up screen snapshot brings no history bytes.
    a.lock().log.clear();
    f.screen_snapshot(&id);
    eventually("screen snapshot", || a.log().join(" ") == "begin ready end");
    let st = a.lock();
    assert!(st.history.is_empty() && !st.screen.is_empty(), "screen snapshot: {} screen bytes, {} history bytes", st.screen.len(), st.history.len());
    drop(st);
    s.close();
}
