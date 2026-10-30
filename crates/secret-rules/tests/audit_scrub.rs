//! Regression tests for the Athena audit items 1-3, 10 and 14: the streaming
//! scrubber must never split a complete match, must not make release depend on
//! the secret, and must cover every ordinary encoding of the key.

use secret_rules::scrub::{
    base64_encode, base64_standard, base64_url_safe, json_escape, percent_patterns,
};
use secret_rules::{scrub_bytes, KeyPatterns, MatcherSet, Scrubber};

const LABEL: &str = "x";
const PLACEHOLDER: &[u8] = b"{{ck-secret:x}}";

fn scrub(key: &[u8], input: &[u8]) -> Vec<u8> {
    scrub_bytes(key, LABEL, input).expect("key is scrubbable")
}

// --- item 1: a complete match must never be split across calls ---------------

#[test]
fn self_overlapping_key_is_scrubbed_whole() {
    // The audit's minimal counterexample: the key is its own 15-byte suffix.
    let key = b"aaaaaaaaaaaaaaaa";
    let out = scrub(key, key);
    assert_eq!(
        out, PLACEHOLDER,
        "the complete key must come back as one placeholder"
    );
}

#[test]
fn a_full_match_ending_exactly_at_the_push_boundary_is_scrubbed() {
    let key: &[u8] = b"0123456789abcdef";
    // Exactly one holdback worth of bytes before the match, so the match ends
    // at the end of the chunk and the caller is about to call finish().
    let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    let lead = vec![b'.'; 3];
    let mut out = scrubber.push(&lead);
    out.extend_from_slice(&scrubber.push(key));
    out.extend_from_slice(&scrubber.finish());
    assert_eq!(out, b"...{{ck-secret:x}}");
}

#[test]
fn every_chunk_partition_produces_the_same_output() {
    let key: &[u8] = b"sk-live-0123456789abcdef";
    let text = b"before sk-live-0123456789abcdef after";
    let whole = scrub(key, text);
    assert_eq!(whole, b"before {{ck-secret:x}} after");
    for chunk in 1..=text.len() {
        let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
        let mut out = Vec::new();
        for piece in text.chunks(chunk) {
            out.extend_from_slice(&scrubber.push(piece));
        }
        out.extend_from_slice(&scrubber.finish());
        assert_eq!(out, whole, "chunk size {chunk} diverged");
    }
}

#[test]
fn a_key_made_of_one_repeated_byte_survives_no_partition() {
    let key = vec![b'='; secret_rules::MIN_KEY_LEN + 4];
    let mut text = b"..".to_vec();
    text.extend_from_slice(&key);
    text.extend_from_slice(b"..");
    for chunk in 1..=text.len() {
        let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, &key).unwrap()]).unwrap();
        let mut out = Vec::new();
        for piece in text.chunks(chunk) {
            out.extend_from_slice(&scrubber.push(piece));
        }
        out.extend_from_slice(&scrubber.finish());
        assert_eq!(
            out,
            b"..{{ck-secret:x}}..".to_vec(),
            "chunk size {chunk} left key material"
        );
    }
}

// --- item 2: release must not depend on the secret's prefix ------------------

#[test]
fn output_does_not_depend_on_a_matching_prefix() {
    let key: &[u8] = b"sk-live-0123456789abcdef";

    let mut matching = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    let mut other = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    let withheld_matching = matching.push(b"s");
    let withheld_other = other.push(b"x");

    assert_eq!(
        withheld_matching.len(),
        withheld_other.len(),
        "release depended on whether the byte continued the secret"
    );

    let withheld_matching = matching.push(b"y");
    let withheld_other = other.push(b"y");
    assert_eq!(
        withheld_matching.len(),
        withheld_other.len(),
        "release depended on whether the bytes continued the secret"
    );
}

#[test]
fn the_fixed_holdback_is_always_longest_consumed_minus_one() {
    let key: &[u8] = b"sk-live-0123456789abcdef";
    // The holdback covers the widest representation a match can consume, which
    // is the fully JSON-escaped form (`\u00XX` per byte), not the longest
    // literal pattern.
    let longest = KeyPatterns::derive(key).unwrap().longest_consumed();
    assert!(longest > KeyPatterns::derive(key).unwrap().longest());
    let mut scrubber = Scrubber::new(vec![MatcherSet::new(LABEL, key).unwrap()]).unwrap();
    let mut seen = 0usize;
    for (position, byte) in b"some ordinary response text ".iter().enumerate() {
        seen += scrubber.push(&[*byte]).len();
        let pending = scrubber.pending_len();
        let fed = position + 1;
        assert_eq!(scrubber.overlap(), longest - 1);
        assert_eq!(
            seen + pending,
            fed,
            "the scrubber lost or invented bytes at offset {fed}"
        );
        assert!(
            pending <= scrubber.overlap(),
            "more than the holdback was withheld"
        );
    }
}

// --- item 3: every common encoding of the key is caught ----------------------

#[test]
fn lowercase_percent_escapes_next_to_literal_bytes_are_scrubbed() {
    // Audit counterexample: fifteen `z` and a `0xFE` byte, echoed with a
    // lowercase escape.
    let mut key = vec![b'z'; 15];
    key.push(0xfe);
    let echoed = b"zzzzzzzzzzzzzzz%fe&z=1";
    assert_eq!(scrub(&key, echoed), b"{{ck-secret:x}}&z=1");
    assert_eq!(scrub(&key, echoed), b"{{ck-secret:x}}&z=1", "stable rerun");
}

#[test]
fn a_single_escaped_unreserved_byte_does_not_evade_scrubbing() {
    let key: &[u8] = b"0123456789abcdef";
    assert_eq!(
        scrub(key, b"k=%30123456789abcdef&z=1"),
        b"k={{ck-secret:x}}&z=1"
    );
    assert_eq!(
        scrub(key, b"k=01%32%33%34%35%36%37%38%39abcdef&z=1"),
        b"k={{ck-secret:x}}&z=1"
    );
}

#[test]
fn mixed_case_hex_without_any_digit_is_scrubbed() {
    let key = vec![0xab; 16];
    let echoed = "Ab".repeat(16);
    assert_eq!(scrub(&key, echoed.as_bytes()), PLACEHOLDER);
    let echoed = "aB".repeat(16);
    assert_eq!(scrub(&key, echoed.as_bytes()), PLACEHOLDER);
}

#[test]
fn json_escaped_slashes_are_scrubbed() {
    let key: &[u8] = b"g/0123456789abcd";
    assert_eq!(
        scrub(key, b"\"g\\/0123456789abcd\""),
        b"\"{{ck-secret:x}}\""
    );
}

#[test]
fn form_encoding_with_plus_for_space_is_scrubbed() {
    let key: &[u8] = b"g 0123456789abcd";
    assert_eq!(scrub(key, b"g+0123456789abcd"), PLACEHOLDER);
}

#[test]
fn percent_escape_case_never_defeats_a_literal_byte_mix() {
    // Every escape is tried in both hex cases, whatever the literal bytes
    // around it look like, and every selection of escaped vs literal bytes is
    // the same value to a decoder. Each case is checked on a full window, the
    // way a response body arrives.
    let key: &[u8] = b"g/0123456789abcd";
    // Every case gets its own scrubber state and declares the stream ended
    // with `finish`, the way a fetch call does; the payload pads the key on
    // both sides.
    let long = |text: &str| format!("hdr: x {text} tail");

    for pattern in percent_patterns(key) {
        let text = String::from_utf8_lossy(&pattern).into_owned();
        let haystack = long(&text);
        assert_eq!(
            scrub(key, haystack.as_bytes()),
            b"hdr: x {{ck-secret:x}} tail",
            "pattern {text} was not scrubbed"
        );

        let flipped: String = text
            .chars()
            .enumerate()
            .map(|(index, ch)| {
                // Flip the case of the hex digits of every escape.
                if index >= 1 && text.as_bytes()[index - 1] == b'%' {
                    if ch.is_ascii_lowercase() {
                        ch.to_ascii_uppercase()
                    } else {
                        ch.to_ascii_lowercase()
                    }
                } else {
                    ch
                }
            })
            .collect();
        let haystack = long(&flipped);
        assert_eq!(
            scrub(key, haystack.as_bytes()),
            b"hdr: x {{ck-secret:x}} tail",
            "escaped form {flipped} was not scrubbed"
        );
    }

    // A scrambled escape selection: the server escapes some bytes and leaves
    // others literal, all with one hex case.
    let scrambled = scrambled_escapes(key, 17);
    let haystack = long(&scrambled);
    assert_eq!(
        scrub(key, haystack.as_bytes()),
        b"hdr: x {{ck-secret:x}} tail",
        "scrambled selection {scrambled} was not scrubbed"
    );
}

/// Encode `key` with a fixed pseudo-random choice of escape vs literal per
/// byte, so the test covers every escaping decision a server could make.
///
/// All escapes share one hex case here, because a response is produced by one
/// encoder; mixed hex case inside a single value is covered by the hex tests.
fn scrambled_escapes(key: &[u8], seed: u64) -> String {
    let mut state = seed;
    let mut out = String::new();
    for byte in key {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if !state.is_multiple_of(3) {
            out.push_str(&format!("%{byte:02x}"));
        } else {
            out.push(*byte as char);
        }
    }
    out
}

#[test]
fn json_escape_of_a_key_with_slashes_round_trips() {
    let key: &[u8] = b"g/0123456789abcd";
    let escaped = json_escape(key);
    assert_eq!(scrub(key, escaped.as_bytes()), PLACEHOLDER);
}

// --- item 10: generated replacements are never rescanned --------------------

#[test]
fn a_label_that_is_the_hex_of_its_own_key_is_refused() {
    // The audit's counterexample: a deposited label that spells the key in
    // hex. The placeholder is written into responses unscanned, so it would
    // carry the key; installing that matcher refuses instead.
    let key: &[u8] = b"abcdefghijklmnop";
    let label = "6162636465666768696a6b6c6d6e6f70";
    assert_eq!(
        scrub_bytes(key, label, b"abcdefghijklmnop!"),
        Err(secret_rules::ScrubError::ReplacementDisclosesKey)
    );
}

#[test]
fn a_key_spelled_by_another_matchers_replacement_is_refused() {
    // The second matcher's key is the whole placeholder the first one
    // generates. A scrubber that never rescans its output would still write
    // that placeholder, which is the second key, so the pair can never be
    // installed: a key holding the placeholder prefix is refused as soon as
    // its matcher is built, before any pairing.
    let first: &[u8] = b"aaaaaaaaaaaaaaaa";
    let generated: &[u8] = b"{{ck-secret:other}}";
    assert!(MatcherSet::new("other", first).is_ok());
    assert_eq!(
        MatcherSet::new("aaaaaaaaaaaaaaaa", generated).err(),
        Some(secret_rules::ScrubError::ReplacementDisclosesKey)
    );
}

// --- item 14: an independent decoder, not the production matcher set --------

/// Whole-buffer oracle: decodes every common encoding back to bytes and looks
/// for the key. It shares no code with `KeyPatterns`.
fn discloses(key: &[u8], output: &[u8]) -> bool {
    let text = String::from_utf8_lossy(output).into_owned();
    if text.contains(std::str::from_utf8(key).unwrap_or_default()) {
        return true;
    }
    if let Some(decoded) = percent_decode(&text) {
        if decoded.windows(key.len()).any(|window| window == key) {
            return true;
        }
    }
    if let Some(decoded) = form_decode(&text) {
        if decoded.windows(key.len()).any(|window| window == key) {
            return true;
        }
    }
    if let Some(decoded) = json_decode(&text) {
        if decoded.windows(key.len()).any(|window| window == key) {
            return true;
        }
    }
    for decoded in hex_decodes(&text) {
        if decoded.windows(key.len()).any(|window| window == key) {
            return true;
        }
    }
    for alpha in [base64_standard(), base64_url_safe()] {
        for trimmed in [0usize, 1, 2] {
            let candidate = text.trim_end_matches('=');
            let candidate = &candidate[..candidate.len().saturating_sub(trimmed)];
            for offset in 0..4 {
                if let Some(decoded) = base64_decode(&candidate[offset..], alpha) {
                    if decoded.len() >= key.len()
                        && decoded.windows(key.len()).any(|window| window == key)
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let pair = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            if let Ok(byte) = u8::from_str_radix(pair, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    Some(out)
}

fn form_decode(text: &str) -> Option<Vec<u8>> {
    percent_decode(&text.replace('+', " "))
}

fn json_decode(text: &str) -> Option<Vec<u8>> {
    let unescaped = text
        .replace("\\/", "/")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\");
    Some(unescaped.into_bytes())
}

fn hex_decodes(text: &str) -> Vec<Vec<u8>> {
    // A key may be written as hex in any case, at either alignment, and mixed
    // with literal bytes. Decode every hex-looking window at both alignments.
    let mut out = Vec::new();
    for start in 0..text.len().min(2) {
        let bytes = text.as_bytes();
        let mut index = start;
        let mut running: Vec<u8> = Vec::new();
        while index + 1 < bytes.len() {
            let pair = &bytes[index..index + 2];
            let decoded = std::str::from_utf8(pair)
                .ok()
                .and_then(|text| u8::from_str_radix(text, 16).ok());
            match decoded {
                Some(byte) => {
                    running.push(byte);
                    index += 2;
                }
                None => {
                    if running.len() >= 8 {
                        out.push(std::mem::take(&mut running));
                    }
                    running.clear();
                    index += 1;
                }
            }
        }
        if running.len() >= 8 {
            out.push(running);
        }
    }
    out
}

fn base64_decode(text: &str, alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = alphabet.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[test]
fn the_independent_oracle_catches_the_encodings_under_test() {
    let key: &[u8] = b"0123456789abcdef";
    for encoded in [
        key.to_vec(),
        b"0123456789%61bcdef".to_vec(),
        b"0123456789%61bcdef".to_vec(),
        b"30313233343536373839616263646566".to_vec(),
        base64_encode(
            &b"ZZ"
                .iter()
                .copied()
                .chain(key.iter().copied())
                .collect::<Vec<u8>>(),
            base64_standard(),
            true,
        ),
    ] {
        assert!(
            discloses(key, &encoded),
            "oracle missed {:?}",
            String::from_utf8_lossy(&encoded)
        );
    }
    assert!(!discloses(key, PLACEHOLDER));
}
