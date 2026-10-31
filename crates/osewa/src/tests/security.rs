//! Printing remote strings safely.

use super::*;
use crate::commands::{owner_name, print_sessions};
use crate::vtfilter::printable_lines;
use osewa_wire as wire;

#[track_caller]
pub(crate) fn no_controls(what: &str, s: &str) {
    for c in s.chars() {
        assert!(!c.is_control() || c == '\n', "{what} carries control {:?}: {s:?}", c);
    }
}

/// What the host sends is printed without its control characters.
#[test]
fn ls_prints_remote_strings_safely() {
    let hostile = "x\u{9b}31mred\u{9d}52;c;aGk=\x07\x1b]0;pwned\x1b\\";
    let mut b = Vec::new();
    print_sessions(
        &mut b,
        &[wire::SessionInfo { id: "id\x1b[2J".into(), title: hostile.into(), owner: "dev\x1b[31m-abcd".into(), cols: 80, rows: 24, ..Default::default() }],
    )
    .unwrap();
    let out = String::from_utf8(b).unwrap();
    no_controls("ls", &out.replace('\n', ""));
    assert!(out.contains("x?31mred?52;c;aGk=??]0;pwned?\\") && out.contains("dev?[31m"), "unexpected rendering:\n{out}");
    assert!(!owner_name("evil\x1b]52;c;x\x07-ab12").chars().any(char::is_control), "owner name");
}

#[test]
fn errors_printed_safely() {
    assert_eq!(printable_lines("several sessions:\n\x1b]52;c;x\x07\nok"), "several sessions:\n?]52;c;x?\nok");
}

/// An ssh target that ssh would read as an option is refused, before ssh
/// runs (and "--" would keep ssh from reading it as an option even so).
#[test]
fn ssh_target_is_not_an_option() {
    let dir = TempDir::new();
    let (e, _, _) = env(dir.path(), "");
    assert!(block(e.login(&ctx(), &args(&["--no-install", "--", "-oProxyCommand=x"]))).is_err(), "login accepted an option as target");
}
