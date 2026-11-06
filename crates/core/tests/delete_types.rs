use disk_cleanup_core::{first_line_capped, CMD_MSG_MAX};

#[test]
fn first_line_capped_should_take_the_first_nonempty_line_replace_control_chars_and_cut_on_a_char_boundary_at_200_bytes_when_given(
) {
    assert_eq!(
        first_line_capped("\nfatal: x is a submodule\nhint: y"),
        "fatal: x is a submodule"
    );
    assert_eq!(first_line_capped("a\u{1b}[31mb"), "a [31mb");
    assert_eq!(first_line_capped(""), "");
    let m = first_line_capped(&"é".repeat(150));
    assert!(m.len() <= CMD_MSG_MAX && m.ends_with("..."));
    let m = first_line_capped(&"a".repeat(500));
    assert_eq!(m.len(), CMD_MSG_MAX);
    assert!(m.ends_with("..."));
    let exact = "b".repeat(CMD_MSG_MAX);
    assert_eq!(first_line_capped(&exact), exact);
}
