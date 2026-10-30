//! P2 and P4: the location model, admissibility, encoding and the refusals
//! that stop AFT from guessing.

use secret_rules::{
    apply_target_edits, evaluate_request_with_connect, is_json_content_type, location_kind_token,
    ConnectAuthority, HeaderField, LabelOutcome, LabelRule, Location, RequestRefusal, RequestView,
};

const KEY: &[u8] = b"sk-live-0123456789abcdef";

fn rule(location: Location) -> LabelRule {
    LabelRule {
        label: "repo_key".to_string(),
        host: "api.example.com".to_string(),
        port: 443,
        path_prefix: "/v1".to_string(),
        location,
        grant_id: "grant-1".to_string(),
    }
}

fn connect() -> ConnectAuthority {
    ConnectAuthority::parse("api.example.com:443").unwrap()
}

fn keys(key: &[u8]) -> Vec<(String, Vec<u8>)> {
    vec![("repo_key".to_string(), key.to_vec())]
}

fn placeholder() -> &'static str {
    "{{ck-secret:repo_key}}"
}

#[test]
fn bearer_rewrites_the_whole_value_and_keeps_the_scheme() {
    let rules = [rule(Location::Bearer)];

    // A ranged whole value is replaced whole: the edit covers the value's
    // range and the replacement keeps the `Bearer ` scheme before the key.
    let text = format!("Host: x\r\nAuthorization: Bearer {}\r\n", placeholder());
    let start = text.find("Bearer ").unwrap();
    let value = HeaderField::ranged("Authorization", start, text.len() - 2);
    let header_block = Box::leak(text.clone().into_bytes().into_boxed_slice());
    let request = RequestView::from_header_block("/v1/things", vec![value], header_block);
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, None);
    assert_eq!(outcome.labels.len(), 1);
    let mut expected = b"Bearer ".to_vec();
    expected.extend_from_slice(KEY);
    match &outcome.labels[0] {
        LabelOutcome::Substituted {
            byte_start,
            byte_end,
            replacement,
        } => {
            assert_eq!(*byte_start, start);
            assert_eq!(*byte_end, text.len() - 2);
            assert_eq!(replacement.as_slice(), expected.as_slice());
        }
        other => panic!("expected substitution, got {other:?}"),
    }

    // The scheme is case-insensitive.
    let lower_text = format!("bearer {}", placeholder());
    let value = HeaderField::value("Authorization", lower_text.as_bytes());
    let request = RequestView::new("/v1/things", vec![value]);
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert!(matches!(
        outcome.labels[0],
        LabelOutcome::Substituted { .. }
    ));
}

#[test]
fn bearer_value_that_is_not_the_whole_credential_is_literal() {
    let rules = [rule(Location::Bearer)];
    let value_bytes = format!("Bearer x{}", placeholder());
    let value = HeaderField::value("Authorization", value_bytes.as_bytes());
    let request = RequestView::new("/v1/things", vec![value]);
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.labels, vec![LabelOutcome::NotWholeValue]);
    assert!(!outcome.substituted());
}

#[test]
fn a_placeholder_elsewhere_in_the_request_is_literal() {
    let rules = [rule(Location::Header("X-Api-Key".to_string()))];
    let value_bytes = placeholder().as_bytes();
    let request = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Authorization", value_bytes)],
    )
    .with_body(placeholder().as_bytes());
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.labels, vec![LabelOutcome::OutOfScope]);
}

#[test]
fn query_substitutes_only_after_percent_decoding_the_whole_value() {
    let rules = [rule(Location::Query("key".to_string()))];
    let encoded = "/v1/things?key=%7B%7Bck-secret%3Arepo_key%7D%7D&other=1";
    let request = RequestView::new(encoded, Vec::new());
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, None);
    let rewritten = apply_target_edits(encoded, &outcome.target_edits);
    assert_eq!(
        rewritten,
        format!("/v1/things?key={}&other=1", String::from_utf8_lossy(KEY))
    );

    let partial = "/v1/things?key=prefix%7B%7Bck-secret%3Arepo_key%7D%7D";
    let request = RequestView::new(partial, Vec::new());
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.labels, vec![LabelOutcome::NotWholeValue]);
}

#[test]
fn header_substitution_targets_the_whole_value() {
    let rules = [rule(Location::Header("X-Api-Key".to_string()))];
    let buffer = format!("Host: x\r\nX-Api-Key: {}\r\n", placeholder());
    let start = buffer.find("X-Api-Key: ").unwrap() + "X-Api-Key: ".len();
    let end = buffer.len() - 2;
    let value = HeaderField::ranged("X-Api-Key", start, end);
    let header_block = Box::leak(buffer.clone().into_bytes().into_boxed_slice());
    let request = RequestView::from_header_block("/v1/things", vec![value], header_block);
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    match outcome.labels.first() {
        Some(LabelOutcome::Substituted {
            byte_start,
            byte_end,
            replacement,
        }) => {
            assert_eq!((*byte_start, *byte_end), (start, end));
            assert_eq!(replacement.as_slice(), KEY);
            assert_eq!(&buffer[start..end], placeholder());
        }
        other => panic!("expected substitution, got {other:?}"),
    }
}

#[test]
fn json_pointer_substitutes_a_string_and_keeps_the_rest_of_the_body() {
    let rules = [rule(Location::Json("/auth/key".to_string()))];
    let body = format!(
        "{{\"z\":1, \"auth\": {{\"key\": \"{}\"}}, \"tail\": 2}}",
        placeholder()
    );
    let request = RequestView::new(
        "/v1/things",
        vec![
            HeaderField::value("Content-Type", b"application/json"),
            HeaderField::value("Authorization", b"nothing"),
        ],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, None);
    let rewritten =
        String::from_utf8(outcome.rewritten_body.expect("body rewritten").to_vec()).unwrap();
    assert_eq!(
        rewritten,
        format!(
            "{{\"z\":1, \"auth\": {{\"key\": \"{}\"}}, \"tail\": 2}}",
            String::from_utf8_lossy(KEY)
        )
    );
}

#[test]
fn json_pointer_escapes_tilde_and_slash_tokens() {
    let rules = [rule(Location::Json("/a~1b~0c".to_string()))];
    let body = format!("{{\"a/b~c\": \"{}\"}}", placeholder());
    let request = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, None);
    assert!(outcome.rewritten_body.is_some());
}

#[test]
fn json_refusals_cover_every_p4_case() {
    let rules = [rule(Location::Json("/key".to_string()))];
    let body = format!("{{\"key\": \"{}\"}}", placeholder());

    let non_string = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(b"{\"key\": \"{{ck-secret:repo_key}}\", \"other\": 1}");
    let outcome = evaluate_request_with_connect(&non_string, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, None, "a string target substitutes");

    let number_target = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(b"{\"key\": 12}");
    let outcome = evaluate_request_with_connect(&number_target, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.labels, vec![LabelOutcome::OutOfScope]);

    let wrong_type = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"text/plain")],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(&wrong_type, &connect(), &rules, &keys(KEY));
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::JsonContentTypeMissing)
    );

    let encoded = RequestView::new(
        "/v1/things",
        vec![
            HeaderField::value("Content-Type", b"application/json"),
            HeaderField::value("Content-Encoding", b"gzip"),
        ],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(&encoded, &connect(), &rules, &keys(KEY));
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::ContentEncodingPresent)
    );

    let missing_type = RequestView::new("/v1/things", Vec::new()).with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(&missing_type, &connect(), &rules, &keys(KEY));
    assert_eq!(
        outcome.refusal,
        Some(RequestRefusal::JsonContentTypeMissing)
    );

    let malformed = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(b"{\"key\": ");
    let outcome = evaluate_request_with_connect(&malformed, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, Some(RequestRefusal::MalformedJsonBody));

    let oversized = vec![b' '; secret_rules::location::MAX_SUBSTITUTABLE_BODY + 1];
    let big = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(&oversized);
    let outcome = evaluate_request_with_connect(&big, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, Some(RequestRefusal::BodyTooLarge));

    let bad_pointer = [rule(Location::Json("auth/key".to_string()))];
    let request = RequestView::new(
        "/v1/things",
        vec![HeaderField::value("Content-Type", b"application/json")],
    )
    .with_body(body.as_bytes());
    let outcome = evaluate_request_with_connect(&request, &connect(), &bad_pointer, &keys(KEY));
    assert_eq!(outcome.refusal, Some(RequestRefusal::InvalidJsonPointer));
}

#[test]
fn duplicate_declared_fields_refuse() {
    let rules = [rule(Location::Header("X-Api-Key".to_string()))];
    let request = RequestView::new(
        "/v1/things",
        vec![
            HeaderField::value("X-Api-Key", placeholder().as_bytes()),
            HeaderField::value("x-api-key", placeholder().as_bytes()),
        ],
    );
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));

    let query_rules = [rule(Location::Query("key".to_string()))];
    let query = format!("/v1/things?key={}&key=other", placeholder());
    let request = RequestView::new(&query, Vec::new());
    let outcome = evaluate_request_with_connect(&request, &connect(), &query_rules, &keys(KEY));
    assert_eq!(outcome.refusal, Some(RequestRefusal::DuplicateField));
}

#[test]
fn every_invalid_header_name_refuses_and_names_the_reason() {
    let reserved = [
        "Host",
        "Content-Length",
        "Content-Type",
        "Content-Encoding",
        "Transfer-Encoding",
        "Connection",
        "Keep-Alive",
        "Proxy-Connection",
        "Proxy-Authorization",
        "Upgrade",
        "TE",
        "Trailer",
        "Accept-Encoding",
        "Cookie",
        "Sec-WebSocket-Extensions",
        "Sec-WebSocket-Protocol",
        ":authority",
        ":path",
    ];
    for name in reserved {
        let rules = [rule(Location::Header(name.to_string()))];
        let request = RequestView::new("/v1/things", Vec::new());
        let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
        assert_eq!(
            outcome.refusal,
            Some(RequestRefusal::ReservedHeaderName),
            "{name} must be reserved"
        );
    }

    for name in ["Bad Name", "Bad:Name", "", "Bad\nName"] {
        let rules = [rule(Location::Header(name.to_string()))];
        let request = RequestView::new("/v1/things", Vec::new());
        let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
        assert_eq!(
            outcome.refusal,
            Some(RequestRefusal::InvalidHeaderName),
            "{name:?} must be an invalid field name"
        );
    }
}

#[test]
fn an_admissible_rule_far_from_the_path_does_not_refuse_the_request() {
    let rules = [rule(Location::Header("X-Api-Key".to_string()))];
    let request = RequestView::new(
        "/elsewhere",
        vec![HeaderField::value("X-Api-Key", placeholder().as_bytes())],
    );
    let outcome = evaluate_request_with_connect(&request, &connect(), &rules, &keys(KEY));
    assert_eq!(outcome.refusal, None);
    assert_eq!(outcome.labels, vec![LabelOutcome::OutOfScope]);
}

#[test]
fn location_kind_tokens_are_stable() {
    assert_eq!(location_kind_token(&Location::Bearer), "bearer");
    assert_eq!(
        location_kind_token(&Location::Header("X-Api-Key".to_string())),
        "header:x-api-key"
    );
    assert_eq!(
        location_kind_token(&Location::Query("key".to_string())),
        "query:key"
    );
    assert_eq!(
        location_kind_token(&Location::Json("/a/b".to_string())),
        "json:/a/b"
    );
}

#[test]
fn json_content_types_accept_application_json_and_plus_json() {
    assert!(is_json_content_type("application/json"));
    assert!(is_json_content_type("application/json; charset=utf-8"));
    assert!(is_json_content_type("application/merge-patch+json"));
    assert!(!is_json_content_type("text/plain"));
    assert!(!is_json_content_type("application/x-www-form-urlencoded"));
}
