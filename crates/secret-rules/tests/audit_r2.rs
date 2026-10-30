//! Regression tests for the second audit round: the leaks, evasions and DoS
//! shapes the closure check named, each written as the cited counterexample.
//!
//! `itemN_` and `hN_` tests follow the review's numbered findings;
//! `closureN_` tests follow the numbered "required closure work" list of the
//! later closure check, which regroups those findings.

mod support;

use secret_rules::{
    evaluate_request_with_connect, scrub_bytes, AuditReason, AuditRecordV1, ConnectAuthority,
    Decision, HeaderField, KeyPatterns, LabelRule, Location, RequestRefusal, RequestView, ScopeRef,
};
use serde_json::json;
use support::{bytes, key_bytes, load, string, MIN_KEY, MIN_KEY_HEX};

const PH: &str = "{{ck-secret:x}}";

fn rule(label: &str, location: Location) -> LabelRule {
    LabelRule {
        label: label.to_string(),
        host: "api.example.com".to_string(),
        port: 443,
        path_prefix: "/v1".to_string(),
        location,
        grant_id: "grant".to_string(),
    }
}

fn connect() -> ConnectAuthority {
    ConnectAuthority::parse("api.example.com:443").unwrap()
}

fn keys(label: &str, key: &[u8]) -> Vec<(String, Vec<u8>)> {
    vec![(label.to_string(), key.to_vec())]
}

/// Every spelling a `Debug` dump could use for the key: ASCII and the decimal
/// byte list libstd prints for byte slices.
fn debug_shows_key(rendered: &str, key: &[u8]) -> bool {
    let ascii = String::from_utf8_lossy(key);
    if !ascii.is_empty() && rendered.contains(ascii.as_ref()) {
        return true;
    }
    let decimal: Vec<String> = key.iter().map(|byte| byte.to_string()).collect();
    rendered.contains(&decimal.join(", "))
}

// --- item 8: no Debug or serialization path leaks key bytes -----------------

#[test]
fn item8_request_outcome_debug_does_not_dump_the_rewritten_body() {
    let body = format!("{{\"key\":\"{PH}\"}}");
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Json("/key".to_string()))],
        &keys("x", MIN_KEY),
    );
    assert_eq!(outcome.refusal, None);
    let body = outcome.rewritten_body.as_ref().expect("a rewritten body");
    assert!(
        body.windows(MIN_KEY.len()).any(|window| window == MIN_KEY),
        "the rewritten body carries the key, so Debug has something to leak"
    );
    let rendered = format!("{outcome:?}");
    assert!(
        !debug_shows_key(&rendered, MIN_KEY),
        "Debug printed the key: {rendered}"
    );
    assert!(rendered.contains("rewritten_body_len"), "{rendered}");
}

#[test]
fn item8_key_patterns_debug_does_not_dump_the_patterns() {
    let patterns = KeyPatterns::derive(MIN_KEY).unwrap();
    let rendered = format!("{patterns:?}");
    assert!(
        !debug_shows_key(&rendered, MIN_KEY),
        "KeyPatterns Debug printed key material: {rendered}"
    );
    assert!(rendered.contains("longest"), "{rendered}");
}

/// A record with `text` in every free-text field: label, request id, origin,
/// host, matched prefix, location kind and all three scope strings.
fn record_with_everywhere(text: &str) -> AuditRecordV1 {
    AuditRecordV1::new(1, text, Decision::Literal, AuditReason::Matched, text, text)
        .with_scope(Some(ScopeRef {
            owner: text.to_string(),
            kind: text.to_string(),
            reference: text.to_string(),
        }))
        .with_rule(text, 443, text, text)
}

/// A record with `text` in exactly one free-text field, named by `field`.
fn record_with_one(field: &str, text: &str) -> AuditRecordV1 {
    let clean = "clean";
    let scope = |owner: &str, kind: &str, reference: &str| ScopeRef {
        owner: owner.to_string(),
        kind: kind.to_string(),
        reference: reference.to_string(),
    };
    let pick = |name: &str| if name == field { text } else { clean };
    AuditRecordV1::new(
        1,
        pick("label"),
        Decision::Literal,
        AuditReason::Matched,
        pick("request_id"),
        pick("origin"),
    )
    .with_scope(Some(scope(pick("owner"), pick("kind"), pick("ref"))))
    .with_rule(pick("host"), 443, pick("prefix"), pick("location_kind"))
}

const RECORD_FIELDS: &[&str] = &[
    "label",
    "request_id",
    "origin",
    "owner",
    "kind",
    "ref",
    "host",
    "prefix",
    "location_kind",
];

#[test]
fn closure1_audit_debug_hides_the_key_in_every_field() {
    let text = String::from_utf8_lossy(MIN_KEY).into_owned();
    let record = record_with_everywhere(&text);
    for rendered in [
        format!("{record:?}"),
        format!("{:?}", record.scope),
        format!("{:?}", record.matched_prefix),
    ] {
        assert!(
            !debug_shows_key(&rendered, MIN_KEY),
            "Debug printed the key: {rendered}"
        );
    }
}

#[test]
fn closure1_header_edit_debug_hides_a_header_name_that_is_the_key() {
    let name = String::from_utf8_lossy(MIN_KEY).into_owned();
    let request = RequestView::new("/v1/x", vec![HeaderField::value(&name, PH.as_bytes())]);
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Header(name.clone()))],
        &keys("x", MIN_KEY),
    );
    assert_eq!(outcome.refusal, None);
    assert_eq!(outcome.header_edits.len(), 1, "the header was substituted");
    let rendered = format!("{outcome:?}");
    assert!(
        !debug_shows_key(&rendered, MIN_KEY),
        "Debug printed the key: {rendered}"
    );
}

/// `(key, field text)`: the field spells the key. Every spelling is written
/// out by hand.
const RECORD_SPELLINGS: &[(&[u8], &str)] = &[
    (b"0123456789abcdef", "0123456789abcdef"),
    // Base64, alone and at a shifted alignment inside a longer token.
    (b"0123456789abcdef", "MDEyMzQ1Njc4OWFiY2RlZg"),
    (b"0123456789abcdef", "xMDEyMzQ1Njc4OWFiY2RlZg"),
    // Hex, shifted by one character, and upper case inside other text.
    (b"abcdefghijklmnop", "g6162636465666768696a6b6c6d6e6f70"),
    (b"abcdefghijklmnop", "id-6162636465666768696A6B6C6D6E6F70"),
    // Percent and form (`+` is a space, `%2B` a plus).
    (
        b"0123456789abcdef",
        "%30%31%32%33%34%35%36%37%38%39%61%62%63%64%65%66",
    ),
    (b"my key/with+plus", "my+key%2Fwith%2Bplus"),
    // JSON escapes: `\uXXXX`, a control escape, and surrogate pairs.
    (
        b"0123456789abcdef",
        r"\u0030\u0031\u0032\u0033\u0034\u0035\u0036\u0037\u0038\u0039\u0061\u0062\u0063\u0064\u0065\u0066",
    ),
    (b"aaa\tbbbbccccdddd", r"aaa\tbbbbccccdddd"),
    (
        "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".as_bytes(),
        r"\ud83d\ude00\ud83d\ude00\ud83d\ude00\ud83d\ude00",
    ),
    // The punctuation-separated key the old keyless shape check let through.
    (b"ab-cd-ef-gh-ij-kl", "ab-cd-ef-gh-ij-kl"),
];

#[test]
fn closure1_the_keyed_writer_refuses_every_spelling_in_every_field() {
    let unrelated: &[u8] = b"zzzzzzzzzzzzzzzz";
    for (key, text) in RECORD_SPELLINGS {
        for field in RECORD_FIELDS {
            let record = record_with_one(field, text);
            // The spelled key is not the first one passed: every key is checked.
            assert_eq!(
                record.to_json_line_with_keys(&[unrelated, *key]),
                Err(secret_rules::AuditRefusal::KeyMaterialInRecord),
                "{field} = {text}"
            );
            assert_eq!(
                record.to_json_line_with_key(key),
                Err(secret_rules::AuditRefusal::KeyMaterialInRecord),
                "{field} = {text}"
            );
        }
    }
}

#[test]
fn closure1_the_keyed_writer_accepts_a_record_that_spells_no_key() {
    let keys: Vec<&[u8]> = RECORD_SPELLINGS.iter().map(|(key, _)| *key).collect();
    let record = record_with_one("origin", "task-1");
    let json = record
        .to_json_line_with_keys(&keys)
        .expect("a clean record is written");
    assert!(json.contains("\"origin\":\"task-1\""), "{json}");
}

// --- item 13: duplicate detection is total and bounded ----------------------

fn duplicate_case(body: &str) -> Option<RequestRefusal> {
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(body.as_bytes());
    evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Json("/key".to_string()))],
        &keys("x", MIN_KEY),
    )
    .refusal
}

#[test]
fn item13_leading_whitespace_duplicate_refuses() {
    let body = format!(" {{\"key\":\"{PH}\",\"key\":\"{PH}\"}}");
    assert_eq!(duplicate_case(&body), Some(RequestRefusal::DuplicateField));
}

#[test]
fn item13_whitespace_before_a_nested_value_still_finds_the_duplicate() {
    let body = format!("{{\"outer\": {{\"key\": \"{PH}\"}}, \"outer\": 1}}");
    assert_eq!(duplicate_case(&body), Some(RequestRefusal::DuplicateField));
}

#[test]
fn item13_escaped_member_names_compare_decoded() {
    // `ke\u0079` is `key` to any parser.
    let body = format!("{{\"key\":\"{PH}\",\"ke\\u0079\":1}}");
    assert_eq!(duplicate_case(&body), Some(RequestRefusal::DuplicateField));
}

#[test]
fn item13_deep_nesting_refuses_instead_of_overflowing() {
    let depth = 100_000;
    let mut body = String::new();
    body.push_str(&"[".repeat(depth));
    body.push_str(&"]".repeat(depth));
    assert_eq!(duplicate_case(&body), Some(RequestRefusal::BodyTooDeep));
}

#[test]
fn item13_a_wide_object_without_duplicates_is_accepted() {
    // Ten thousand distinct members and no duplicate. This checks only the
    // answer; the comparison count is bounded by the unit test
    // `a_wide_object_costs_n_log_n_name_comparisons` in `src/location/json.rs`.
    let members: Vec<String> = (0..10_000).map(|index| format!("\"k{index}\":1")).collect();
    let body = format!("{{{}}}", members.join(","));
    assert_eq!(duplicate_case(&body), None);
}

// --- item 14: base64 edges and mixed alphabets ------------------------------

fn b64(input: &[u8], url_safe: bool, pad: bool) -> String {
    let encoded = secret_rules::scrub::base64_encode(
        input,
        if url_safe {
            secret_rules::scrub::base64_url_safe()
        } else {
            secret_rules::scrub::base64_standard()
        },
        pad,
    );
    String::from_utf8(encoded).unwrap()
}

#[test]
fn item14_key_at_token_start_loses_no_edge_byte() {
    let mut token = MIN_KEY.to_vec();
    token.extend_from_slice(b"ZZ");
    let encoded = b64(&token, false, false);
    let output = scrub_bytes(MIN_KEY, "x", encoded.as_bytes()).unwrap();
    assert_eq!(output, PH.as_bytes(), "token was only partly replaced");
}

#[test]
fn item14_mixed_standard_and_urlsafe_alphabet_is_scrubbed() {
    // A token that decodes to the key but mixes `+`/`/` with `-`/`_`.
    let standard = b64(MIN_KEY, false, false);
    let mixed: String = standard
        .chars()
        .enumerate()
        .map(|(index, ch)| match (index % 2, ch) {
            (0, '+') => '-',
            (0, '/') => '_',
            _ => ch,
        })
        .collect();
    let url_safe = b64(MIN_KEY, true, false);
    for token in [mixed, url_safe, standard] {
        let output = scrub_bytes(MIN_KEY, "x", token.as_bytes()).unwrap();
        assert_eq!(output, PH.as_bytes(), "token {token} survived");
    }
}

#[test]
fn item14_long_base64_body_stays_linear() {
    // A one-megabyte base64 body with no key in it: the scrubber must finish
    // in linear time (the old per-offset decode took minutes). The bound is
    // loose on purpose: a linear scan of this body took 7 s in a debug build on
    // a loaded CI runner, and the quadratic one takes minutes.
    let body: String = (0..1_000_000)
        .map(|index| char::from(b'A' + (index % 26) as u8))
        .collect();
    let started = std::time::Instant::now();
    let output = scrub_bytes(MIN_KEY, "x", body.as_bytes()).unwrap();
    assert_eq!(output.len(), body.len());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "base64 body took {:?}",
        started.elapsed()
    );
}

// --- item 16: prefix boundaries, fragments, redirect validation -------------

#[test]
fn item16_dot_segment_or_doubled_prefixes_do_not_become_root() {
    for prefix in ["//", "/.", "/v1/..", "///"] {
        let rule = LabelRule {
            path_prefix: prefix.to_string(),
            ..rule("x", Location::Header("X-A".to_string()))
        };
        let request = RequestView::new("/admin", vec![HeaderField::value("X-A", PH.as_bytes())]);
        let outcome =
            evaluate_request_with_connect(&request, &connect(), &[rule], &keys("x", MIN_KEY));
        assert_eq!(
            outcome.refusal,
            Some(RequestRefusal::TargetMismatch(
                secret_rules::TargetRefusal::MalformedPath
            )),
            "prefix {prefix:?}"
        );
    }
}

#[test]
fn item16_a_fragment_in_the_target_refuses() {
    // `#` is not a path byte: a server routes /admin#/../v1/x to /admin while a
    // fragment-stripping client might see /v1/x.
    let request = RequestView::new(
        "/admin#/../v1/x",
        vec![HeaderField::value("X-A", PH.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Header("X-A".to_string()))],
        &keys("x", MIN_KEY),
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::FragmentInTarget));
}

#[test]
fn closure6_the_redirect_path_validates_rules_against_a_matching_hop() {
    use secret_rules::{evaluate_redirect, RedirectOutcome, RequestTarget, TargetRefusal};
    // The hop matches every rule's host, port and prefix, so each refusal
    // below can only come from rule validation.
    let hop = RequestTarget {
        host: "api.example.com".to_string(),
        port: 443,
        scheme: "https",
        target: "/v1/x",
    };
    let rules = vec![
        rule("x", Location::Query("k".to_string())),
        LabelRule {
            path_prefix: "//".to_string(),
            ..rule("x", Location::Query("k".to_string()))
        },
        rule("Bad Label", Location::Query("k".to_string())),
        rule("x", Location::Header("bad name".to_string())),
    ];
    let decisions = evaluate_redirect(&rules, &hop);
    let got: Vec<(RedirectOutcome, Option<RequestRefusal>)> = decisions
        .iter()
        .map(|decision| (decision.outcome, decision.refusal))
        .collect();
    assert_eq!(
        got,
        vec![
            (RedirectOutcome::Authorized, None),
            (
                RedirectOutcome::NotAuthorized,
                Some(RequestRefusal::TargetMismatch(TargetRefusal::MalformedPath))
            ),
            (
                RedirectOutcome::NotAuthorized,
                Some(RequestRefusal::InvalidLabel)
            ),
            (
                RedirectOutcome::NotAuthorized,
                Some(RequestRefusal::InvalidHeaderName)
            ),
        ]
    );
}

#[test]
fn closure6_a_fragment_anywhere_in_a_redirect_target_refuses() {
    use secret_rules::{evaluate_redirect, RedirectOutcome, RequestTarget, TargetRefusal};
    // The closure check's hop: a fragment after the query of an otherwise
    // matching target.
    for target in [
        "/v1/x?q=#fragment",
        "/v1/x#fragment",
        "https://api.example.com/v1/x?q=#f",
    ] {
        assert_eq!(
            secret_rules::normalize_request_target(target),
            Err(TargetRefusal::FragmentInTarget),
            "{target}"
        );
        let hop = RequestTarget {
            host: "api.example.com".to_string(),
            port: 443,
            scheme: "https",
            target,
        };
        let decisions = evaluate_redirect(&[rule("x", Location::Query("q".to_string()))], &hop);
        assert_eq!(decisions.len(), 1, "{target}");
        assert_eq!(
            decisions[0].outcome,
            RedirectOutcome::NotAuthorized,
            "{target}"
        );
        assert_eq!(
            decisions[0].refusal,
            Some(RequestRefusal::TargetMismatch(
                TargetRefusal::FragmentInTarget
            )),
            "{target}"
        );
    }
    // The same hop without the fragment is authorized, so the refusal above
    // is the fragment's.
    let hop = RequestTarget {
        host: "api.example.com".to_string(),
        port: 443,
        scheme: "https",
        target: "/v1/x?q=",
    };
    let decisions = evaluate_redirect(&[rule("x", Location::Query("q".to_string()))], &hop);
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].outcome, RedirectOutcome::Authorized);
}

#[test]
fn closure6_a_query_fragment_refuses_in_the_request_evaluator() {
    let request = RequestView::new(
        "/v1/x?k=#frag",
        vec![HeaderField::value("X-A", PH.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Header("X-A".to_string()))],
        &keys("x", MIN_KEY),
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::FragmentInTarget));
    assert!(outcome.header_edits.is_empty());
}

#[test]
fn closure6_the_standalone_prefix_helpers_never_widen_to_the_root() {
    use secret_rules::path_covers;
    use secret_rules::redirect::path_is_covered;
    for prefix in ["//", "///", "/v1/", "/v1/..", "/.", "", "v1"] {
        assert!(
            !path_is_covered("/admin", prefix),
            "path_is_covered {prefix:?}"
        );
        assert!(!path_covers("/admin", prefix), "path_covers {prefix:?}");
    }
    // The valid spellings still work, the root included.
    assert!(path_is_covered("/admin", "/"));
    assert!(path_covers("/admin", "/"));
    assert!(path_is_covered("/v1/x", "/v1"));
    assert!(!path_is_covered("/v10", "/v1"));
}

// --- item 6: duplicate authority fields and a bound destination -------------

#[test]
fn item6_duplicate_host_or_authority_refuses() {
    let bearer = format!("Bearer {PH}");
    for headers in [
        vec![
            HeaderField::value("Host", b"api.example.com"),
            HeaderField::value("Host", b"evil.example"),
            HeaderField::value("Authorization", bearer.as_bytes()),
        ],
        vec![
            HeaderField::value(":authority", b"api.example.com"),
            HeaderField::value(":authority", b"evil.example"),
            HeaderField::value("Authorization", bearer.as_bytes()),
        ],
    ] {
        let request = RequestView::new("/v1/x", headers);
        let outcome = evaluate_request_with_connect(
            &request,
            &connect(),
            &[rule("x", Location::Bearer)],
            &keys("x", MIN_KEY),
        );
        assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
        assert!(!outcome.substituted());
    }
}

#[test]
fn item6_an_approved_target_for_an_evil_connect_is_not_authorized() {
    // The caller cannot hand `evaluate_request_with_connect` a target of its
    // own: the destination comes from the CONNECT authority. A rule for
    // evil.example therefore matches nothing on an api.example.com tunnel.
    let bearer = format!("Bearer {PH}");
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("Authorization", bearer.as_bytes())],
    );
    let evil_rule = LabelRule {
        host: "evil.example".to_string(),
        ..rule("x", Location::Bearer)
    };
    let outcome =
        evaluate_request_with_connect(&request, &connect(), &[evil_rule], &keys("x", MIN_KEY));
    assert_eq!(
        outcome.refusal, None,
        "the rule is valid, it simply does not match"
    );
    assert!(!outcome.substituted());
}

// --- item 15: known-answer tests for the independent detectors --------------

/// One detector case: a name, a key and an output to check against it.
fn case<'a>(name: &'a str, key: &'a [u8], output: &'a [u8]) -> (&'a str, &'a [u8], &'a [u8]) {
    (name, key, output)
}

#[test]
fn item15_each_decoder_has_known_answers() {
    // Every expected encoding below is written out by hand, never produced by
    // the crate's own encoders, so a shared encoder bug cannot hide here.
    let all_ff = [0xffu8; 16];
    // 0xfb 0xef 0xbe encodes to `++++` (standard) and `----` (URL-safe).
    let mut all_plus = [0xfb, 0xef, 0xbe].repeat(5);
    all_plus.push(0xfb);
    let positives = vec![
        case("base64 standard /", &all_ff, b"/////////////////////w"),
        case("base64 url-safe _", &all_ff, b"_____________________w"),
        case("base64 standard +", &all_plus, b"+++++++++++++++++++++w"),
        case("base64 url-safe -", &all_plus, b"---------------------w"),
        case(
            "json \\uXXXX",
            b"0123456789abcdef",
            br"\u0030\u0031\u0032\u0033\u0034\u0035\u0036\u0037\u0038\u0039\u0061\u0062\u0063\u0064\u0065\u0066",
        ),
        case("json \\t", b"aaa\tbbbbccccdddd", br"aaa\tbbbbccccdddd"),
        case(
            "json control escapes",
            b"ab\ncd\ref\x08gh\x0cij\tkl",
            br"ab\ncd\ref\bgh\fij\tkl",
        ),
        case(
            "json surrogate pairs",
            "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".as_bytes(),
            br"\ud83d\ude00\ud83d\ude00\ud83d\ude00\ud83d\ude00",
        ),
        case(
            "json surrogate pairs, upper case",
            "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".as_bytes(),
            br"\uD83D\uDE00\uD83D\uDE00\uD83D\uDE00\uD83D\uDE00",
        ),
        // Form: `+` is a space and `%2B` a plus, decoded in that order.
        case("form plus and percent", b"my key/with+plus", b"my+key%2Fwith%2Bplus"),
    ];
    for (name, key, output) in positives {
        assert!(support::discloses(key, output), "detector missed {name}");
    }
    let negatives = vec![
        case("other base64 key", &all_plus, b"/////////////////////w"),
        case(
            "escaped backslash, not a tab",
            b"aaa\tbbbbccccdddd",
            br"aaa\\tbbbbccccdddd",
        ),
        case(
            "lone high surrogates",
            "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".as_bytes(),
            br"\ud83d\ud83d\ud83d\ud83d",
        ),
        case(
            "surrogates in the wrong order",
            "\u{1F600}\u{1F600}\u{1F600}\u{1F600}".as_bytes(),
            br"\ude00\ud83d\ude00\ud83d\ude00\ud83d\ude00\ud83d",
        ),
        // A literal `+` in the key is `%2B` in a form; a bare `+` is a space.
        case(
            "form plus is a space",
            b"my key/with+plus",
            b"my+key/with+plus",
        ),
        case(
            "form with a wrong byte",
            b"my key/with+plus",
            b"my+key%2Fwith%2Bpluz",
        ),
    ];
    for (name, key, output) in negatives {
        assert!(
            !support::discloses(key, output),
            "detector reported {name} as a leak"
        );
    }
}

// --- closure 3: JSON surrogate pairs in the production scrubber ---------------

/// Four U+1F600 characters: sixteen UTF-8 bytes, an admissible key.
const EMOJI_KEY: &str = "\u{1F600}\u{1F600}\u{1F600}\u{1F600}";

/// Scrub `input` in the given chunks with one `x` matcher for `key`.
fn scrub_chunks(key: &[u8], chunks: &[&[u8]]) -> Vec<u8> {
    let mut scrubber =
        secret_rules::Scrubber::new(vec![secret_rules::MatcherSet::new("x", key).unwrap()])
            .unwrap();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend_from_slice(&scrubber.push(chunk));
    }
    out.extend_from_slice(&scrubber.finish());
    out
}

#[test]
fn closure3_surrogate_pair_escaped_key_is_scrubbed_whole_split_and_byte_at_a_time() {
    let key = EMOJI_KEY.as_bytes();
    for spelled in [
        r"\ud83d\ude00\ud83d\ude00\ud83d\ude00\ud83d\ude00",
        r"\uD83D\uDE00\uD83D\uDE00\uD83D\uDE00\uD83D\uDE00",
    ] {
        let body = format!("{{\"k\":\"{spelled}\"}}");
        let expected = "{\"k\":\"{{ck-secret:x}}\"}";
        let whole = scrub_bytes(key, "x", body.as_bytes()).unwrap();
        assert_eq!(String::from_utf8_lossy(&whole), expected, "whole buffer");
        let bytes = body.as_bytes();
        for split in 0..=bytes.len() {
            let out = scrub_chunks(key, &[&bytes[..split], &bytes[split..]]);
            assert_eq!(String::from_utf8_lossy(&out), expected, "split at {split}");
        }
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        let out = scrub_chunks(key, &singles);
        assert_eq!(String::from_utf8_lossy(&out), expected, "byte at a time");
    }
}

#[test]
fn closure3_lone_surrogates_are_not_the_key() {
    // Not valid JSON for the key, so the text passes through unchanged.
    let body = r"\ud83d\ud83d\ud83d\ud83d\ude00";
    let out = scrub_bytes(EMOJI_KEY.as_bytes(), "x", body.as_bytes()).unwrap();
    assert_eq!(out, body.as_bytes());
}

#[test]
fn closure3_json_control_escapes_in_a_key_are_scrubbed() {
    let key: &[u8] = b"ab\ncd\ref\x08gh\x0cij\tkl";
    let out = scrub_bytes(key, "x", br#"["ab\ncd\ref\bgh\fij\tkl"]"#).unwrap();
    assert_eq!(String::from_utf8_lossy(&out), r#"["{{ck-secret:x}}"]"#);
}

// --- H2: a replacement may not disclose an installed key -------------------

#[test]
fn h2_a_label_that_spells_its_key_refuses() {
    // The audit's pair: key `abcdefghijklmnop`, label its hex.
    let key: &[u8] = b"abcdefghijklmnop";
    let hex_label = "6162636465666768696a6b6c6d6e6f70";
    let request = RequestView::new("/v1/x", Vec::new());
    let mut hex_rule = rule("placeholder", Location::Header("X-A".to_string()));
    hex_rule.label = hex_label.to_string();
    let outcome =
        evaluate_request_with_connect(&request, &connect(), &[hex_rule], &keys(hex_label, key));
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::ReplacementDisclosesKey)
    );
    let rendered = format!("{outcome:?}");
    assert!(!debug_shows_key(&rendered, key), "{rendered}");
}

#[test]
fn h2_a_key_that_contains_its_label_is_still_substituted() {
    // The reverse of the pair above, and not a refusal case: the key
    // `a-very-long-key-value` contains the label `x`, but the placeholder
    // `{{ck-secret:x}}` does not spell the key, so the rule is substituted as
    // usual. This guards against the collision check refusing too much.
    let key: &[u8] = b"a-very-long-key-value";
    let mut containing = rule("x", Location::Header("X-A".to_string()));
    containing.label = "x".to_string();
    let request = RequestView::new("/v1/x", vec![HeaderField::value("X-A", PH.as_bytes())]);
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[containing],
        &[("x".to_string(), key.to_vec())],
    );
    assert_eq!(outcome.refusal, None);
    let edit = outcome.header_edits.first().expect("a header edit");
    assert_eq!(edit.replacement.as_slice(), key);
}

#[test]
fn h2_a_key_that_is_its_labels_hex_refuses() {
    // The audit's exact pair: the label is the key's hex, so the placeholder
    // written for it would spell the whole key.
    let key: &[u8] = b"abcdefghijklmnop";
    let hex_label = "6162636465666768696a6b6c6d6e6f70";
    let mut hex_rule = rule("placeholder", Location::Header("X-A".to_string()));
    hex_rule.label = hex_label.to_string();
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value(
            "X-A",
            Box::leak(
                format!("{{{{ck-secret:{hex_label}}}}}")
                    .into_bytes()
                    .into_boxed_slice(),
            ),
        )],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[hex_rule],
        &[(hex_label.to_string(), key.to_vec())],
    );
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::ReplacementDisclosesKey)
    );
}

// --- closure 4: no installation entry point accepts a disclosing replacement ---

/// The closure check's pairs: `(label, key)` where the label's placeholder
/// spells the key. Each spelling is written out by hand.
const DISCLOSING_PAIRS: &[(&str, &[u8])] = &[
    // The key's lowercase hex, aligned.
    ("6162636465666768696a6b6c6d6e6f70", b"abcdefghijklmnop"),
    // The same hex shifted by one character inside the label.
    ("g6162636465666768696a6b6c6d6e6f70", b"abcdefghijklmnop"),
    // The key's unpadded base64: `jzz` is `anp6`, the final `j` is `ag`.
    ("anp6anp6anp6anp6anp6ag", b"jzzjzzjzzjzzjzzj"),
];

/// A key none of the labels above spells.
const OTHER_KEY: &[u8] = b"zzzzzzzzzzzzzzzz";

#[test]
fn closure4_the_request_evaluator_refuses_every_disclosing_label() {
    for (label, key) in DISCLOSING_PAIRS {
        let placeholder = format!("{{{{ck-secret:{label}}}}}");
        let request = RequestView::new(
            "/v1/x",
            vec![HeaderField::value("X-A", placeholder.as_bytes())],
        );
        let header_rule = rule(label, Location::Header("X-A".to_string()));
        // Against the rule's own key, and against another label's key.
        for keys in [
            vec![(label.to_string(), key.to_vec())],
            vec![
                (label.to_string(), OTHER_KEY.to_vec()),
                ("plain".to_string(), key.to_vec()),
            ],
        ] {
            let outcome = evaluate_request_with_connect(
                &request,
                &connect(),
                std::slice::from_ref(&header_rule),
                &keys,
            );
            assert_eq!(
                outcome.refusal,
                Some(RequestRefusal::ReplacementDisclosesKey),
                "label {label}"
            );
            assert!(outcome.header_edits.is_empty(), "label {label}");
        }
    }
}

#[test]
fn closure4_every_scrubber_entry_point_refuses_a_disclosing_replacement() {
    use secret_rules::{MatcherSet, ScrubError, Scrubber};
    let refused = Some(ScrubError::ReplacementDisclosesKey);
    for (label, key) in DISCLOSING_PAIRS {
        // A matcher whose own placeholder spells its own key.
        assert_eq!(MatcherSet::new(label, key).err(), refused, "label {label}");
        assert_eq!(
            scrub_bytes(key, label, b"x").err(),
            refused,
            "label {label}"
        );
        // The label belongs to another key; the spelled key is installed
        // under a plain label, before or after it.
        let spelling = || MatcherSet::new(label, OTHER_KEY).unwrap();
        let spelled = || MatcherSet::new("plain", key).unwrap();
        assert_eq!(
            Scrubber::new(vec![spelled(), spelling()]).err(),
            refused,
            "label {label}"
        );
        assert_eq!(
            Scrubber::new(vec![spelling(), spelled()]).err(),
            refused,
            "label {label}"
        );
        let mut rotated = Scrubber::new(vec![spelled()]).unwrap();
        assert_eq!(rotated.install(spelling()).err(), refused, "label {label}");
        assert_eq!(
            rotated.matcher_count(),
            1,
            "a refused install changed state"
        );
        let mut rotated = Scrubber::new(vec![spelling()]).unwrap();
        assert_eq!(rotated.install(spelled()).err(), refused, "label {label}");
        assert_eq!(
            rotated.matcher_count(),
            1,
            "a refused install changed state"
        );
    }
}

#[test]
fn closure4_generated_replacements_disclose_no_installed_key() {
    // Labels shaped like encodings, but of unrelated bytes: the hex of
    // sixteen `z`, and the base64 of `kzzkzzkzzkzzkzzk`.
    let installed: &[(&str, &[u8])] = &[
        ("7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a", b"abcdefghijklmnop"),
        ("a3p6a3p6a3p6a3p6a3p6aw", b"jzzjzzjzzjzzjzzj"),
    ];
    let mut scrubber = secret_rules::Scrubber::new(
        installed
            .iter()
            .map(|(label, key)| secret_rules::MatcherSet::new(label, key).unwrap())
            .collect(),
    )
    .unwrap();
    let mut out = scrubber.push(b"a=abcdefghijklmnop b=jzzjzzjzzjzzjzzj");
    out.extend_from_slice(&scrubber.finish());
    assert_eq!(
        String::from_utf8_lossy(&out),
        "a={{ck-secret:7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a}} b={{ck-secret:a3p6a3p6a3p6a3p6a3p6aw}}"
    );
    for (_, key) in installed {
        assert!(
            !support::discloses(key, &out),
            "output discloses {}",
            String::from_utf8_lossy(key)
        );
    }
    // The independent detector does see the disclosing labels, so the check
    // above is not vacuous.
    for (label, key) in DISCLOSING_PAIRS {
        assert!(support::discloses(key, label.as_bytes()), "label {label}");
    }
}

// --- H1: a fully escaped representation streams intact ----------------------

#[test]
fn h1_unicode_escaped_key_survives_every_split() {
    let escaped: String = MIN_KEY
        .iter()
        .map(|byte| format!("\\u{:04x}", byte))
        .collect();
    let body = format!("before {escaped} after");
    let whole = scrub_bytes(MIN_KEY, "x", body.as_bytes()).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&whole),
        "before {{ck-secret:x}} after"
    );
    for split in 0..=body.len() {
        let mut scrubber =
            secret_rules::Scrubber::new(vec![secret_rules::MatcherSet::new("x", MIN_KEY).unwrap()])
                .unwrap();
        let mut out = scrubber.push(body.as_bytes().get(..split).unwrap_or_default());
        out.extend_from_slice(&scrubber.push(body.as_bytes().get(split..).unwrap_or_default()));
        out.extend_from_slice(&scrubber.finish());
        assert_eq!(
            String::from_utf8_lossy(&out),
            "before {{ck-secret:x}} after",
            "split at {split} lost the match"
        );
    }
}

// --- closure 5: header reads come from header bytes; explicit bearer forms -----

/// A Bearer-rule request whose Host is the range `[6, 21)` of `header_bytes`.
fn host_case(header_bytes: &'static [u8], body: &'static [u8]) -> secret_rules::RequestOutcome {
    let bearer: &'static str = Box::leak(format!("Bearer {PH}").into_boxed_str());
    let request = RequestView::from_header_block(
        "/v1/x",
        vec![
            HeaderField::ranged("Host", 6, 21),
            HeaderField::value("Authorization", bearer.as_bytes()),
        ],
        header_bytes,
    )
    .with_body(body);
    evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Bearer)],
        &keys("x", MIN_KEY),
    )
}

#[test]
fn closure5_the_host_is_read_from_the_header_block_not_the_body() {
    // Both buffers hold a host at `[6, 21)`; only the header block's counts.
    // `evl.example.com` and `api.example.com` are both fifteen bytes.
    let evil_header = host_case(b"Host: evl.example.com\r\n", br#"{"a":"api.example.com"}"#);
    assert_eq!(
        evil_header.refusal,
        Some(RequestRefusal::TargetMismatch(
            secret_rules::TargetRefusal::AuthorityMismatch
        ))
    );
    assert!(evil_header.header_edits.is_empty());
    let good_header = host_case(b"Host: api.example.com\r\n", br#"{"a":"evl.example.com"}"#);
    assert_eq!(good_header.refusal, None);
    assert!(good_header.substituted());
}

#[test]
fn closure5_a_host_range_outside_the_header_block_refuses() {
    let body = b"api.example.com";
    let request = RequestView::from_header_block(
        "/v1/x",
        vec![HeaderField::ranged("Host", 0, body.len())],
        b"",
    )
    .with_body(body);
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Bearer)],
        &keys("x", MIN_KEY),
    );
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::TargetMismatch(
            secret_rules::TargetRefusal::AuthorityMismatch
        ))
    );
}

/// A JSON-rule request whose Content-Type is the range `[14, 30)` of
/// `header_bytes`.
fn content_type_case(
    header_bytes: &'static [u8],
    body: &'static [u8],
) -> secret_rules::RequestOutcome {
    let request = RequestView::from_header_block(
        "/v1/x",
        vec![HeaderField::ranged("Content-Type", 14, 30)],
        header_bytes,
    )
    .with_body(body);
    evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Json("/key".to_string()))],
        &keys("x", MIN_KEY),
    )
}

#[test]
fn closure5_a_body_cannot_forge_its_own_json_content_type() {
    // The closure check's input: the header says CBOR; the body's bytes at the
    // same offsets say `application/json`.
    let outcome = content_type_case(
        b"Content-Type: application/cbor\r\n",
        br#"{"paddingxx":"application/json","key":"{{ck-secret:x}}"}"#,
    );
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::JsonContentTypeMissing)
    );
    assert_eq!(outcome.rewritten_body, None);
}

#[test]
fn closure5_a_ranged_json_content_type_is_honoured_whatever_the_body_holds() {
    // The header says JSON; the body's bytes at `[14, 30)` are not a media type.
    let outcome = content_type_case(
        b"Content-Type: application/json\r\n",
        br#"{"paddingxx":"text/plain-xxxxxx","key":"{{ck-secret:x}}"}"#,
    );
    assert_eq!(outcome.refusal, None);
    assert_eq!(
        outcome.rewritten_body.as_ref().map(|body| body.as_slice()),
        Some(br#"{"paddingxx":"text/plain-xxxxxx","key":"0123456789abcdef"}"#.as_slice())
    );
}

fn bearer_outcome(field: HeaderField<'_>) -> secret_rules::RequestOutcome {
    let request = RequestView::new("/v1/x", vec![field]);
    evaluate_request_with_connect(
        &request,
        &connect(),
        &[rule("x", Location::Bearer)],
        &keys("x", MIN_KEY),
    )
}

#[test]
fn closure5_a_borrowed_whole_bearer_value_is_rewritten_with_its_scheme() {
    for scheme in ["Bearer ", "bearer "] {
        let value = format!("{scheme}{PH}");
        let outcome = bearer_outcome(HeaderField::value("Authorization", value.as_bytes()));
        assert_eq!(outcome.refusal, None);
        let edit = outcome.header_edits.first().expect("a header edit");
        assert_eq!(edit.kind, secret_rules::HeaderEditKind::WholeValue);
        assert_eq!(
            (edit.start, edit.end),
            (0, 0),
            "a borrowed value has no offsets"
        );
        let expected = format!("{scheme}0123456789abcdef");
        assert_eq!(edit.replacement.as_slice(), expected.as_bytes());
    }
}

#[test]
fn closure5_a_parsed_bearer_credential_is_replaced_by_the_key_alone() {
    let outcome = bearer_outcome(HeaderField::bearer_credential(PH.as_bytes()));
    assert_eq!(outcome.refusal, None);
    let edit = outcome.header_edits.first().expect("a header edit");
    assert_eq!(edit.kind, secret_rules::HeaderEditKind::BearerCredential);
    assert_eq!((edit.start, edit.end), (0, 0));
    assert_eq!(edit.replacement.as_slice(), MIN_KEY);
}

#[test]
fn closure5_a_scheme_less_wire_authorization_refuses() {
    // `Authorization: {{ck-secret:x}}` with no scheme, ranged and borrowed.
    let ranged = RequestView::from_header_block(
        "/v1/x",
        vec![HeaderField::ranged("Authorization", 0, PH.len())],
        PH.as_bytes(),
    );
    let ranged = evaluate_request_with_connect(
        &ranged,
        &connect(),
        &[rule("x", Location::Bearer)],
        &keys("x", MIN_KEY),
    );
    let borrowed = bearer_outcome(HeaderField::value("Authorization", PH.as_bytes()));
    for outcome in [ranged, borrowed] {
        assert_eq!(outcome.refusal, Some(RequestRefusal::BearerSchemeMissing));
        assert!(outcome.header_edits.is_empty());
        assert!(!outcome.substituted());
    }
}

// --- the first-round items that must stay closed ---------------------------

#[test]
fn audit_suite_constants_are_the_audit_keys() {
    assert_eq!(MIN_KEY.len(), secret_rules::MIN_KEY_LEN);
    assert_eq!(MIN_KEY_HEX.len(), MIN_KEY.len() * 2);
    let _ = json!({});
    let _ = load("secret_rules_v1.json");
    let _ = (bytes, key_bytes, string);
}
