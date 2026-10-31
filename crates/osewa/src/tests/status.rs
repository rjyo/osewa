//! Program status lines (the status notice is the attach UI's).

use super::security::no_controls;
use crate::commands::print_sessions;
use crate::status::status_text;
use osewa_wire as wire;

#[test]
fn status_text_cases() {
    for (state, kind, msg, progress, want) in [
        ("working", "", "", Some(40), "working 40%".to_string()),
        ("working", "", "", None, "working".into()),
        ("blocked", "permission", "Apply?", None, "blocked: permission — Apply?".into()),
        ("blocked", "", "Apply?", None, "blocked: Apply?".into()),
        ("done", "", "", None, "done".into()),
        ("error", "", "build failed", None, "error: build failed".into()),
        ("idle", "", "", Some(10), "idle".into()),
        ("clear", "", "x", None, "".into()),
        ("", "", "", None, "".into()),
    ] {
        assert_eq!(status_text(state, kind, msg, progress), want, "{state:?} {kind:?} {msg:?}");
    }
    // The message is the remote program's: no controls, and short.
    let msg = format!("\x1b]52;c;x\x07{}", "y".repeat(60));
    assert_eq!(status_text("blocked", "question", &msg, None), format!("blocked: question — ?]52;c;x?{}…", "y".repeat(30)));
    assert_eq!(status_text("blocked", "perm\x1b[2J", "", None), "blocked: perm?[2J");
}

#[test]
fn ls_status_column() {
    let st = |state: &str, kind: &str, app: &str, msg: &str, progress| wire::ProgramStatus {
        state: state.into(),
        kind: kind.into(),
        app: app.into(),
        msg: msg.into(),
        progress,
        ..Default::default()
    };
    let s = |id: &str, status| wire::SessionInfo { id: id.into(), cols: 80, rows: 24, status: Some(status), ..Default::default() };
    let mut b = Vec::new();
    print_sessions(
        &mut b,
        &[
            s("aa", st("blocked", "permission", "claude-code", "Apply?", None)),
            s("bb", st("working", "", "", "", Some(40))),
            s("cc", st("done", "", "", "x\u{9d}52;c;aGk=\x07", None)),
        ],
    )
    .unwrap();
    let out = String::from_utf8(b).unwrap();
    for want in ["blocked: permission — Apply?", "working 40%", "done: x?52;c;aGk=?"] {
        assert!(out.contains(want), "missing {want:?} in\n{out}");
    }
    no_controls("ls", &out.replace('\n', ""));
}
