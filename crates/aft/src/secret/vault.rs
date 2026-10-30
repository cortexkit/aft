//! The vault seam: the one way AFT asks for a label's rule and key material.
//!
//! AFT never reads a key itself. The vault maps a label to a rule in a
//! `secret.use` reply ([`crate::secret::wire`]), hands over the bytes for one
//! request or fetch hop, and collects one audit record per decided label.
//!
//! `material` exists because a grant is a permission, not a promise of the same
//! bytes: the vault releases material per request, and AFT must not cache it.
//! Key bytes live in a zeroizing buffer whose `Debug` prints a length, so no log
//! line or panic message carries a key.
//!
//! The live vault is not deployed and AFT holds no `secret.use` grant yet, so
//! production wires [`UnavailableVault`], which resolves nothing and releases
//! nothing. [`SubcSecretVault`] is the real client, built and tested
//! against the contract's golden examples; it becomes the production stand-in's
//! replacement once the daemon can carry the call. The fake for tests compiles
//! only under `cfg(test)` or the `secret-test-fake` feature, which no release
//! profile enables (`scripts/check-secret-test-fake.sh` checks that in CI).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use secret_rules::audit::AuditRecordV1;
use secret_rules::location::LabelRule;

use super::wire::{
    self, ApprovalRequest, SecretUseAnswer, SecretUseReply, SecretUseRequest, VaultError,
};

/// Key bytes for one request or fetch hop.
///
/// Held in a buffer that is wiped when it drops and never reused across
/// requests (a grant may release different bytes after a rotation). `Debug`
/// prints the length only.
pub struct Material(Vec<u8>);

impl Material {
    /// Take ownership of key bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The key bytes.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The key's length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the material is empty, which no admissible key is.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Material {
    fn drop(&mut self) {
        zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for Material {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Material")
            .field("len", &self.0.len())
            .finish()
    }
}

/// Overwrite a byte buffer with zeros, in a way the compiler may not drop as
/// dead. AFT reaches this through material buffers, whose contents must not
/// outlive their one request.
pub fn zeroize(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, aligned pointer to one initialized `u8`,
        // and a volatile store is the only way to keep the wipe from being
        // optimized away for a buffer that is about to be freed.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// What the vault says about one label.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resolution {
    /// A rule to follow and the grant that allows material for it.
    Granted {
        /// Where and how the placeholder may be replaced.
        rule: LabelRule,
        /// Opaque grant identifier passed back to `material`.
        grant_id: String,
        /// When the grant lapses, in milliseconds since the epoch.
        expires_at_ms: u64,
    },
    /// The vault is waiting for the user's answer.
    ApprovalRequired(ApprovalRequest),
    /// The vault refused the label.
    Denied {
        /// The vault's reason, plain text with no key material.
        reason: String,
    },
    /// No vault answers. Production answers this until the vault ships and AFT
    /// holds a grant, which keeps the seam fail-closed.
    Unavailable,
}

/// Why `material` handed out no bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MaterialError {
    /// The grant lapsed: its expiry passed, it was revoked, or its scope ended.
    GrantLapsed,
    /// The vault could not be reached, or answered transiently.
    Unavailable {
        /// Plain-text detail with no key material.
        detail: String,
    },
    /// The vault refused to release material for the grant.
    Refused {
        /// The vault's reason, plain text with no key material.
        reason: String,
    },
}

/// Why `audit` did not take a record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AuditError {
    /// The record failed its key or scope check; the detail names the field,
    /// never a value.
    Invalid {
        /// Which check failed, in plain text.
        detail: String,
    },
    /// The sink could not be reached.
    Unavailable {
        /// Plain-text detail.
        detail: String,
    },
}

/// The three calls AFT makes on the vault.
///
/// Both scopes are [`crate::secret::SecretScope`] values the daemon stamped,
/// never tool arguments or session ids.
pub trait SecretVault: Send + Sync {
    /// Ask which rule a label follows, without asking for the key bytes.
    fn resolve(&self, scope: &crate::secret::SecretScope, label: &str) -> Resolution;

    /// Fetch the bytes for one request or fetch hop whose target matched the
    /// rule that `grant_id` belongs to. Called once per matching request; the
    /// same grant may release different bytes after a rotation.
    fn material(
        &self,
        scope: &crate::secret::SecretScope,
        label: &str,
        grant_id: &str,
    ) -> Result<Material, MaterialError>;

    /// Hand one record to the vault's audit sink.
    fn audit(&self, record: &AuditRecordV1) -> Result<(), AuditError>;

    /// Whether this vault can ever hand key bytes to AFT. A launch hardens the
    /// process (no core dumps; non-dumpable on Linux) before talking to a vault
    /// that can. The hardening is irreversible and on Linux costs the daemon
    /// its own /proc counters, so a vault that never releases material skips it.
    fn can_release_material(&self) -> bool {
        true
    }
}

/// The vault production uses while the live vault is undeployed and AFT holds
/// no `secret.use` grant.
///
/// Every resolution is `Unavailable` and `material` refuses, so nothing is ever
/// substituted without a real grant, whatever the configuration says. No config
/// file, environment variable or project setting selects another production
/// vault, so this cannot be switched on by mistake.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableVault;

impl SecretVault for UnavailableVault {
    fn resolve(&self, _scope: &crate::secret::SecretScope, _label: &str) -> Resolution {
        Resolution::Unavailable
    }

    fn material(
        &self,
        _scope: &crate::secret::SecretScope,
        _label: &str,
        _grant_id: &str,
    ) -> Result<Material, MaterialError> {
        Err(MaterialError::Unavailable {
            detail: "no vault is connected".into(),
        })
    }

    fn audit(&self, _record: &AuditRecordV1) -> Result<(), AuditError> {
        // The record's own checks already ran; there is simply nowhere to send
        // it until the vault's shape is live. Count it rather than drop it.
        UNRECORDED_AUDIT_RECORDS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn can_release_material(&self) -> bool {
        false
    }
}

static UNRECORDED_AUDIT_RECORDS: AtomicU64 = AtomicU64::new(0);

/// How many audit records the production vault accepted with no sink to write
/// them to, so a future sink can report the gap instead of losing it.
pub fn unrecorded_audit_records() -> u64 {
    UNRECORDED_AUDIT_RECORDS.load(Ordering::Relaxed)
}

/// Milliseconds since the Unix epoch, for a record's `timestamp_ms`.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// The transport one `secret.use` call travels on.
///
/// Production opens a scoped management-surface route to the vault module and
/// carries the request envelope; tests install a fake. The request is the
/// contract's `params`, so a transport never has to know the grammar.
pub trait SecretUseTransport: Send + Sync {
    /// Send `params` as a `secret.use` call and return the reply body: either
    /// `{"result": …}` or an error object (§15, §1a).
    fn secret_use(&self, params: serde_json::Value) -> Result<serde_json::Value, String>;
}

/// The real client: `secret.use` over the daemon's transport (§15).
///
/// AFT is one of only two senders the vault admits, and it receives only API
/// keys. `resolve` reads the reply's rule without keeping any bytes the vault
/// may have released; a released key at resolution time is zeroized
/// immediately, exactly as the contract's material rule requires.
pub struct SubcSecretVault {
    transport: std::sync::Arc<dyn SecretUseTransport>,
}

impl std::fmt::Debug for SubcSecretVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubcSecretVault").finish_non_exhaustive()
    }
}

impl SubcSecretVault {
    /// Build the client over `transport`.
    pub fn new(transport: std::sync::Arc<dyn SecretUseTransport>) -> Self {
        Self { transport }
    }

    /// Send one `secret.use` call for `label` and read the answer.
    fn call(
        &self,
        scope: &crate::secret::SecretScope,
        label: &str,
        target: wire::Target,
    ) -> Result<SecretUseAnswer, wire::ReplyError> {
        let request = SecretUseRequest::new(label, scope.stamp(), target);
        let body = self
            .transport
            .secret_use(request.to_params())
            .map_err(|detail| wire::ReplyError::Malformed { detail })?;
        wire::parse_answer(&body)
    }
}

impl SecretVault for SubcSecretVault {
    fn resolve(&self, scope: &crate::secret::SecretScope, label: &str) -> Resolution {
        // A resolution asks about every path, so it names the root path: the
        // contract matches the target against the record's sealed scope, and a
        // released key here is dropped, never kept.
        let target = scope.root_target();
        match self.call(scope, label, target) {
            Ok(SecretUseAnswer::Reply(SecretUseReply::ReleasedApiKey(key))) => {
                let rule = scope.rule_for(label, &key.location);
                // The released bytes are not what this call asked for; drop them
                // so they are zeroized here rather than retained.
                drop(key);
                Resolution::Granted {
                    rule,
                    grant_id: String::new(),
                    expires_at_ms: 0,
                }
            }
            Ok(SecretUseAnswer::Reply(SecretUseReply::ReleasedLogin(_))) => Resolution::Denied {
                reason: "the vault released a login to a sender that only takes API keys".into(),
            },
            Ok(SecretUseAnswer::Reply(SecretUseReply::ApprovalRequired(request))) => {
                Resolution::ApprovalRequired(request)
            }
            Ok(SecretUseAnswer::Refused(error)) => resolution_for_error(&error),
            Ok(SecretUseAnswer::Rejected(error)) => {
                if error.code == "invalid_params" {
                    Resolution::Denied {
                        reason: format!("the vault refused the request: {}", error.code),
                    }
                } else {
                    Resolution::Unavailable
                }
            }
            Err(_) => Resolution::Unavailable,
        }
    }

    fn material(
        &self,
        scope: &crate::secret::SecretScope,
        label: &str,
        _grant_id: &str,
    ) -> Result<Material, MaterialError> {
        let target = scope.root_target();
        match self.call(scope, label, target) {
            Ok(SecretUseAnswer::Reply(SecretUseReply::ReleasedApiKey(key))) => {
                Ok(Material::new(key.material().to_vec()))
            }
            Ok(SecretUseAnswer::Reply(SecretUseReply::ReleasedLogin(_))) => {
                Err(MaterialError::Refused {
                    reason: "the vault released a login, not an API key".into(),
                })
            }
            Ok(SecretUseAnswer::Reply(SecretUseReply::ApprovalRequired(_))) => {
                Err(MaterialError::GrantLapsed)
            }
            Ok(SecretUseAnswer::Refused(error)) => Err(error_for_material(&error)),
            Ok(SecretUseAnswer::Rejected(_)) | Err(_) => Err(MaterialError::Unavailable {
                detail: "the vault did not answer".into(),
            }),
        }
    }

    fn audit(&self, _record: &AuditRecordV1) -> Result<(), AuditError> {
        // The contract has no AFT-facing sink yet; the production stand-in's
        // counter is the honest record until one lands.
        UNRECORDED_AUDIT_RECORDS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Map a result error onto a resolution (the outcome table).
fn resolution_for_error(error: &VaultError) -> Resolution {
    match error.code.as_str() {
        "not_found" | "invalid_params" => Resolution::Denied {
            reason: "the vault does not know this secret or target".into(),
        },
        _ => Resolution::Unavailable,
    }
}

/// Map a result error onto a material refusal.
fn error_for_material(error: &VaultError) -> MaterialError {
    match error.code.as_str() {
        "not_found" => MaterialError::GrantLapsed,
        code => MaterialError::Refused {
            reason: format!("the vault refused to release the secret: {code}"),
        },
    }
}

/// The fake vault, compiled only for tests or an explicitly feature-enabled
/// build.
#[cfg(any(test, feature = "secret-test-fake"))]
pub(crate) mod fake;
