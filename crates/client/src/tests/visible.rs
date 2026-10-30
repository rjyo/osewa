//! visible_test.go

use super::*;

#[test]
fn wait_text_ignores_scrollback() {
    let f = Fake::new();
    let id = f.create(40, 3);
    f.output(&id, "old-marker\r\n1\r\n2\r\n3\r\n4\r\n");
    let s = attach(&f.dial("a"), &id, opts(40, 3));
    s.wait_text("4");
    assert!(s.text().contains("old-marker"), "text = {:?}", s.text());
    assert!(!s.visible_text().contains("old-marker"), "visible_text = {:?}", s.visible_text());
    assert!(s.try_wait_text(Duration::from_millis(50), "old-marker").is_err(), "wait_text matched scrollback");
}
