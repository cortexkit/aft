//! The secret vault seam and its consent adapter.
//!
//! [`SecretVault`] asks for material using each outgoing request's real target.
//! [`wire`] holds the `secret.use` contract's request and reply shapes (v1 §15);
//! Standalone processes use [`UnavailableVault`]. The fake
//! lives behind `cfg(test)` or the `secret-test-fake` feature, which no release
//! profile enables.
//!
//! An eligible call's two scopes ([`eligibility::CallScopes`]) are stamped by
//! the daemon: the authorization scope reaches `use_secret`, the
//! consent ask and the audit record, while the lifetime scope decides when a
//! proxy and its matchers stop.

pub mod consent;
pub mod eligibility;
pub mod outcome;
#[cfg(unix)]
pub(crate) mod requests;
pub mod scope;
pub mod transport;
pub mod vault;
pub mod wire;

pub use consent::{
    file_consent, request_params, withhold_consent, ConsentOutcome, SecretConsentRequest,
    SecretConsentTransport, SECRET_CONSENT_KIND,
};
pub use eligibility::{CallScopes, EligibilityForm, Ineligibility};
pub use outcome::{
    first_failure, outcome_for, outcome_for_second_use, refuse, refuse_ineligible, LabelOutcome,
    SECRETS_UNAVAILABLE, SECRET_ACCESS_PENDING, SECRET_ACCESS_PENDING_MESSAGE,
    SECRET_ACCESS_UNAVAILABLE, SECRET_REFUSED,
};
pub use scope::SecretScope;
pub use vault::{
    now_ms, unrecorded_audit_records, zeroize, AuditError, Material, SecretUseTransport,
    SecretVault, SubcSecretVault, UnavailableVault, UseAnswer,
};
pub use wire::{
    parse_answer, parse_reply, ApiKeyTarget, ApprovalRequest, Coverage, LoginTarget,
    ReleasedApiKey, ReleasedLogin, ReplyError, SecretUseAnswer, SecretUseRefusal, SecretUseReply,
    SecretUseRequest, Target, VaultError, VaultTransportError,
};

#[cfg(any(test, feature = "secret-test-fake"))]
pub(crate) use vault::fake::{approval, granted, FakeReply, FakeVault, FakeVaultScript};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
