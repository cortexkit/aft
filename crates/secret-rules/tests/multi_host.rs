//! Only scope parsing, canonical emission and matching belong to this crate.
//! Approval signatures, creation settings, storage, labels and byte envelopes
//! remain vault responsibilities; their samples still exercise embedded scopes.

use secret_rules::scope::{SecretScope, UseTarget, PSL_SNAPSHOT};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MATCH: &str = include_str!("vectors/multi-host/secret-scope-match-v1.json");
const APPROVAL: &str = include_str!("vectors/multi-host/secret-approval-v3.json");
const FLOW: &str = include_str!("vectors/multi-host/secret-flow-approval-v3.json");
const DEPOSIT: &str = include_str!("vectors/multi-host/secret-deposit-v2.json");
const LIST: &str = include_str!("vectors/multi-host/secret-list-v2.json");

fn scope_fields(value: &Value, admin: bool) -> Value {
    let mut fields = serde_json::Map::new();
    for name in ["host", "path_prefix", "rules", "origin", "origins"] {
        if let Some(value) = value.get(name) {
            if !(admin && value.is_null() && matches!(name, "host" | "path_prefix" | "origin")) {
                fields.insert(name.into(), value.clone());
            }
        }
    }
    Value::Object(fields)
}

#[test]
fn multi_host_scope_match_corpus() {
    let corpus: Value = serde_json::from_str(MATCH).unwrap();
    assert_eq!(corpus["v"], "ck-secret-scope-match/v1");
    assert_eq!(
        corpus["psl_snapshot"],
        json!({
            "url": PSL_SNAPSHOT.url, "date": PSL_SNAPSHOT.date, "sha256": PSL_SNAPSHOT.sha256
        })
    );
    let mut ran = 0;
    for case in corpus["cases"].as_array().unwrap() {
        let name = case["case"].as_str().unwrap();
        let kind = case["kind"].as_str().unwrap();
        let scope = match case["form"].as_str().unwrap() {
            "input" => SecretScope::parse_input(kind, &case["scope"]),
            "stored" => SecretScope::decode_stored(kind, &case["scope"]),
            other => panic!("unknown form {other} in {name}"),
        };
        if let Some(code) = case["expect"].get("scope_refused") {
            assert_eq!(scope.unwrap_err().code(), code.as_str().unwrap(), "{name}");
        } else {
            let scope = scope.unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let target = &case["target"];
            let target = if let Some(origin) = target["origin"].as_str() {
                UseTarget::SiteLogin { origin }
            } else {
                UseTarget::ApiKey {
                    scheme: target["scheme"].as_str().unwrap(),
                    host: target["host"].as_str().unwrap(),
                    port: target["port"].as_u64().unwrap().try_into().unwrap(),
                    path: target["path"].as_str().unwrap(),
                }
            };
            assert_eq!(scope.matches(target), case["expect"] == "match", "{name}");
            assert_eq!(
                SecretScope::decode_stored(kind, &scope.to_value()).unwrap(),
                scope,
                "{name}"
            );
        }
        ran += 1;
    }
    assert_eq!(ran, 92);
}

#[test]
fn multi_host_vault_corpus_scope_projections() {
    for (text, total, input_count, stored_count, statement_count) in [
        (APPROVAL, 11, 0, 11, 11),
        (FLOW, 15, 0, 15, 11),
        (DEPOSIT, 36, 34, 0, 0),
        (LIST, 12, 0, 8, 0),
    ] {
        let corpus: Value = serde_json::from_str(text).unwrap();
        let cases = corpus["cases"].as_array().unwrap();
        assert_eq!(cases.len(), total);
        let (mut inputs, mut stored, mut statements) = (0, 0, 0);
        for case in cases {
            let name = case["case"].as_str().unwrap();
            if let Some(params) = case.get("params").filter(|p| p.get("kind").is_some()) {
                let admin = case["operation"] == "admin.secret_put";
                let fields = scope_fields(params, admin);
                let parsed = SecretScope::parse_input(params["kind"].as_str().unwrap(), &fields);
                let expected = case["expect"]["code"].as_str();
                if expected == Some("invalid_params") && name != "invalid-location" {
                    assert_eq!(parsed.unwrap_err().code(), "invalid_params", "{name}");
                } else if name == "nine-rules-with-invalid-location" {
                    assert_eq!(parsed.unwrap_err().code(), "limit_exceeded", "{name}");
                } else {
                    let scope = parsed.unwrap_or_else(|error| panic!("{name}: {error:?}"));
                    if let Some(expected) = case["expect"].get("stored_scope") {
                        assert_eq!(scope.to_value(), scope_fields(expected, false), "{name}");
                    }
                }
                inputs += 1;
            }
            if let Some(scope) = case["setup"].get("stored_scope") {
                let fields = scope_fields(scope, false);
                let parsed =
                    SecretScope::decode_stored(case["setup"]["kind"].as_str().unwrap(), &fields)
                        .unwrap_or_else(|error| panic!("{name}: {error:?}"));
                assert_eq!(parsed.to_value(), fields, "{name}");
                if let Some(statement) = case.get("statement") {
                    assert_eq!(
                        parsed.to_value(),
                        scope_fields(&statement["use"], false),
                        "{name}"
                    );
                    statements += 1;
                }
                stored += 1;
            }
        }
        assert_eq!(
            (inputs, stored, statements),
            (input_count, stored_count, statement_count)
        );
    }
}

#[test]
fn multi_host_corpus_bytes_match_manifest() {
    let manifest = include_str!("vectors/multi-host/SHA256SUMS");
    assert_eq!(
        format!("{:x}", Sha256::digest(manifest.as_bytes())),
        "2b0ec7ffc87a62bd027939648320f94e54fcd6a2b8af8dbf7dbd4a6cd60f0207"
    );
    let files = [
        ("secret-scope-match-v1.json", MATCH),
        ("secret-approval-v3.json", APPROVAL),
        ("secret-flow-approval-v3.json", FLOW),
        ("secret-deposit-v2.json", DEPOSIT),
        ("secret-list-v2.json", LIST),
    ];
    let mut ran = 0;
    for line in manifest.lines() {
        let (hash, path) = line.split_once("  ").unwrap();
        let (_, bytes) = files.iter().find(|(name, _)| path.ends_with(name)).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(bytes.as_bytes())),
            hash,
            "{path}"
        );
        ran += 1;
    }
    assert_eq!(ran, 5);
}

#[test]
fn stored_scopes_require_emitted_canonical_fields() {
    for fields in [
        json!({"rules": [{"host": "*.x.test"}]}),
        json!({"rules": [{"host": "b.x.test", "path_prefix": ""},
                         {"host": "a.x.test", "path_prefix": ""}]}),
        json!({"rules": [{"host": "*.x.test", "path_prefix": null}]}),
    ] {
        assert_eq!(
            SecretScope::decode_stored("agent_api_key", &fields)
                .unwrap_err()
                .code(),
            "invalid_params"
        );
    }
    assert_eq!(
        SecretScope::decode_stored(
            "site_login",
            &json!({
                "origins": ["https://z.test", "https://a.test"]
            })
        )
        .unwrap_err()
        .code(),
        "invalid_params"
    );
    let input = json!({"rules": [{"host": "z.x.test"}, {"host": "a.x.test"}]});
    let scope = SecretScope::parse_input("agent_api_key", &input).unwrap();
    assert!(scope.is_extended());
    assert_eq!(
        scope.to_value(),
        json!({"rules": [
            {"host": "a.x.test", "path_prefix": ""}, {"host": "z.x.test", "path_prefix": ""}
        ]})
    );
    assert!(
        !SecretScope::parse_input("agent_api_key", &json!({"host": "a.x.test"}))
            .unwrap()
            .is_extended()
    );
}

#[test]
fn scope_shape_and_prefix_grammar_are_closed() {
    for fields in [
        json!({"host": "a.test", "unknown": "value"}),
        json!({"rules": [{"host": "*.x.test"}], "host": null}),
        json!({"rules": [{"host": "*.x.test"}], "origin": "https://x.test"}),
        json!({"rules": [{"host": "*.x.test"}], "path_prefix": null}),
    ] {
        assert_eq!(
            SecretScope::parse_input("agent_api_key", &fields)
                .unwrap_err()
                .code(),
            "invalid_params"
        );
    }
    for prefix in [
        "/a/.", "/a/..;x", "/a\\b", "/%00", "/%20", "/%7F", "/%2e", "/%25", "/%5c", "/%3b", "/a?b",
        "/a#b", "/é",
    ] {
        assert_eq!(
            SecretScope::parse_input(
                "agent_api_key",
                &json!({
                    "host": "a.test", "path_prefix": prefix
                })
            )
            .unwrap_err()
            .code(),
            "invalid_params",
            "{prefix}"
        );
    }
    for prefix in ["/a;v=1", "/%7E", "/a:b@c!$&'()*+,=", "/a.../b"] {
        assert!(
            SecretScope::parse_input(
                "agent_api_key",
                &json!({
                    "host": "a.test", "path_prefix": prefix
                })
            )
            .is_ok(),
            "{prefix}"
        );
    }
}
