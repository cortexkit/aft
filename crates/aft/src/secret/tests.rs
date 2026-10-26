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
        grant_id: "grant-1".into(),
    }
}

// ---------------------------------------------------------------------------
// Production stays fail-closed.
// ---------------------------------------------------------------------------

#[test]
fn production_resolve_is_unavailable_for_every_label_and_scope() {
    let vault = UnavailableVault;
    let scope = production_scope();
    for label in ["prod", "test", "repo"] {
        assert_eq!(vault.resolve(&scope, label), Resolution::Unavailable);
    }
    assert!(matches!(
        vault.material(&scope, "prod", "grant-1"),
        Err(MaterialError::Unavailable { .. })
    ));
}

#[test]
fn an_unavailable_resolution_ends_the_launch_with_secrets_unavailable() {
    let outcome = outcome_for("prod", &Resolution::Unavailable);
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
fn a_granted_resolution_proceeds() {
    let outcome = outcome_for("repo", &granted(rule("repo"), "grant-1"));
    assert_eq!(outcome, LabelOutcome::Proceed);
    assert_eq!(outcome.code(), None);
}

#[test]
fn an_approval_required_resolution_files_a_consent_and_holds_the_call() {
    let outcome = outcome_for("repo", &Resolution::ApprovalRequired(approval("req-1")));
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
fn a_denied_resolution_refuses_naming_the_label() {
    let outcome = outcome_for(
        "repo",
        &Resolution::Denied {
            reason: "this device may not use repo".into(),
        },
    );
    assert_eq!(outcome.code(), Some(SECRET_REFUSED));
    let LabelOutcome::Refuse { message, .. } = &outcome else {
        panic!("a denial refuses the launch");
    };
    assert!(message.contains("repo"), "{message}");
    assert!(
        message.contains("this device may not use repo"),
        "{message}"
    );
}

#[test]
fn a_second_resolve_is_the_only_thing_that_can_grant() {
    // Cingulate's answer alone never grants: a resolve that still asks again
    // substitutes nothing, while a grant after the answer proceeds and a
    // denial refuses.
    assert_eq!(
        outcome_for_second_resolve("repo", &Resolution::ApprovalRequired(approval("req-1"))).code(),
        Some(SECRETS_UNAVAILABLE)
    );
    assert!(outcome_for_second_resolve("repo", &granted(rule("repo"), "grant-1")).proceeds());
    assert_eq!(
        outcome_for_second_resolve(
            "repo",
            &Resolution::Denied {
                reason: "no".into()
            }
        )
        .code(),
        Some(SECRET_REFUSED)
    );
    assert_eq!(
        outcome_for_second_resolve("repo", &Resolution::Unavailable).code(),
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
fn a_material_lapse_refuses_without_filing_a_consent() {
    let outcome = outcome_for_material("repo", &MaterialError::GrantLapsed);
    assert_eq!(outcome.code(), Some(SECRET_REFUSED));
    // A lapse never asks for a new consent: the proxy is already live.
    assert!(!matches!(outcome, LabelOutcome::FileConsent { .. }));
}

// ---------------------------------------------------------------------------
// The fake and the material buffer.
// ---------------------------------------------------------------------------

#[test]
fn the_fake_counts_resolve_and_material_calls_and_rotates_material() {
    let mut script = FakeVaultScript::default();
    script
        .resolutions
        .insert("repo".into(), granted(rule("repo"), "grant-1"));
    script.material.insert(
        "repo".into(),
        vec![
            b"first-key-material-1".to_vec(),
            b"second-key-material".to_vec(),
        ],
    );
    let vault = FakeVault::with_script(script);
    let scope = production_scope();
    assert!(matches!(
        vault.resolve(&scope, "repo"),
        Resolution::Granted { .. }
    ));
    let first = vault.material(&scope, "repo", "grant-1").unwrap();
    let second = vault.material(&scope, "repo", "grant-1").unwrap();
    // One grant, two different values: the seam never assumes a grant returns
    // the same bytes.
    assert_eq!(first.expose(), b"first-key-material-1");
    assert_eq!(second.expose(), b"second-key-material");
    assert_eq!(vault.resolve_calls(), 1);
    assert_eq!(vault.material_calls(), 2);
    assert_eq!(
        vault.materials(),
        vec!["repo".to_string(), "repo".to_string()]
    );
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
fn a_secret_consent_carries_the_vaults_request_and_no_answer() {
    let request = SecretConsentRequest {
        scope: production_scope(),
        label: "repo".into(),
        request_id: "req-9".into(),
        statement_b64url: "c3RhdGVtZW50".into(),
    };
    let params = request_params(&request);
    assert_eq!(params["kind"], serde_json::json!(SECRET_CONSENT_KIND));
    assert_eq!(params["label"], serde_json::json!("repo"));
    assert_eq!(params["request_id"], serde_json::json!("req-9"));
    assert_eq!(
        params["statement_b64url"],
        serde_json::json!("c3RhdGVtZW50")
    );
    // A secret ask borrows nothing from the filesystem grant shape, and it
    // carries no answer: only Cingulate's record decides.
    assert!(params.get("paths").is_none());
    assert!(params.get("call_key").is_none());
    assert!(params.get("args_hash").is_none());
    assert!(params.get("outcome").is_none());
    let rendered = format!("{request:?}");
    assert!(rendered.contains("req-9"), "{rendered}");
}
