//! A program's status (OSC 7501) in one line (Go's status.go).

use crate::vtfilter::printable;

/// Bounds a status message in `osewa ls` and notices.
pub(crate) const STATUS_MAX: usize = 40;

/// Renders a program status in one line: "working 40%", "blocked:
/// permission — Apply?", "done". The message comes from the remote program,
/// so it is made printable and cut to STATUS_MAX characters.
pub(crate) fn status_text(state: &str, kind: &str, msg: &str, progress: Option<i64>) -> String {
    if state.is_empty() || state == "clear" {
        return String::new();
    }
    let mut s = state.to_string();
    if let (Some(p), "working") = (progress, state) {
        s += &format!(" {p}%");
    }
    if !kind.is_empty() {
        s += &format!(": {}", printable(kind));
    }
    let mut msg = printable(msg).trim().to_string();
    if !msg.is_empty() {
        let chars: Vec<char> = msg.chars().collect();
        if chars.len() > STATUS_MAX {
            msg = chars[..STATUS_MAX - 1].iter().collect::<String>() + "…";
        }
        if kind.is_empty() {
            s += &format!(": {msg}");
        } else {
            s += &format!(" — {msg}");
        }
    }
    s
}
