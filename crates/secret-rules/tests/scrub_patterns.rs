//! S1 and S2: every encoding is scrubbed, including matches split across
//! reads.

use secret_rules::scrub::{
    base64_encode, base64_standard, json_escape, percent_patterns, MatcherSet, Scrubber,
};
use secret_rules::{scrub_bytes, KeyPatterns};

const LABEL: &str = "repo_key";

/// Lowercase hex of `bytes`.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn keys() -> [&'static [u8]; 3] {
    [
        b"0123456789abcdef",
        b"0123456789abcdefg",
        b"0123456789abcdefgh",
    ]
}

fn assert_scrubbed(key: &[u8], haystack: &str, context: &str) {
    let out = scrub_bytes(key, LABEL, haystack.as_bytes()).expect("key is scrubbable");
    let text = String::from_utf8_lossy(&out);
    assert!(
        !text.contains(std::str::from_utf8(key).unwrap()),
        "{context}: raw key survived in {text:?}"
    );
    assert!(
        text.contains("{{ck-secret:repo_key}}"),
        "{context}: substitution missing in {text:?}"
    );
}

#[test]
fn exact_key_bytes_are_scrubbed_in_headers_bodies_and_trailers() {
    for key in keys() {
        assert_scrubbed(
            key,
            &format!("x-key: {}", std::str::from_utf8(key).unwrap()),
            "header",
        );
        assert_scrubbed(
            key,
            &format!(
                "body before {} body after",
                std::str::from_utf8(key).unwrap()
            ),
            "body",
        );
    }
}

#[test]
fn base64_standard_and_url_safe_padded_and_unpadded_are_scrubbed() {
    let key: &[u8] = b"0123456789abcdef";
    let standard = base64_encode(key, base64_standard(), false);
    let url_safe = base64_encode(
        key,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
        false,
    );
    // 16 bytes: standard unpadded 22 chars, padded 24 chars.
    assert_eq!(standard.len(), 22);
    assert_scrubbed(
        key,
        &String::from_utf8_lossy(&standard),
        "standard unpadded",
    );
    assert_scrubbed(
        key,
        &String::from_utf8_lossy(&url_safe),
        "url-safe unpadded",
    );
    let padding =
        String::from_utf8_lossy(&base64_encode(key, base64_standard(), true)).into_owned();
    assert_eq!(padding.len(), 24);
    assert_scrubbed(key, &padding, "standard padded");
}

#[test]
fn base64_inside_a_larger_payload_is_scrubbed_at_offsets_zero_one_and_two() {
    // Each key is embedded in a JSON-ish blob at every base64 byte alignment.
    for key in keys() {
        for offset in 0..3 {
            let prefix_len = 5 + offset;
            let mut payload = vec![b'Z'; prefix_len];
            payload.extend_from_slice(key);
            payload.extend_from_slice(b"trailing-data-here");
            let encoded = base64_encode(&payload, base64_standard(), false);
            let encoded_text = String::from_utf8_lossy(&encoded).into_owned();
            let out = scrub_bytes(key, LABEL, encoded_text.as_bytes()).unwrap();
            let out_text = String::from_utf8_lossy(&out);
            assert!(
                out_text.contains("{{ck-secret:repo_key}}"),
                "offset {offset}: base64 payload at offset was not scrubbed: {out_text}"
            );
            assert!(
                !out_text.contains(&encoded_text),
                "offset {offset}: base64 payload survived"
            );
        }
    }
}

#[test]
fn percent_forms_are_scrubbed_in_both_spellings_and_both_hex_cases() {
    let key: &[u8] = b"\x00\x01abc\xfe\xff0123456789";
    let patterns = percent_patterns(key);
    for pattern in &patterns {
        let haystack = String::from_utf8_lossy(pattern).into_owned();
        let out = scrub_bytes(key, LABEL, haystack.as_bytes()).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("{{ck-secret:repo_key}}"),
            "percent form {:?} survived",
            haystack
        );
        let lower = haystack.to_ascii_lowercase();
        let out = scrub_bytes(key, LABEL, lower.as_bytes()).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("{{ck-secret:repo_key}}"),
            "lowercase percent form {lower} survived"
        );
    }
}

#[test]
fn hex_is_scrubbed_in_any_case() {
    let key: &[u8] = b"\xde\xad\xbe\xef0123456789abcd";
    // The hex form of the key, built from the key so the two spellings cannot
    // drift apart.
    let lower = hex_lower(key);
    let upper = lower.to_uppercase();
    let mixed: String = key
        .iter()
        .enumerate()
        .flat_map(|(position, byte)| {
            let pair = format!("{byte:02x}");
            if position % 2 == 0 {
                pair.to_uppercase().chars().collect::<Vec<_>>()
            } else {
                pair.chars().collect::<Vec<_>>()
            }
        })
        .collect();
    for form in [lower.as_str(), upper.as_str(), mixed.as_str()] {
        let out = scrub_bytes(key, LABEL, form.as_bytes()).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("{{ck-secret:repo_key}}"),
            "hex form {form} survived"
        );
    }
}

#[test]
fn json_escaped_key_is_scrubbed() {
    let key: &[u8] = b"a\"b\\c\td-0123456789";
    let json = json_escape(key);
    let out = scrub_bytes(key, LABEL, json.as_bytes()).unwrap();
    assert!(String::from_utf8_lossy(&out).contains("{{ck-secret:repo_key}}"));
}

#[test]
fn a_match_split_across_reads_is_caught() {
    let key: &[u8] = b"0123456789abcdef";
    let payload = b"prefix sk-live 0123456789abcdef suffix".to_vec();
    let text = String::from_utf8(payload).unwrap();
    for split in 1..text.len() {
        let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
        let mut out = scrubber.push(&text.as_bytes()[..split]);
        out.extend_from_slice(&scrubber.push(&text.as_bytes()[split..]));
        out.extend_from_slice(&scrubber.finish());
        let out_text = String::from_utf8_lossy(&out);
        assert!(
            out_text.contains("{{ck-secret:repo_key}}"),
            "split at {split} lost the match: {out_text}"
        );
        assert!(!out_text.contains("0123456789abcdef"));
    }
}

#[test]
fn a_match_split_across_three_reads_is_caught() {
    let key: &[u8] = b"0123456789abcdef";
    let text = "aaaa 0123456789abcdef bbbb";
    let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    let mut out = Vec::new();
    for chunk in text.as_bytes().chunks(6) {
        out.extend_from_slice(&scrubber.push(chunk));
    }
    out.extend_from_slice(&scrubber.finish());
    let out_text = String::from_utf8_lossy(&out);
    assert!(out_text.contains("{{ck-secret:repo_key}}"), "{out_text}");
}

#[test]
fn the_overlap_window_is_one_less_than_the_longest_consumed_form() {
    let key: &[u8] = b"0123456789abcdef";
    let scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    // The window covers the widest representation a match consumes, which is
    // the fully JSON-escaped form (six bytes per key byte), not the longest
    // literal pattern.
    let consumed = KeyPatterns::derive(key).unwrap().longest_consumed();
    assert!(consumed > KeyPatterns::derive(key).unwrap().longest());
    assert_eq!(scrubber.overlap(), consumed - 1);
    assert!(scrubber.overlap() >= 2, "a 16-byte key keeps a real window");
}

#[test]
fn a_partial_prefix_at_the_end_of_a_stream_that_never_completes_is_emitted_by_finish() {
    let key: &[u8] = b"0123456789abcdef";
    let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    let mut out = scrubber.push(b"body ends with 01234");
    out.extend_from_slice(&scrubber.finish());
    assert_eq!(out, b"body ends with 01234");
}

#[test]
fn two_keys_in_one_stream_are_both_scrubbed() {
    let first: &[u8] = b"first-key-0123456789";
    let second: &[u8] = b"second-key-9876543210";
    let text = b"a first-key-0123456789 b second-key-9876543210 c";
    let mut scrubber = Scrubber::new(vec![
        MatcherSet::new("one", first).unwrap(),
        MatcherSet::new("two", second).unwrap(),
    ])
    .unwrap();
    let mut out = scrubber.push(text);
    out.extend_from_slice(&scrubber.finish());
    let out_text = String::from_utf8_lossy(&out);
    assert!(out_text.contains("{{ck-secret:one}}"), "{out_text}");
    assert!(out_text.contains("{{ck-secret:two}}"), "{out_text}");
    assert!(!out_text.contains("first-key") && !out_text.contains("second-key"));
}

#[test]
fn a_key_shorter_than_the_minimum_is_not_scrubbable() {
    assert!(KeyPatterns::derive(b"").is_err());
    assert!(KeyPatterns::derive(b"0123456789abcde").is_err());
}
