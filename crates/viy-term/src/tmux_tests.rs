use super::*;

#[test]
fn the_mouse_is_captured_unless_tmux_says_off() {
    assert!(mouse_capture(None));
    assert!(mouse_capture(Some("1\n")));
    assert!(mouse_capture(Some("on")));
    assert!(mouse_capture(Some("")));
    assert!(!mouse_capture(Some("0\n")));
    assert!(!mouse_capture(Some("off")));
}
