use roadviewer_hars::media::{NalHasher, annex_b_signature};
#[test]
fn nal_signature_is_independent_of_chunk_and_start_code_boundaries() {
    // Synthetic SPS/PPS + picture; scanner tests do not claim decoder validity.
    let data = [
        0, 0, 0, 1, 0x67, 5, 6, 0, 0, 1, 0x68, 8, 9, 0, 0, 0, 1, 0x65, 10, 11, 0, 0,
    ];
    let expected = annex_b_signature(&data, false).unwrap();
    for chunk in 1..=data.len() {
        let mut h = NalHasher::new(false);
        for bytes in data.chunks(chunk) {
            h.feed(bytes).unwrap();
        }
        assert_eq!(h.finish().unwrap(), expected);
    }
    let mut changed = data;
    changed[18] = 55;
    assert_ne!(annex_b_signature(&changed, false).unwrap(), expected);
}
#[test]
fn missing_headers_or_truncated_nal_do_not_validate() {
    assert!(annex_b_signature(&[0, 0, 1, 0x65, 1], false).is_err());
    assert!(annex_b_signature(&[0, 0, 1, 0x40], true).is_err());
}
