//! Program status (OSC 7501).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use osewa_wire as wire;

use super::catchup::session_cfg;
use super::helpers::*;
use crate::session::Session;
use crate::Config;

fn status_session() -> Rc<RefCell<Session>> {
    unit_session(session_cfg(80, 24, 1 << 20))
}

fn osc7501(body: &str) -> String {
    format!("\x1b]7501;{body}\x1b\\")
}

fn status(s: &Rc<RefCell<Session>>) -> Option<wire::ProgramStatus> {
    s.borrow().info().status
}

#[test]
fn status_root_record() {
    let s = status_session();
    assert!(status(&s).is_none(), "status before any report");
    let out = |b: String| s.borrow_mut().handle_output(b.as_bytes());
    out(osc7501("state=working:app=cargo:progress=40:msg=QnVpbGRpbmc="));
    let st = status(&s);
    assert!(
        st.as_ref().is_some_and(|st| st.state == "working" && st.app == "cargo" && st.msg == "Building" && st.progress == Some(40)),
        "status {st:?}"
    );
    // A record with an id is a task, not the program.
    out(osc7501("state=error:id=deploy"));
    out(osc7501("state=clear:id=deploy"));
    let st = status(&s);
    assert!(st.as_ref().is_some_and(|st| st.state == "working"), "id'd records touched the root: {st:?}");
    out(osc7501("state=blocked:kind=permission:app=claude-code"));
    let st = status(&s);
    assert!(
        st.as_ref().is_some_and(|st| st.state == "blocked" && st.kind == "permission" && st.progress.is_none()),
        "replace: {st:?}"
    );
    out(osc7501("state=clear"));
    let st = status(&s);
    assert!(st.is_none(), "clear left {st:?}");
}

/// A new prompt (OSC 133;A) ends working and blocked, not done or idle; and
/// a status and a prompt in one read keep their order.
#[test]
fn status_ends_at_prompt() {
    let prompt = "\x1b]133;A\x07$ ";
    for (out, want) in [
        (osc7501("state=working") + prompt, ""),
        (osc7501("state=blocked:kind=question") + prompt, ""),
        (osc7501("state=done") + prompt, "done"),
        (osc7501("state=idle") + prompt, "idle"),
        (prompt.to_string() + &osc7501("state=working"), "working"),
        (osc7501("state=working") + "\x1b]133;B\x07", "working"),
    ] {
        let s = status_session();
        s.borrow_mut().handle_output(out.as_bytes());
        let got = status(&s).map(|st| st.state).unwrap_or_default();
        assert_eq!(got, want, "{out:?}");
    }
    // A prompt mark split across reads, at every byte.
    let full = osc7501("state=working") + prompt;
    for cut in 0..=full.len() {
        let s = status_session();
        s.borrow_mut().handle_output(&full.as_bytes()[..cut]);
        s.borrow_mut().handle_output(&full.as_bytes()[cut..]);
        let st = status(&s);
        assert!(st.is_none(), "cut {cut}: {st:?} survived the prompt");
    }
}

/// End to end: session.info reports it, and the child's exit ends working.
#[test]
fn status_in_info_and_exit() {
    let dir = TempDir::new();
    let p = payload(&dir, "st", (osc7501("state=working:app=make") + "go\r\n").as_bytes());
    let h = start(Config {
        run_dir: dir.0.join("run"),
        argv: sh(&format!("cat {p}; read x; exit 0")),
        exit_retention: Duration::from_secs(60),
        ..Default::default()
    });
    let mut c = h.dial("dev1");
    c.attach(wire::Request { own: true, ..Default::default() });
    c.wait_text("go");
    let st = c.info().status;
    assert!(st.as_ref().is_some_and(|st| st.state == "working" && st.app == "make"), "info status {st:?}");
    c.text_input(1, "\r");
    c.wait_for("exit", |c| c.ended.is_some());
    let st = c.info().status;
    assert!(st.is_none(), "working survived the exit: {st:?}");
}
