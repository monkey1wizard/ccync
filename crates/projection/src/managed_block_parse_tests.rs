use super::managed_block::locate_block;

const BEGIN: &[u8] = b"<!-- ccync:managed:begin -->";
const END: &[u8] = b"<!-- ccync:managed:end -->";

#[test]
fn no_markers_returns_no_block() {
    assert_eq!(locate_block(b"user instructions\n"), Ok(None));
}

#[test]
fn complete_lf_block_is_located() {
    let mut input = Vec::from(BEGIN);
    input.extend_from_slice(b"\nmanaged text\n");
    input.extend_from_slice(END);

    assert!(locate_block(&input).unwrap().is_some());
}

#[test]
fn complete_crlf_block_is_located() {
    let mut input = Vec::from(BEGIN);
    input.extend_from_slice(b"\r\nmanaged text\r\n");
    input.extend_from_slice(END);

    assert!(locate_block(&input).unwrap().is_some());
}

#[test]
fn marker_inside_fenced_code_still_counts() {
    let input = b"```\n<!-- ccync:managed:begin -->\nexample\n<!-- ccync:managed:end -->\n```";

    assert!(locate_block(input).unwrap().is_some());
}

#[test]
fn foreign_markers_are_content() {
    let input = b"<!-- other:begin -->\ntext\n<!-- other:end -->";

    assert_eq!(locate_block(input), Ok(None));
}

#[test]
fn unpaired_begin_is_damage() {
    let input = [BEGIN, b"\nmanaged text"].concat();

    assert!(locate_block(&input).is_err());
}

#[test]
fn unpaired_end_is_damage() {
    let input = [b"user text\n", END].concat();

    assert!(locate_block(&input).is_err());
}

#[test]
fn duplicate_begin_is_damage() {
    let input = [BEGIN, b"\n", BEGIN, b"\ntext\n", END].concat();

    assert!(locate_block(&input).is_err());
}

#[test]
fn duplicate_end_is_damage() {
    let input = [BEGIN, b"\ntext\n", END, b"\n", END].concat();

    assert!(locate_block(&input).is_err());
}
