pub use aft_base::url_fetch::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::lang_id::language_by_extension;
#[cfg(test)]
#[allow(unused_imports)]
use htmd::{
    element_handler::{HandlerResult, Handlers},
    Element, HtmlToMarkdown,
};
#[cfg(test)]
#[allow(unused_imports)]
use markup5ever_rcdom::{Node, NodeData};
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::blocking::{Client, Response as HttpResponse};
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, LOCATION, USER_AGENT,
};
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::redirect::Policy;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::audit::{AuditReason, AuditRecordV1, Decision};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::{
    evaluate_request_with_connect, location_kind_token, LabelOutcome, LabelRule, Location,
    RequestView,
};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::material::{check_admissible, encode_for_location};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::{MatcherSet, ScrubError, Scrubber};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::target::{normalize_request_target, ConnectAuthority};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use sha2::{Digest, Sha256};
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
use std::io::{self, Read, Write};
#[cfg(test)]
#[allow(unused_imports)]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::rc::Rc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{mpsc, Arc};
#[cfg(test)]
#[allow(unused_imports)]
use std::thread;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(test)]
#[allow(unused_imports)]
use url::Url;
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(test)]
mod tests {

    use super::*;
    use url::Url;
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
/// The fetch times the slow-call line reports, measured against local
/// servers that are slow in a known phase.
#[path = "url_fetch_deadline_tests.rs"]
mod deadline_tests;

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

#[cfg(test)]
#[path = "url_fetch_sensitive.rs"]
mod sensitive;
