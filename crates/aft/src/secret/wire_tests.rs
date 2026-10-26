//! The `secret.use` seam against the contract's golden examples and the pinned
//! approval vectors.
//!
//! `crates/aft/tests/fixtures/secret/secret-approval-v1.json` is a byte-for-byte
//! copy of claustrum's `crates/credentials-core/tests/vectors/secret-approval-v1.json`
//! (sha256 `efd4ba6064…495cc27`). Reading it here proves three things at once:
//! the request AFT builds carries exactly the contract's key set, a reply is
//! read by exactly the contract's rules, and the statement AFT hands to the
//! consent ask is the signed bytes the phone will answer.

use super::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// The pinned vector file, as shipped in this repository.
const VECTORS: &str = include_str!("../../tests/fixtures/secret/secret-approval-v1.json");

/// The sha256 the campaign pinned for the vector file.
const VECTORS_SHA256: &str = "efd4ba60641586e143bce81087e4f9f5aaf8a99ffede74190303da7d4495cc27";

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("the pinned vector file is JSON")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn base64url_decode(text: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .unwrap_or_else(|error| panic!("strict unpadded base64url: {error}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(bytes))
}

/// The daemon stamp the contract's golden request relays.
fn golden_stamp() -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": "H",
        "scope_epoch": 7,
        "kind": "head",
        "attributes": {"agent_id": "agent-A"},
        "owner_authorized": true,
    }))
    .unwrap()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn the_shipped_vector_file_is_the_pinned_one() {
    assert_eq!(sha256_hex(VECTORS.as_bytes()), VECTORS_SHA256);
}

#[test]
fn the_golden_api_key_request_has_exactly_the_contract_key_set() {
    let request = SecretUseRequest::new(
        "key",
        &golden_stamp(),
        Target::ApiKey(ApiKeyTarget {
            scheme: "https".into(),
            host: "x.test".into(),
            port: Some(443),
            path: "/repos/a/issues/1".into(),
        }),
    )
    .with_coverage(Coverage::Scope);
    let params = request.to_params();
    assert_eq!(keys(&params), set(&["name", "coverage", "stamp", "target"]));
    assert_eq!(
        params["target"],
        json!({"scheme": "https", "host": "x.test", "port": 443, "path": "/repos/a/issues/1"})
    );
    // The stamp is relayed unchanged, attribute object included.
    assert_eq!(params["stamp"], json!(golden_stamp()));
    assert_eq!(params["stamp"]["attributes"]["agent_id"], json!("agent-A"));
    // No agent name is ever sent: AFT holds no trusted one, and the contract's
    // key set is closed.
    assert!(params.get("display_name").is_none());
}

#[test]
fn the_default_request_matches_the_contracts_defaults_example() {
    // `coverage` and `port` omitted is the same call as the golden example
    // with them, per the contract's own "defaults" example.
    let request = SecretUseRequest::new(
        "key",
        &golden_stamp(),
        Target::ApiKey(ApiKeyTarget {
            scheme: "https".into(),
            host: "x.test".into(),
            port: None,
            path: "/repos/a/issues/1".into(),
        }),
    );
    let params = request.to_params();
    assert_eq!(keys(&params), set(&["name", "stamp", "target"]));
    assert_eq!(
        params["target"],
        json!({"scheme": "https", "host": "x.test", "path": "/repos/a/issues/1"})
    );
}

#[test]
fn a_golden_released_api_key_reply_parses_to_the_key_and_its_location() {
    let reply = parse_reply(&json!({
        "status": "released",
        "kind": "agent_api_key",
        "key_b64url": "c2stZXhhbXBsZQ",
        "location": "header:X-Api-Key",
    }))
    .expect("the contract's released api key example parses");
    let SecretUseReply::ReleasedApiKey(key) = reply else {
        panic!("a released api key reply is the key form");
    };
    assert_eq!(key.material(), b"sk-example");
    assert_eq!(key.location, "header:X-Api-Key");
    // The location is one the rules crate can act on.
    assert!(secret_rules::location::parse_vault_location(&key.location).is_some());
    // Nothing in the debug form prints the key.
    assert!(!format!("{key:?}").contains("sk-example"));
}

#[test]
fn a_golden_released_login_reply_parses_with_and_without_totp() {
    let reply = parse_reply(&json!({
        "status": "released",
        "kind": "site_login",
        "username": "alice",
        "password": "correct horse",
        "totp": "123456",
    }))
    .expect("the contract's site login example parses");
    let SecretUseReply::ReleasedLogin(login) = reply else {
        panic!("a site login reply is the login form");
    };
    assert_eq!(login.username, "alice");
    assert_eq!(login.password(), b"correct horse");
    assert_eq!(login.totp.as_deref(), Some("123456"));

    let without = parse_reply(&json!({
        "status": "released",
        "kind": "site_login",
        "username": "alice",
        "password": "correct horse",
    }))
    .expect("a login without a TOTP seed carries no totp key");
    let SecretUseReply::ReleasedLogin(login) = without else {
        panic!("a site login reply is the login form");
    };
    assert_eq!(login.totp, None);
}

#[test]
fn a_released_reply_carrying_a_key_the_contract_does_not_show_is_refused() {
    // Each key set is closed, so an extra member is a protocol error rather
    // than something to ignore.
    for reply in [
        json!({"status":"released","kind":"agent_api_key","key_b64url":"c2stZXhhbXBsZQ","location":"header:X-Api-Key","expires_at_ms":1}),
        json!({"status":"released","kind":"site_login","username":"alice","password":"correct horse","totp":"123456","expires_at_ms":1}),
        json!({"status":"approval_required","request_id":"01HF7YAT000000000000000001","statement_b64url":"eyJ2IjoxfQ","expires_at_ms":1,"note":"x"}),
    ] {
        assert!(parse_reply(&reply).is_err(), "{reply}");
    }
    // A missing member is refused too.
    assert!(parse_reply(&json!({
        "status": "released", "kind": "agent_api_key", "key_b64url": "c2stZXhhbXBsZQ"
    }))
    .is_err());
}

#[test]
fn a_key_that_is_not_strict_unpadded_base64url_is_refused() {
    for key in ["c2stZXhhbXBsZQ==", "c2stZXhh bXBsZQ", "c2stZXhhbXBsZR"] {
        let reply = json!({
            "status": "released",
            "kind": "agent_api_key",
            "key_b64url": key,
            "location": "authorization_bearer",
        });
        assert_eq!(
            parse_reply(&reply).err(),
            Some(ReplyError::BadKeyEncoding),
            "{key}"
        );
    }
}

#[test]
fn the_golden_approval_required_reply_parses_to_its_statement() {
    let file = vectors();
    let statement = &file["vectors"][0]["statements"][0];
    let reply = parse_reply(&json!({
        "status": "approval_required",
        "request_id": statement["request_id"],
        "statement_b64url": statement["statement_b64url"],
        "expires_at_ms": 1700000600000u64,
    }))
    .expect("the golden approval_required reply parses");
    let SecretUseReply::ApprovalRequired(request) = reply else {
        panic!("an approval_required reply is the approval form");
    };
    assert_eq!(
        request.request_id,
        statement["request_id"].as_str().unwrap()
    );
    assert_eq!(request.expires_at_ms, 1700000600000);
    // The statement AFT hands to the consent ask is the exact bytes the phone
    // signs: decoding and hashing it must reproduce the vector's digest.
    assert_eq!(
        sha256_hex(&base64url_decode(&request.statement_b64url)),
        statement["statement_sha256"].as_str().unwrap()
    );
}

#[test]
fn every_approval_vector_statement_decodes_the_documented_fields() {
    let file = vectors();
    for vector in file["vectors"].as_array().expect("vectors is an array") {
        let case = vector["case"].as_str().unwrap();
        for statement in vector["statements"].as_array().unwrap() {
            let raw = base64url_decode(statement["statement_b64url"].as_str().unwrap());
            assert_eq!(
                sha256_hex(&raw),
                statement["statement_sha256"].as_str().unwrap(),
                "{case}"
            );
            let decoded: Value = serde_json::from_slice(&raw).expect("a statement is JSON");
            assert_eq!(
                decoded["request_id"],
                statement["request_id"].clone(),
                "{case}"
            );
            // The two statement domains the vectors cover.
            let version = decoded["v"].as_str().unwrap();
            assert!(
                version == "ck-secret-use-statement/v1"
                    || version == "ck-secret-delete-statement/v1",
                "{case}: {version}"
            );
            if version == "ck-secret-use-statement/v1" {
                // The approval statement's own scope names the mapped asking
                // scope, which is what AFT relays in the stamp.
                assert_eq!(decoded["scope"]["kind"], json!("head"), "{case}");
                assert_eq!(
                    decoded["scope"]["owner"],
                    json!("reserved:prefrontal-core"),
                    "{case}"
                );
                assert_eq!(decoded["scope"]["ref"], json!("H"), "{case}");
                assert_eq!(decoded["scope"]["epoch"], json!(7), "{case}");
                assert_eq!(decoded["agent_id"], json!("agent-A"), "{case}");
                assert_eq!(decoded["coverage"], json!("scope"), "{case}");
            }
        }
        // The envelope carries the statement digests, and it is the bytes the
        // phone signs: its own hash must match.
        let envelope = base64url_decode(vector["envelope_b64url"].as_str().unwrap());
        assert_eq!(
            sha256_hex(&envelope),
            vector["envelope_sha256"].as_str().unwrap(),
            "{case}"
        );
        let envelope: Value = serde_json::from_slice(&envelope).expect("an envelope is JSON");
        assert_eq!(envelope["v"], json!("ck-secret-use-envelope/v1"), "{case}");
        let items = envelope["items"].as_array().unwrap();
        // Every listed item is one of the case's statements. An envelope may
        // leave a pending request out (the `batch-omitted` case exists to prove
        // it), and it never invents a request id.
        for item in items {
            if item["request_id"] == json!("00000000000000000000000000") {
                // `batch-unknown`'s deliberately unknown item.
                continue;
            }
            assert!(
                vector["statements"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|statement| {
                        item["request_id"] == statement["request_id"]
                            && item["statement_sha256"] == statement["statement_sha256"]
                    }),
                "{case}: the envelope lists an item no statement backs"
            );
        }
        assert!(
            !items.is_empty(),
            "{case}: an envelope carries at least one item"
        );
    }
}

#[test]
fn a_tampered_envelope_vector_no_longer_matches_its_recorded_hash() {
    // The vector file ships a deliberately altered envelope for each case; it
    // must not hash to the accepted one, which is what makes the digest a
    // signature input rather than a decoration.
    for vector in vectors()["vectors"].as_array().unwrap() {
        let case = vector["case"].as_str().unwrap();
        let tampered = base64url_decode(vector["tampered_envelope_b64url"].as_str().unwrap());
        assert_ne!(
            sha256_hex(&tampered),
            vector["envelope_sha256"].as_str().unwrap(),
            "{case}"
        );
    }
}

#[test]
fn a_result_error_and_a_transport_error_read_as_the_contract_spells_them() {
    let file = vectors();
    // A result error carries the code and class (§15's table).
    let answer =
        parse_answer(&json!({"result":{"error":{"code":"not_found","class":"permanent"}}}))
            .expect("a result error parses");
    let SecretUseAnswer::Refused(error) = answer else {
        panic!("a result error is a refusal");
    };
    assert_eq!(error.code, "not_found");
    assert_eq!(error.class.as_deref(), Some("permanent"));
    assert!(!error.is_transient());

    // A transport error carries a fixed message and no class (§1a).
    let answer = parse_answer(&json!({
        "code": "invalid_params",
        "message": "invalid_params"
    }))
    .expect("a transport error parses");
    let SecretUseAnswer::Rejected(error) = answer else {
        panic!("a transport error is a rejection");
    };
    assert_eq!(error.code, "invalid_params");

    // The reply forms ride inside `result`.
    let answer = parse_answer(&json!({"result":{
        "status": "released",
        "kind": "agent_api_key",
        "key_b64url": "c2stZXhhbXBsZQ",
        "location": "header:X-Api-Key"
    }}))
    .expect("a wrapped release parses");
    assert!(matches!(
        answer,
        SecretUseAnswer::Reply(SecretUseReply::ReleasedApiKey(_))
    ));
    assert!(!file["vectors"].as_array().unwrap().is_empty());
}

#[test]
fn the_secret_use_client_sends_the_pinned_params_and_reads_a_release() {
    struct Recording(std::sync::Mutex<Vec<Value>>);
    impl SecretUseTransport for Recording {
        fn secret_use(&self, params: Value) -> Result<Value, String> {
            self.0.lock().unwrap().push(params);
            Ok(json!({"result":{
                "status": "released",
                "kind": "agent_api_key",
                "key_b64url": "c2stZXhhbXBsZQ",
                "location": "header:X-Api-Key"
            }}))
        }
    }
    let transport = std::sync::Arc::new(Recording(std::sync::Mutex::new(Vec::new())));
    let vault = SubcSecretVault::new(transport.clone());
    let scope = SecretScope::from_stamp(&golden_stamp());
    let resolution = vault.resolve(&scope, "key");
    // A released key at resolution time is reported as a granted rule and the
    // bytes themselves are dropped (zeroized), per the contract's material rule.
    let Resolution::Granted { rule, .. } = resolution else {
        panic!("a release at resolution time is a grant");
    };
    assert_eq!(rule.label, "key");
    assert!(matches!(
        rule.location,
        secret_rules::location::Location::Header(ref name) if name == "X-Api-Key"
    ));
    let sent = transport.0.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(keys(&sent[0]), set(&["name", "stamp", "target"]));
    assert_eq!(sent[0]["stamp"], json!(golden_stamp()));
    assert_eq!(sent[0]["name"], json!("key"));
}

#[test]
fn the_secret_use_client_reports_an_approval_as_pending() {
    struct Pending;
    impl SecretUseTransport for Pending {
        fn secret_use(&self, _params: Value) -> Result<Value, String> {
            let file = vectors();
            Ok(json!({"result":{
                "status": "approval_required",
                "request_id": file["vectors"][0]["statements"][0]["request_id"],
                "statement_b64url": file["vectors"][0]["statements"][0]["statement_b64url"],
                "expires_at_ms": 1700000600000u64,
            }}))
        }
    }
    let vault = SubcSecretVault::new(std::sync::Arc::new(Pending));
    let scope = SecretScope::from_stamp(&golden_stamp());
    let Resolution::ApprovalRequired(request) = vault.resolve(&scope, "key") else {
        panic!("an approval_required answer is a pending resolution");
    };
    assert_eq!(request.request_id, "01HF7YAT000000000000000001");
    assert!(!request.statement_b64url.is_empty());
}

#[test]
fn the_secret_use_client_maps_the_uniform_not_found_to_a_refusal() {
    struct NotFound;
    impl SecretUseTransport for NotFound {
        fn secret_use(&self, _params: Value) -> Result<Value, String> {
            Ok(json!({"result":{"error":{"code":"not_found","class":"permanent"}}}))
        }
    }
    let vault = SubcSecretVault::new(std::sync::Arc::new(NotFound));
    let scope = SecretScope::from_stamp(&golden_stamp());
    assert!(matches!(
        vault.resolve(&scope, "key"),
        Resolution::Denied { .. }
    ));
}

#[test]
fn a_deposit_location_token_reads_as_the_rules_location() {
    // The deposit's four location spellings, as §15 lists them.
    assert_eq!(
        secret_rules::location::parse_vault_location("authorization_bearer"),
        Some(secret_rules::location::Location::Bearer)
    );
    assert_eq!(
        secret_rules::location::parse_vault_location("header:X-Api-Key"),
        Some(secret_rules::location::Location::Header("X-Api-Key".into()))
    );
    assert_eq!(
        secret_rules::location::parse_vault_location("query:token"),
        Some(secret_rules::location::Location::Query("token".into()))
    );
    assert_eq!(
        secret_rules::location::parse_vault_location("json_pointer:/a/b"),
        Some(secret_rules::location::Location::Json("/a/b".into()))
    );
    assert_eq!(
        secret_rules::location::parse_vault_location("header:"),
        None
    );
    assert_eq!(
        secret_rules::location::parse_vault_location("cookie:x"),
        None
    );
}
