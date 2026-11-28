use super::fake::golden;
use super::*;

#[allow(unused_imports)]
pub(crate) use aft_base::secret::transport::tests::*;

#[test]
fn secret_use_standalone_cannot_construct_a_module_transport_and_keeps_unavailable_vault() {
    let _nonce = crate::launch_nonce::install_for_tests(None);
    assert!(SubcSecretUseTransport::new(
        PathBuf::from("absent-connection.json"),
        BindIdentity::new("standalone", "standalone"),
        Duration::from_millis(150),
    )
    .is_none());
    let stamp: ScopeStamp = serde_json::from_value(golden(0)["stamp"].clone()).unwrap();
    let vault = crate::bash_background::secrets::vault();
    assert!(matches!(
        vault.use_secret(
            &crate::secret::SecretScope::from_stamp(&stamp),
            "key",
            &crate::secret::ApiKeyTarget {
                scheme: "https".into(),
                host: "x.test".into(),
                port: Some(443),
                path: "/repos/a".into()
            }
        ),
        crate::secret::UseAnswer::Unavailable
    ));
}
