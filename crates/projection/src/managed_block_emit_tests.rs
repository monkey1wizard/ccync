use super::managed_block::apply_block;

const BEGIN: &[u8] = b"<!-- ccync:managed:begin -->";
const END: &[u8] = b"<!-- ccync:managed:end -->";

fn block(begin_line: &[u8], payload: &[u8], end_line: &[u8]) -> Vec<u8> {
    [BEGIN, begin_line, payload, end_line, END].concat()
}

#[test]
fn missing_emitter_seam_is_exercised_first() {
    let result = apply_block(b"user instructions\n", Some("managed instructions"));

    assert_eq!(
        result,
        Ok(b"user instructions\n<!-- ccync:managed:begin -->\nmanaged instructions\n<!-- ccync:managed:end -->\n".to_vec())
    );
}

#[test]
fn appending_to_a_file_without_a_trailing_newline_keeps_existing_bytes() {
    let input = b"before\nlast line";

    let output = apply_block(input, Some("managed")).unwrap();

    assert!(output.starts_with(b"before\nlast line\n"));
    assert!(output.ends_with(b"managed\n<!-- ccync:managed:end -->"));
}

#[test]
fn rewrites_an_existing_block_in_place_and_preserves_surrounding_bytes() {
    let input = [
        b"before\n".as_slice(),
        &block(b"\n", b"old", b"\n"),
        b"\nafter".as_slice(),
    ]
    .concat();

    let output = apply_block(&input, Some("new")).unwrap();

    assert_eq!(
        output,
        [
            b"before\n".as_slice(),
            &block(b"\n", b"new", b"\n"),
            b"\nafter".as_slice(),
        ]
        .concat()
    );
}

#[test]
fn dominant_crlf_is_used_for_the_emitted_block() {
    let input = b"before\r\nline\r\nlast\r\n";

    let output = apply_block(input, Some("managed")).unwrap();

    assert_eq!(
        output,
        b"before\r\nline\r\nlast\r\n<!-- ccync:managed:begin -->\r\nmanaged\r\n<!-- ccync:managed:end -->\r\n"
    );
}

#[test]
fn line_ending_ties_and_new_files_use_lf() {
    let tied = b"lf\ncrlf\r\n";
    let new_file = apply_block(b"", Some("managed")).unwrap();
    let tied_file = apply_block(tied, Some("managed")).unwrap();

    assert!(new_file.windows(2).all(|pair| pair != b"\r\n"));
    assert!(tied_file.starts_with(tied));
    assert!(tied_file[tied.len()..].windows(2).all(|pair| pair != b"\r\n"));
}

#[test]
fn rewriting_preserves_mixed_external_line_endings() {
    let prefix = b"first\nsecond\r\n";
    let suffix = b"\r\nthird\nfourth\r\n";
    let input = [prefix.as_slice(), &block(b"\n", b"old", b"\n"), suffix].concat();

    let output = apply_block(&input, Some("new")).unwrap();

    assert_eq!(
        output,
        [prefix.as_slice(), &block(b"\n", b"new", b"\n"), suffix].concat()
    );
}

#[test]
fn appending_preserves_mixed_external_line_endings() {
    let input = b"first\nsecond\r\nthird\n";

    let output = apply_block(input, Some("managed")).unwrap();

    assert!(output.starts_with(input));
    assert_eq!(
        &output[input.len()..],
        b"<!-- ccync:managed:begin -->\nmanaged\n<!-- ccync:managed:end -->\n"
    );
}

#[test]
fn bom_is_preserved_byte_for_byte() {
    let input = b"\xef\xbb\xbfuser content\n";

    let output = apply_block(input, Some("managed")).unwrap();

    assert_eq!(&output[..3], b"\xef\xbb\xbf");
    assert!(output[3..].starts_with(b"user content\n"));
}

#[test]
fn none_removes_only_the_managed_block() {
    let input = [
        b"before\r\n".as_slice(),
        &block(b"\r\n", b"managed", b"\r\n"),
        b"\r\nafter\r\n".as_slice(),
    ]
    .concat();

    assert_eq!(
        apply_block(&input, None),
        Ok(b"before\r\n\r\nafter\r\n".to_vec())
    );
}

#[test]
fn pruning_preserves_mixed_external_line_endings() {
    let prefix = b"first\nsecond\r\n";
    let suffix = b"\r\nthird\nfourth\r\n";
    let input = [prefix.as_slice(), &block(b"\n", b"managed", b"\n"), suffix].concat();

    assert_eq!(
        apply_block(&input, None),
        Ok([prefix.as_slice(), suffix].concat())
    );
}

#[test]
fn none_is_a_noop_when_no_managed_block_exists() {
    let input = b"user content\r\n";

    assert_eq!(apply_block(input, None), Ok(input.to_vec()));
}

#[test]
fn damaged_marker_pairs_fail_closed() {
    let unpaired = [BEGIN, b"\nmanaged"].concat();
    let duplicate = [BEGIN, b"\n", BEGIN, b"\nmanaged\n", END].concat();

    assert!(apply_block(&unpaired, Some("replacement")).is_err());
    assert!(apply_block(&duplicate, None).is_err());
}
