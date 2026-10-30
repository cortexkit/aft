//! Regression tests for the Athena audit items 4-9, 11-13: per-label keys,
//! authority composition, atomic refusal, strict URL parsing, double-encoded
//! traversal, duplicate detection and the audit boundary.

use secret_rules::{
    authority_matches_connect, evaluate_request_with_connect, location_kind_token,
    normalize_request_target, split_absolute, AuditRecordV1, ConnectAuthority, Decision,
    HeaderField, LabelOutcome, LabelRule, Location, RequestOutcome, RequestRefusal, RequestTarget,
    RequestView, ScopeRef, TargetRefusal,
};

const PLACEHOLDER: &str = "{{ck-secret:repo_key}}";
const OTHER_PLACEHOLDER: &str = "{{ck-secret:other_key}}";
const KEY: &[u8] = b"sk-live-0123456789abcdef";
const OTHER_KEY: &[u8] = b"other-live-0123456789abc";

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

// --- item 4 (per-label keys) and item 5 (per-label redirect verdicts) -------

#[test]
fn every_label_receives_its_own_key() {
    let rules = [
        rule("repo_key", Location::Header("X-Repo".to_string())),
        rule("other_key", Location::Header("X-Other".to_string())),
    ];
    let request = RequestView::new(
        "/v1/things",
        vec![
            HeaderField::value("X-Repo", PLACEHOLDER.as_bytes()),
            HeaderField::value("X-Other", OTHER_PLACEHOLDER.as_bytes()),
        ],
    );
    let keys = [
        ("repo_key".to_string(), KEY.to_vec()),
        ("other_key".to_string(), OTHER_KEY.to_vec()),
    ];
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys);
    assert_eq!(outcome.refusal, None);
    match (&outcome.labels[0], &outcome.labels[1]) {
        (
            secret_rules::LabelOutcome::Substituted {
                replacement: first, ..
            },
            secret_rules::LabelOutcome::Substituted {
                replacement: second,
                ..
            },
        ) => {
            assert_eq!(first.as_slice(), KEY);
            assert_eq!(second.as_slice(), OTHER_KEY);
        }
        other => panic!("expected two substitutions, got {other:?}"),
    }
}

#[test]
fn a_key_for_another_label_is_never_substituted() {
    // Only the key for label `other_key` is available; label `repo_key` is in
    // the request but has no key of its own, so the request refuses instead of
    // borrowing the other key.
    let rules = [rule("repo_key", Location::Header("X-Repo".to_string()))];
    let request = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("X-Repo", PLACEHOLDER.as_bytes())],
    );
    let keys = [("other_key".to_string(), OTHER_KEY.to_vec())];
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys);
    assert_eq!(outcome.refusal, Some(RequestRefusal::MissingMaterial));
    assert!(!outcome.substituted());
}

#[test]
fn redirect_authority_is_decided_per_label() {
    use secret_rules::{evaluate_redirect, RedirectOutcome};
    let rules = [
        rule("repo_key", Location::Query("key".to_string())),
        LabelRule {
            label: "other_key".to_string(),
            host: "raw.example.com".to_string(),
            port: 443,
            path_prefix: "/v1".to_string(),
            location: Location::Query("key".to_string()),
            grant_id: "grant".to_string(),
        },
    ];
    let hop = RequestTarget {
        host: "raw.example.com".to_string(),
        port: 443,
        scheme: "https",
        target: "/v1/next",
    };
    let decisions = evaluate_redirect(&rules, &hop);
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[0].outcome, RedirectOutcome::NotAuthorized);
    assert_eq!(decisions[1].outcome, RedirectOutcome::Authorized);
    assert_eq!(decisions[1].label, "other_key");
}

// --- item 6: the evaluator composes the CONNECT authority check -------------

#[test]
fn an_absolute_form_target_to_another_host_is_refused() {
    let rules = [rule("repo_key", Location::Bearer)];
    let bearer = format!("Bearer {PLACEHOLDER}");
    let request = RequestView::new(
        "https://evil.example/v1/x",
        vec![HeaderField::value("Authorization", bearer.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::TargetMismatch(
            TargetRefusal::AuthorityMismatch
        ))
    );
    assert!(!outcome.substituted());
}

#[test]
fn a_host_header_for_another_host_is_refused() {
    let rules = [rule("repo_key", Location::Bearer)];
    let bearer = format!("Bearer {PLACEHOLDER}");
    let request = RequestView::new(
        "/v1/x",
        vec![
            HeaderField::value("Host", b"evil.example"),
            HeaderField::value("Authorization", bearer.as_bytes()),
        ],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::TargetMismatch(
            TargetRefusal::AuthorityMismatch
        ))
    );
}

#[test]
fn the_connect_check_and_the_evaluator_agree() {
    assert_eq!(
        authority_matches_connect(&connect(), Some("evil.example"), None, "/v1"),
        Err(TargetRefusal::AuthorityMismatch)
    );
}

// --- item 7: refusal is atomic, and the predicate counts header edits -------

#[test]
fn a_refusal_clears_every_key_bearing_outcome() {
    let rules = [
        rule("repo_key", Location::Header("X-A".to_string())),
        rule("repo_key", Location::Header("X-B".to_string())),
    ];
    let request = RequestView::new(
        "/v1/things",
        vec![
            HeaderField::value("X-A", PLACEHOLDER.as_bytes()),
            HeaderField::value("X-B", PLACEHOLDER.as_bytes()),
            HeaderField::value("X-B", PLACEHOLDER.as_bytes()),
        ],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
    for label in &outcome.labels {
        assert!(
            !matches!(label, secret_rules::LabelOutcome::Substituted { .. }),
            "a refused request kept a key-bearing outcome: {label:?}"
        );
    }
    assert!(outcome.header_edits.is_empty());
}

#[test]
fn substituted_counts_a_bearer_or_header_edit() {
    let rules = [rule("repo_key", Location::Bearer)];
    // A proxy that parsed the `Bearer` scheme itself passes the credential's
    // range only, and says so.
    let buffer = format!("Authorization: Bearer {PLACEHOLDER}");
    let start = "Authorization: Bearer ".len();
    let header_block = Box::leak(buffer.clone().into_bytes().into_boxed_slice());
    let request = RequestView::from_header_block(
        "/v1/things",
        vec![HeaderField::ranged_bearer_credential(start, buffer.len())],
        header_block,
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, None);
    assert!(
        outcome.substituted(),
        "a successful bearer substitution reported no substitution"
    );
    let header_edit = outcome
        .header_edits
        .first()
        .expect("the header edit is reported");
    assert_eq!(header_edit.name, "Authorization");
    assert_eq!(
        (header_edit.start, header_edit.end),
        (start, buffer.len()),
        "the edit covers exactly the credential"
    );
    assert_eq!(header_edit.replacement.as_slice(), KEY);
    assert_eq!(
        header_edit.kind,
        secret_rules::HeaderEditKind::BearerCredential
    );
}

// --- item 8: authority parsing rejects userinfo and stray suffixes ----------

#[test]
fn an_ipv6_authority_with_userinfo_is_rejected() {
    assert_eq!(
        split_absolute("https://[::1]@evil.example/v1?k=x"),
        Err(TargetRefusal::MalformedAuthority)
    );
    assert_eq!(
        split_absolute("https://user@api.example.com/v1"),
        Err(TargetRefusal::MalformedAuthority)
    );
    assert_eq!(
        ConnectAuthority::parse("[::1]@evil.example"),
        Err(TargetRefusal::MalformedAuthority)
    );
}

#[test]
fn an_unconsumed_authority_suffix_is_rejected() {
    for url in [
        "https://api.example.com:443x/v1",
        "https://api.example.com:4x43/v1",
        "https://api.example.com:443extra/v1",
        "https://[::1]junk/v1",
        "https://api.example.com./v1",
    ] {
        assert!(
            split_absolute(url).is_err(),
            "{url} should not parse as an authority"
        );
    }
}

// --- item 9: recursively encoded dot segments never widen the prefix -------

#[test]
fn a_double_encoded_dot_segment_is_refused() {
    let rules = [rule("repo_key", Location::Header("X-A".to_string()))];
    let request = RequestView::new(
        "/v1/%252e%252e/echo",
        vec![HeaderField::value("X-A", PLACEHOLDER.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    // The rule cannot match a path it refuses to normalize, so the label is out
    // of scope and nothing is substituted; the path itself is refused outright.
    assert_eq!(outcome.refusal, None);
    assert_eq!(outcome.labels, vec![LabelOutcome::OutOfScope]);
    assert!(!outcome.substituted());
    assert_eq!(
        normalize_request_target("/v1/%252e%252e/echo"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
}

#[test]
fn single_encoded_dot_segments_still_normalize() {
    let rules = [rule("repo_key", Location::Header("X-A".to_string()))];
    let request = RequestView::new(
        "/v1/a/%2E%2E/b",
        vec![HeaderField::value("X-A", PLACEHOLDER.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, None);
}

// --- item 11: duplicate names are compared decoded --------------------------

#[test]
fn a_percent_encoded_query_alias_is_a_duplicate() {
    let rules = [rule("repo_key", Location::Query("key".to_string()))];
    let request_target = format!("/v1/x?key={PLACEHOLDER}&k%65y=other");
    let request = RequestView::new(&request_target, Vec::new());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
}

#[test]
fn duplicate_json_object_names_refuse() {
    let rules = [rule("repo_key", Location::Json("/key".to_string()))];
    let body = format!("{{\"key\":\"{PLACEHOLDER}\",\"key\":\"{PLACEHOLDER}\"}}");
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
}

// --- item 12: fragments, prefixes, redaction and label grammar --------------

#[test]
fn a_fragment_is_not_a_query_string() {
    let rules = [rule("repo_key", Location::Query("k".to_string()))];
    let request = RequestView::new("/v1/x#?k={{ck-secret:repo_key}}", Vec::new());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert!(
        outcome.refusal.is_some() || outcome.labels[0] == secret_rules::LabelOutcome::OutOfScope,
        "a fragment was read as a query: {:?}",
        outcome.labels
    );
}

#[test]
fn a_rule_prefix_that_is_not_absolute_is_refused() {
    let rules = [LabelRule {
        path_prefix: "v1".to_string(),
        ..rule("repo_key", Location::Header("X-A".to_string()))
    }];
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("X-A", PLACEHOLDER.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert!(
        outcome.refusal.is_some(),
        "a relative rule prefix must not broaden the rule"
    );
}

#[test]
fn header_rules_validate_the_label_grammar() {
    let rules = [rule("Repo Key", Location::Header("X-A".to_string()))];
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value(
            "X-A",
            "{{ck-secret:Repo Key}}".as_bytes(),
        )],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("Repo Key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::InvalidLabel));
}

#[test]
fn debug_output_never_prints_key_bytes() {
    let rules = [rule("repo_key", Location::Header("X-A".to_string()))];
    let buffer = format!("X-A: {PLACEHOLDER}");
    let start = "X-A: ".len();
    let request = {
        let mut request = RequestView::new(
            "/v1/x",
            vec![HeaderField::ranged("X-A", start, buffer.len())],
        );
        request.body = Box::leak(buffer.clone().into_bytes().into_boxed_slice());
        request
    };
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    let rendered = format!("{outcome:?}");
    assert!(
        !rendered.contains("sk-live-0123456789abcdef"),
        "Debug printed the key: {rendered}"
    );
}

#[test]
fn location_kind_tokens_stay_stable() {
    assert_eq!(location_kind_token(&Location::Bearer), "bearer");
    assert_eq!(
        location_kind_token(&Location::Header("X-A".to_string())),
        "header:x-a"
    );
}

// --- item 13: the audit record cannot carry key bytes -----------------------

#[test]
fn an_audit_reason_is_a_closed_vocabulary() {
    assert!(secret_rules::AuditReason::parse("matched").is_some());
    assert!(secret_rules::AuditReason::parse("sk-live-0123456789abcdef").is_none());
}

#[test]
fn an_audit_record_refuses_a_metadata_value_that_holds_the_key() {
    let record = AuditRecordV1::new(
        1,
        "repo_key",
        Decision::Literal,
        secret_rules::AuditReason::Matched,
        "req-1",
        "sk-live-0123456789abcdef",
    );
    let result = record.to_json_line_with_key(KEY);
    assert_eq!(
        result,
        Err(secret_rules::AuditRefusal::KeyMaterialInRecord),
        "a record whose origin held the key was serialized"
    );
}

#[test]
fn an_audit_record_refuses_then_scrubs_a_field_holding_the_key() {
    // A field that holds the key refuses the keyed writer.
    let record = AuditRecordV1::new(
        1,
        "repo_key",
        Decision::Literal,
        secret_rules::AuditReason::Matched,
        "req-sk-live-0123456789abcdef",
        "task-1",
    );
    assert_eq!(
        record.to_json_line_with_key(KEY),
        Err(secret_rules::AuditRefusal::KeyMaterialInRecord)
    );
    // With the key the record is written, with the key replaced by the label's
    // placeholder.
    let json = record
        .scrubbed_with_key(b"sk-live-0123456789abcdef")
        .and_then(|clean| clean.to_json_line_with_key(KEY))
        .expect("a header-shaped field is scrubbed");
    assert!(!json.contains("sk-live-0123456789abcdef"), "{json}");
    assert!(json.contains("{{ck-secret:repo_key}}"), "{json}");
}

#[test]
fn audit_metadata_is_validated_at_construction() {
    let scope = ScopeRef {
        owner: "principal".to_string(),
        kind: "kind".to_string(),
        reference: "ref".to_string(),
    };
    assert!(scope.is_valid());
    let bad = ScopeRef {
        owner: "principal".to_string(),
        kind: "kind".to_string(),
        reference: "line\nbreak".to_string(),
    };
    assert!(!bad.is_valid());
}

#[test]
fn a_clean_record_round_trips_without_scrubbing() {
    let record = AuditRecordV1::new(
        7,
        "repo_key",
        Decision::Substituted,
        secret_rules::AuditReason::Matched,
        "req-1",
        "task-1",
    );
    let json = record
        .to_json_line_with_key(KEY)
        .expect("clean record serializes");
    assert!(json.contains("\"reason\":\"matched\""));
    assert!(json.contains("\"label\":\"repo_key\""));
}

#[test]
fn request_outcome_is_not_partial_after_a_refusal() {
    let rules = [rule("repo_key", Location::Header("X-A".to_string()))];
    let request = RequestView::new(
        "/v1/x",
        vec![
            HeaderField::value("X-A", PLACEHOLDER.as_bytes()),
            HeaderField::value("X-A", PLACEHOLDER.as_bytes()),
        ],
    );
    let outcome: RequestOutcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert!(outcome.refusal.is_some());
    assert_eq!(outcome.header_edits.len(), 0);
}

// --- minimal counterexamples, one per audit item ---------------------------

#[test]
fn item4_a_request_for_label_b_never_receives_label_a_key() {
    // Audit: rules A and B, the request names only B, and the caller supplies
    // A's key. B must not be substituted with A's key.
    let rules = [
        rule("alpha", Location::Header("X-Alpha".to_string())),
        LabelRule {
            label: "beta".to_string(),
            host: "beta.example.com".to_string(),
            ..rule("beta", Location::Header("X-Beta".to_string()))
        },
    ];
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("X-Beta", b"{{ck-secret:beta}}")],
    );
    let connect = ConnectAuthority::parse("beta.example.com:443").unwrap();
    let outcome = evaluate_request_with_connect(
        &request,
        &connect,
        &rules,
        &[("alpha".to_string(), KEY.to_vec())],
    );
    // `beta` has no key of its own, so the request refuses; A's key is never
    // written into B's header.
    assert_eq!(outcome.refusal, Some(RequestRefusal::MissingMaterial));
    assert!(outcome.header_edits.is_empty());
}

#[test]
fn item6_absolute_form_target_on_another_host_refuses() {
    // Audit: tunnel to api.example.com, absolute-form target to evil.example.
    let rules = [rule("repo_key", Location::Bearer)];
    let bearer = format!("Bearer {PLACEHOLDER}");
    let request = RequestView::new(
        "https://evil.example/v1/x",
        vec![HeaderField::value("Authorization", bearer.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::TargetMismatch(
            TargetRefusal::AuthorityMismatch
        ))
    );
}

#[test]
fn item7_header_substitution_before_a_refusal_leaves_no_key() {
    // Audit: rule 1 substitutes X-A, rule 2 declares X-B which appears twice.
    let rules = [
        rule("repo_key", Location::Header("X-A".to_string())),
        rule("repo_key", Location::Header("X-B".to_string())),
    ];
    let request = RequestView::new(
        "/v1/x",
        vec![
            HeaderField::value("X-A", PLACEHOLDER.as_bytes()),
            HeaderField::value("X-B", PLACEHOLDER.as_bytes()),
            HeaderField::value("X-B", PLACEHOLDER.as_bytes()),
        ],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
    let rendered = format!("{outcome:?}");
    assert!(!rendered.contains("sk-live-0123456789abcdef"), "{rendered}");
    assert!(outcome.header_edits.is_empty());
    assert!(!outcome.substituted());
}

#[test]
fn item8_ipv6_userinfo_is_never_authorized() {
    // Audit: https://[::1]@evil.example/v1?k=... parsed as host [::1].
    assert_eq!(
        split_absolute("https://[::1]@evil.example/v1?k=x"),
        Err(TargetRefusal::MalformedAuthority)
    );
}

#[test]
fn item9_double_encoded_traversal_is_refused() {
    // Audit: /v1/%252e%252e/echo passed the /v1 prefix.
    assert_eq!(
        normalize_request_target("/v1/%252e%252e/echo"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
}

#[test]
fn item11_encoded_query_alias_is_a_duplicate() {
    // Audit: /v1?key=...&%6bey=other.
    let rules = [rule("repo_key", Location::Query("key".to_string()))];
    let target_text = format!("/v1/x?key={PLACEHOLDER}&%6bey=other");
    let request = RequestView::new(&target_text, Vec::new());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
}

#[test]
fn item11_duplicate_json_members_refuse() {
    // Audit: {"key":"...","key":"..."}.
    let rules = [rule("repo_key", Location::Json("/key".to_string()))];
    let body = format!("{{\"key\":\"{PLACEHOLDER}\",\"key\":\"{PLACEHOLDER}\"}}");
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
}

#[test]
fn item12_a_fragment_is_not_a_query() {
    let rules = [rule("repo_key", Location::Query("k".to_string()))];
    let target_text = format!("/v1/x#{PLACEHOLDER}?k=1");
    let request = RequestView::new(&target_text, Vec::new());
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    assert!(!outcome.substituted());
}

#[test]
fn item12_debug_output_does_not_print_key_bytes() {
    let rules = [rule("repo_key", Location::Header("X-A".to_string()))];
    let request = RequestView::new(
        "/v1/x",
        vec![HeaderField::value("X-A", PLACEHOLDER.as_bytes())],
    );
    let outcome = evaluate_request_with_connect(
        &request,
        &connect(),
        &rules,
        &[("repo_key".to_string(), KEY.to_vec())],
    );
    let rendered = format!("{outcome:?}");
    assert!(!rendered.contains("sk-live-0123456789abcdef"), "{rendered}");
}

#[test]
fn item13_the_audit_record_refuses_key_shaped_metadata() {
    // Audit: the key written into `origin`.
    let record = AuditRecordV1::new(
        1,
        "repo_key",
        Decision::Literal,
        secret_rules::AuditReason::Matched,
        "r",
        "sk-live-0123456789abcdef",
    );
    assert!(record.to_json_line_with_key(KEY).is_err());
}

#[test]
fn item13_the_audit_record_reason_is_not_free_text() {
    assert!(secret_rules::AuditReason::parse("secrets_remote_refused").is_none());
    let key = b"sk-live-0123456789abcdef";
    assert!(secret_rules::AuditReason::parse(std::str::from_utf8(key).unwrap()).is_none());
}
