pub use aft_base::url_fetch::*;

#[cfg(test)]
#[allow(unused_imports)]
use htmd::element_handler::HandlerResult;
#[cfg(test)]
#[allow(unused_imports)]
use htmd::element_handler::Handlers;
#[cfg(test)]
#[allow(unused_imports)]
use htmd::Element;
#[cfg(test)]
#[allow(unused_imports)]
use htmd::HtmlToMarkdown;
#[cfg(test)]
#[allow(unused_imports)]
use markup5ever_rcdom::Node;
#[cfg(test)]
#[allow(unused_imports)]
use markup5ever_rcdom::NodeData;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::blocking::Client;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::blocking::Response as HttpResponse;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::ACCEPT;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::ACCEPT_ENCODING;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::CONTENT_ENCODING;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::CONTENT_TYPE;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::LOCATION;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::USER_AGENT;
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::redirect::Policy;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::audit::AuditReason;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::audit::AuditRecordV1;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::audit::Decision;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::evaluate_request_with_connect;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::location_kind_token;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::LabelOutcome;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::LabelRule;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::Location;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::RequestView;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::material::check_admissible;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::material::encode_for_location;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::MatcherSet;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::ScrubError;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::Scrubber;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::target::normalize_request_target;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::target::ConnectAuthority;
#[cfg(test)]
#[allow(unused_imports)]
use serde::Deserialize;
#[cfg(test)]
#[allow(unused_imports)]
use serde::Serialize;
#[cfg(test)]
#[allow(unused_imports)]
use sha2::Digest;
#[cfg(test)]
#[allow(unused_imports)]
use sha2::Sha256;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::error::Error;
#[cfg(test)]
#[allow(unused_imports)]
use std::fmt;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs;
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Read;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::Write;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::IpAddr;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::Ipv4Addr;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::Ipv6Addr;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::SocketAddr;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::ToSocketAddrs;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::rc::Rc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::mpsc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::thread;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Instant;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::SystemTime;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::UNIX_EPOCH;
#[cfg(test)]
#[allow(unused_imports)]
use url::Url;
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(test)]
mod tests {
    #[test]
    fn extension_only_url_decision_matches_parser() {
        for ext in crate::lang_id::TEST_EXTENSIONS {
            let probe = std::path::Path::new("file").with_extension(ext);
            let expected = if crate::parser::detect_language(&probe).is_some() {
                super::static_extension_for_lang_ext(ext)
            } else {
                None
            };
            let url = url::Url::parse(&format!("https://example.com/file.{ext}")).unwrap();
            assert_eq!(super::extension_from_url_path(&url), expected, "{ext}");
        }
        for name in ["Jenkinsfile", "file", ".hidden"] {
            let url = url::Url::parse(&format!("https://example.com/{name}")).unwrap();
            assert_eq!(super::extension_from_url_path(&url), None, "{name}");
        }
    }
}

#[cfg(test)]
mod secret_audit_regressions {
    use super::*;
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
}
