//! The secret vault seam and its consent adapter.
//!
//! [`SecretVault`] is the only way AFT asks for a label's rule or key bytes:
//! `resolve` says whether and where a label may be used, `material` releases the
//! bytes for one request, and `audit` collects one record per decided label.
//! [`wire`] holds the `secret.use` contract's request and reply shapes (v1 §15);
//! production wires [`UnavailableVault`], which resolves nothing because the
//! live vault is not deployed and AFT holds no use grant yet. The fake
//! lives behind `cfg(test)` or the `secret-test-fake` feature, which no release
//! profile enables.
//!
//! An eligible call's two scopes ([`eligibility::CallScopes`]) are stamped by
//! the daemon: the authorization scope reaches `resolve`, `material`, the
//! consent ask and the audit record, while the lifetime scope decides when a
//! proxy and its matchers stop.

pub mod consent;
pub mod eligibility;
pub mod outcome;
pub mod scope;
pub mod vault;
pub mod wire;

pub use consent::{
    file_consent, request_params, withhold_consent, ConsentOutcome, SecretConsentRequest,
    SecretConsentTransport, SECRET_CONSENT_KIND,
};
pub use eligibility::{CallScopes, EligibilityForm, Ineligibility};
pub use outcome::{
    first_failure, outcome_for, outcome_for_material, outcome_for_second_resolve, refuse,
    refuse_ineligible, LabelOutcome, SECRETS_UNAVAILABLE, SECRET_ACCESS_PENDING,
    SECRET_ACCESS_PENDING_MESSAGE, SECRET_ACCESS_UNAVAILABLE, SECRET_REFUSED,
};
pub use scope::SecretScope;
pub use vault::{
    now_ms, unrecorded_audit_records, zeroize, AuditError, Material, MaterialError, Resolution,
    SecretUseTransport, SecretVault, SubcSecretVault, UnavailableVault,
};
pub use wire::{
    parse_answer, parse_reply, ApiKeyTarget, ApprovalRequest, Coverage, LoginTarget,
    ReleasedApiKey, ReleasedLogin, ReplyError, SecretUseAnswer, SecretUseReply, SecretUseRequest,
    Target, VaultError, VaultTransportError,
};

#[cfg(any(test, feature = "secret-test-fake"))]
pub(crate) use vault::fake::{approval, granted, FakeVault, FakeVaultScript};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
