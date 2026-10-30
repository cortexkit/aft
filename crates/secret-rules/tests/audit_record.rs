//! A1: the audit record schema, and that no field can hold key bytes.

use secret_rules::{AuditReason, AuditRecordV1, Decision, ScopeRef};

const KEY: &[u8] = b"sk-live-0123456789abcdef";

#[test]
fn a_full_record_serializes_with_every_field_and_version_one() {
    let record = AuditRecordV1::new(
        1_760_000_000_000,
        "repo_key",
        Decision::Substituted,
        AuditReason::Matched,
        "req-1",
        "task-9",
    )
    .with_scope(Some(ScopeRef {
        owner: "principal-a".to_string(),
        kind: "head".to_string(),
        reference: "ref-1".to_string(),
    }))
    .with_rule("api.example.com", 443, "/v1", "header:x-api-key");
    let json = record
        .to_json_line_with_key(KEY)
        .expect("clean record serializes");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["v"], 1);
    assert_eq!(parsed["timestamp_ms"], 1_760_000_000_000u64);
    assert_eq!(parsed["label"], "repo_key");
    assert_eq!(parsed["scope"]["owner"], "principal-a");
    assert_eq!(parsed["scope"]["kind"], "head");
    assert_eq!(parsed["scope"]["ref"], "ref-1");
    assert_eq!(parsed["host"], "api.example.com");
    assert_eq!(parsed["port"], 443);
    assert_eq!(parsed["matched_prefix"]["path_prefix"], "/v1");
    assert_eq!(parsed["location_kind"], "header:x-api-key");
    assert_eq!(parsed["decision"], "substituted");
    assert_eq!(parsed["reason"], "matched");
    assert_eq!(parsed["request_id"], "req-1");
    assert_eq!(parsed["origin"], "task-9");
}

#[test]
fn a_launch_refusal_record_has_null_rule_fields_and_null_scope() {
    let record = AuditRecordV1::new(
        1_760_000_000_001,
        "repo_key",
        Decision::Refused,
        AuditReason::Refused,
        "req-2",
        "task-9",
    );
    let json = record
        .to_json_line_with_key(KEY)
        .expect("clean record serializes");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    for field in ["scope", "host", "port", "matched_prefix", "location_kind"] {
        assert!(parsed[field].is_null(), "{field} must be null");
    }
    assert_eq!(parsed["decision"], "refused");
}

#[test]
fn decisions_serialize_as_their_snake_case_names() {
    for (decision, text) in [
        (Decision::Substituted, "\"substituted\""),
        (Decision::Literal, "\"literal\""),
        (Decision::Refused, "\"refused\""),
    ] {
        assert_eq!(serde_json::to_string(&decision).unwrap(), text);
    }
}

#[test]
fn a_written_record_has_only_the_schema_fields() {
    // A clean record, written with the key check: the output carries no key
    // text, and the schema has no field for a query string, a URL or a body.
    let key_text = std::str::from_utf8(KEY).unwrap();
    let record = AuditRecordV1::new(
        1,
        "repo_key",
        Decision::Literal,
        AuditReason::NotWholeValue,
        "req-3",
        "task-9",
    )
    .with_rule("api.example.com", 443, "/v1", "query:key");
    let json = record
        .to_json_line_with_key(KEY)
        .expect("clean record serializes");
    assert!(
        !json.contains(key_text),
        "record must not contain the key: {json}"
    );
    let encoded = secret_rules::scrub::base64_encode(
        KEY,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
        false,
    );
    assert!(
        !json.contains(&String::from_utf8_lossy(&encoded).into_owned()),
        "record must not contain the key's base64 form: {json}"
    );
    // The struct has no field for a value, a URL or a body: enumerate the keys
    // so a future addition is caught by this test.
    let parsed: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&json).unwrap();
    let mut fields: Vec<&str> = parsed.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        vec![
            "decision",
            "host",
            "label",
            "location_kind",
            "matched_prefix",
            "origin",
            "port",
            "reason",
            "request_id",
            "scope",
            "timestamp_ms",
            "v",
        ]
    );
}

#[test]
fn records_round_trip_through_serde() {
    let record = AuditRecordV1::new(
        7,
        "repo_key",
        Decision::Substituted,
        AuditReason::Matched,
        "req-4",
        "fetch-1#2",
    );
    // The record has no `Serialize` impl on purpose (a compile_fail doc test on
    // `AuditRecordV1` checks that); the writers check every field first.
    let json = record
        .to_json_line_with_key(KEY)
        .expect("clean record serializes");
    let back: AuditRecordV1 = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);
}
