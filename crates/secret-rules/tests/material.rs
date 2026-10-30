//! P3: key admissibility and per-location encoding.

use secret_rules::{check_admissible, encode_for_location, Location, MaterialRefusal, MIN_KEY_LEN};

const KEY: &[u8] = b"sk-live-0123456789abcdef";

#[test]
fn empty_short_and_placeholder_keys_refuse() {
    assert_eq!(
        check_admissible(b"", "repo_key", &Location::Bearer),
        Err(MaterialRefusal::Empty)
    );
    assert_eq!(
        check_admissible(&[b'x'; MIN_KEY_LEN - 1], "repo_key", &Location::Bearer),
        Err(MaterialRefusal::TooShort)
    );
    assert_eq!(
        check_admissible(&[b'x'; MIN_KEY_LEN], "repo_key", &Location::Bearer),
        Ok(())
    );
    // The label's placeholder text contains the label itself, so a key that
    // reproduces it is refused: it would appear in every substituted request.
    assert_eq!(
        check_admissible(
            b"ck-secret:repo_key",
            "repo_key",
            &Location::Query("k".to_string())
        ),
        Err(MaterialRefusal::SubstringOfPlaceholder)
    );
}

#[test]
fn header_values_refuse_control_and_non_visible_bytes() {
    for byte in [0x00u8, 0x09, 0x0a, 0x0d, 0x1f, 0x7f, 0x80, 0xff] {
        let mut key = KEY.to_vec();
        key[4] = byte;
        assert_eq!(
            check_admissible(&key, "repo_key", &Location::Header("X-Api-Key".to_string())),
            Err(MaterialRefusal::NonVisibleHeaderByte),
            "byte {byte:#04x} must refuse a header value"
        );
    }
    // Space and tilde are visible and legal in a field value.
    let mut spaced = KEY.to_vec();
    spaced[4] = b' ';
    assert!(check_admissible(&spaced, "repo_key", &Location::Bearer).is_ok());
}

#[test]
fn query_encoding_matches_rfc3986_with_uppercase_hex() {
    let key = b"a b~c%-0123456789";
    assert_eq!(
        encode_for_location(key, "repo_key", &Location::Query("k".to_string())).unwrap(),
        b"a%20b~c%25-0123456789"
    );
    let encoded = encode_for_location(KEY, "repo_key", &Location::Query("k".to_string())).unwrap();
    assert_eq!(encoded, KEY, "unreserved bytes stay literal");
    let binary = encode_for_location(
        b"\x00\x1f\x7f\xff-a0123456789",
        "repo_key",
        &Location::Query("k".into()),
    )
    .unwrap();
    assert_eq!(binary, b"%00%1F%7F%FF-a0123456789");
}

#[test]
fn json_encoding_escapes_rfc8259() {
    let key = b"a\"b\\c\n\td-01234567";
    let encoded = encode_for_location(key, "repo_key", &Location::Json("/k".to_string())).unwrap();
    assert_eq!(encoded, b"a\\\"b\\\\c\\n\\td-01234567");
}

#[test]
fn bearer_and_header_encoding_carry_the_key_verbatim() {
    assert_eq!(
        encode_for_location(KEY, "repo_key", &Location::Bearer).unwrap(),
        b"Bearer sk-live-0123456789abcdef"
    );
    assert_eq!(
        encode_for_location(KEY, "repo_key", &Location::Header("X".to_string())).unwrap(),
        KEY
    );
}

#[test]
fn a_header_location_refuses_a_key_with_a_newline() {
    assert_eq!(
        encode_for_location(
            b"sk-live-0123456789abc\ndef",
            "repo_key",
            &Location::Header("X-Api-Key".to_string())
        ),
        Err(MaterialRefusal::NonVisibleHeaderByte)
    );
}
