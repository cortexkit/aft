//! Seam tests: the production vault, the outcome table, the material
//! buffer, the fake, and the consent adapter's request shape.

use super::*;
use secret_rules::audit::{AuditReason, AuditRecordV1, Decision};
use secret_rules::location::Location;

fn production_scope() -> SecretScope {
    SecretScope::from_stamp(
        &serde_json::from_value(serde_json::json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": "head",
            "scope_epoch": 7,
            "kind": "head",
            "owner_authorized": true,
        }))
        .unwrap(),
    )
}

fn rule(label: &str) -> secret_rules::location::LabelRule {
    secret_rules::location::LabelRule {
        label: label.into(),
        host: "api.example.com".into(),
        port: 443,
        path_prefix: "/v1".into(),
        location: Location::Bearer,
    }
}

// ---------------------------------------------------------------------------
// Production stays fail-closed.
// ---------------------------------------------------------------------------

#[test]
fn standalone_use_is_unavailable_for_every_label_and_scope() {
    for label in ["prod", "test", "repo"] {
        assert!(matches!(
            UnavailableVault.use_secret(&production_scope(), label, &target()),
            UseAnswer::Unavailable
        ));
    }
}

fn target() -> ApiKeyTarget {
    ApiKeyTarget {
        scheme: "https".into(),
        host: "api.example.com".into(),
        port: Some(443),
        path: "/v1".into(),
    }
}

#[test]
fn an_unavailable_resolution_ends_the_launch_with_secrets_unavailable() {
    let outcome = outcome_for("prod", &UseAnswer::Unavailable);
    assert_eq!(outcome.code(), Some(SECRETS_UNAVAILABLE));
    assert!(!outcome.proceeds());
    // The message names the label it refused and never a key.
    let LabelOutcome::Refuse { message, .. } = &outcome else {
        panic!("an unavailable resolution refuses the launch");
    };
    assert!(message.contains("prod"), "{message}");
}

// ---------------------------------------------------------------------------
// The outcome table.
// ---------------------------------------------------------------------------

#[test]
fn a_released_request_proceeds() {
    let answer = UseAnswer::Released(ReleasedApiKey::new(
        "authorization_bearer".into(),
        Material::new(b"test-secret".to_vec()),
    ));
    assert_eq!(outcome_for("repo", &answer), LabelOutcome::Proceed);
}

#[test]
fn an_approval_required_resolution_files_a_consent_and_holds_the_call() {
    let outcome = outcome_for("repo", &UseAnswer::ApprovalRequired(approval("req-1")));
    let LabelOutcome::FileConsent {
        request_id,
        statement_b64url,
    } = &outcome
    else {
        panic!("an approval request files a consent");
    };
    // The ask carries the vault's identity, not AFT's.
    assert_eq!(request_id, "req-1");
    assert_eq!(statement_b64url, "c3RhdGVtZW50");
    assert_eq!(outcome.code(), Some(SECRET_ACCESS_PENDING));
    assert!(!outcome.proceeds());
}

#[test]
fn uniform_not_found_keeps_the_literal_request() {
    assert_eq!(
        outcome_for("repo", &UseAnswer::NotFound),
        LabelOutcome::Proceed
    );
}

#[test]
fn a_phone_answer_never_grants_material_without_a_vault_release() {
    assert_eq!(
        outcome_for_second_use("repo", &UseAnswer::ApprovalRequired(approval("req-1"))).code(),
        Some(SECRETS_UNAVAILABLE)
    );
    assert!(outcome_for_second_use("repo", &UseAnswer::NotFound).proceeds());
    assert_eq!(
        outcome_for_second_use("repo", &UseAnswer::Unavailable).code(),
        Some(SECRETS_UNAVAILABLE)
    );
}

#[test]
fn an_ineligible_scope_refuses_with_secret_refused_and_names_the_label() {
    let outcome = refuse_ineligible("repo", Ineligibility::OtherOwner);
    assert_eq!(outcome.code(), Some(SECRET_REFUSED));
    let LabelOutcome::Refuse { message, .. } = outcome else {
        panic!("ineligible scopes refuse");
    };
    assert!(message.contains("repo"), "{message}");
}

#[test]
fn the_first_failing_label_decides_a_multi_label_launch() {
    let outcomes = vec![
        LabelOutcome::Proceed,
        LabelOutcome::FileConsent {
            request_id: "req-1".into(),
            statement_b64url: "c3RhdGVtZW50".into(),
        },
        refuse(SECRET_REFUSED, "third", "denied"),
    ];
    let failure = first_failure(&outcomes).expect("a pending label stops the launch");
    assert_eq!(failure.code(), Some(SECRET_ACCESS_PENDING));
    assert!(first_failure(&[LabelOutcome::Proceed, LabelOutcome::Proceed]).is_none());
}

#[test]
fn a_new_request_may_ask_for_a_fresh_approval() {
    assert!(matches!(
        outcome_for(
            "repo",
            &UseAnswer::ApprovalRequired(approval("next-request"))
        ),
        LabelOutcome::FileConsent { .. }
    ));
}

// ---------------------------------------------------------------------------
// The fake and the material buffer.
// ---------------------------------------------------------------------------

#[test]
fn the_fake_counts_addressed_uses_and_rotates_material() {
    let mut script = FakeVaultScript::default();
    script.answers.insert("repo".into(), granted(rule("repo")));
    script.material.insert(
        "repo".into(),
        vec![
            b"first-key-material-1".to_vec(),
            b"second-key-material".to_vec(),
        ],
    );
    let vault = FakeVault::with_script(script);
    let scope = production_scope();
    let UseAnswer::Released(first) = vault.use_secret(&scope, "repo", &target()) else {
        panic!("release");
    };
    let UseAnswer::Released(second) = vault.use_secret(&scope, "repo", &target()) else {
        panic!("release");
    };
    assert_eq!(first.material(), b"first-key-material-1");
    assert_eq!(second.material(), b"second-key-material");
    assert_eq!(vault.use_calls(), 2);
    assert_eq!(vault.material_calls(), 2);
    assert_eq!(vault.materials(), vec!["repo", "repo"]);
    let mut other = target();
    other.host = "other.example.com".into();
    assert!(matches!(
        vault.use_secret(&scope, "repo", &other),
        UseAnswer::NotFound
    ));
    assert_eq!(vault.material_calls(), 2);
}

#[test]
fn material_prints_its_length_and_never_its_bytes() {
    let material = Material::new(b"a-very-secret-key-value".to_vec());
    let rendered = format!("{material:?}");
    assert!(rendered.contains("len"), "{rendered}");
    assert!(!rendered.contains("secret"), "{rendered}");
}

// ---------------------------------------------------------------------------
// The fake collects audit records.
// ---------------------------------------------------------------------------

#[test]
fn the_fake_collects_audit_records_in_order() {
    let vault = FakeVault::new();
    let scope = production_scope();
    let record = AuditRecordV1::new(
        1_700_000_000_000,
        "repo",
        Decision::Substituted,
        AuditReason::Matched,
        "req-1",
        "task-9",
    )
    .with_scope(Some(scope.to_scope_ref()));
    vault.audit(&record).unwrap();
    let collected = vault.audit_records();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].label, "repo");
    assert_eq!(collected[0].scope.as_ref().unwrap().kind, "head");
    assert_eq!(
        vault.audit_summary(),
        vec![("repo".into(), Decision::Substituted)]
    );
    assert_eq!(vault.audit_calls(), 1);
}

// ---------------------------------------------------------------------------
// The consent adapter's request shape.
// ---------------------------------------------------------------------------

#[test]
fn a_secret_consent_files_exactly_the_single_phone_item() {
    use sha2::{Digest, Sha256};
    let vector = include_str!("../../tests/fixtures/secret/single-phone-get.json");
    assert_eq!(
        format!("{:x}", Sha256::digest(vector.as_bytes())),
        "81794c4c98c77fb57813d8cb4c396c52da17bab4c1e8e7f56c113f4a521cb4a8"
    );
    let vector: serde_json::Value = serde_json::from_str(vector).unwrap();
    let item = &vector["result"]["secretUse"]["items"][0];
    let request_id = item["request_id"].as_str().unwrap();
    let statement = item["statement_b64url"].as_str().unwrap();
    let request = SecretConsentRequest {
        scope: production_scope(),
        project_root: "project".into(),
        session: "session".into(),
        label: "repo".into(),
        request_id: request_id.into(),
        statement_b64url: statement.into(),
    };
    let expected = format!(
        r#"{{"kind":"secret_use","secret_use":{{"items":[{{"request_id":"{request_id}","statement_b64url":"{statement}"}}]}}}}"#
    );
    assert_eq!(
        serde_json::to_string(&request_params(&request)).unwrap(),
        expected
    );
    let rendered = format!("{request:?}");
    assert!(rendered.contains(request_id), "{rendered}");
    assert!(!rendered.contains(statement), "{rendered}");
}
