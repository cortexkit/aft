use crate::url_fetch::{secret_hop, HeldKeys, SecretFetch, SECRET_URL_BUILDS};
use secret_rules::audit::AuditRecordV1;
use std::sync::Arc;
use url::Url;
#[test]
fn failed_audit_does_not_create_an_ordinary_key_bearing_url() {
    struct NoAudit;
    impl crate::secret::SecretVault for NoAudit {
        fn use_secret(
            &self,
            _: &crate::secret::SecretScope,
            _: &str,
            _: &crate::secret::ApiKeyTarget,
        ) -> crate::secret::UseAnswer {
            crate::secret::UseAnswer::Released(crate::secret::ReleasedApiKey::new(
                "query:key".into(),
                crate::secret::Material::new(b"released-test-key-123456".to_vec()),
            ))
        }
        fn audit(&self, _: &AuditRecordV1) -> Result<(), crate::secret::AuditError> {
            Err(crate::secret::AuditError::Unavailable {
                detail: "offline".into(),
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let stamp: subc_protocol::scope::ScopeStamp = serde_json::from_value(serde_json::json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"audit-url","kind":"head","scope_epoch":1,"owner_authorized":true})).unwrap();
    let scope = crate::secret::SecretScope::from_stamp(&stamp);
    SECRET_URL_BUILDS.with(|n| n.set(0));
    crate::subc::with_secret_identity_for_test(root.path(), stamp, || {
        let secrets = SecretFetch {
            vault: Arc::new(NoAudit),
            authorization: Some(scope),
            labels: vec!["test".into()],
            test_https_port: None,
            call_id: "audit-url".into(),
            cancellation: Default::default(),
        };
        let mut held = HeldKeys::default();
        assert!(secret_hop(
            &Url::parse("https://example.test/doc.md?key={{ck-secret:test}}").unwrap(),
            0,
            &secrets,
            &mut held
        )
        .is_err());
    });
    assert_eq!(
        SECRET_URL_BUILDS.with(std::cell::Cell::get),
        0,
        "audit refusal must precede construction of any Url that owns a key"
    );
}
