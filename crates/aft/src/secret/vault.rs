//! Request-addressed vault access. Every outgoing use names its real target;
//! material is zeroized after that request and is never cached between uses.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use secret_rules::audit::AuditRecordV1;

use super::wire::{
    self, ApiKeyTarget, ApprovalRequest, ReleasedApiKey, SecretUseAnswer, SecretUseReply,
    SecretUseRequest,
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

/// What the vault says about one concrete outgoing request.
#[derive(Debug)]
pub enum UseAnswer {
    /// Key and location for this request only.
    Released(ReleasedApiKey),
    /// The vault is waiting for the user's answer.
    ApprovalRequired(ApprovalRequest),
    /// Uniform refusal: forwarding the literal placeholder reveals no key.
    NotFound,
    /// Transport, grammar, setup, and store failures never permit substitution.
    Unavailable,
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

/// Request-addressed release and local audit reporting.
///
/// Both scopes are [`crate::secret::SecretScope`] values the daemon stamped,
/// never tool arguments or session ids.
pub trait SecretVault: Send + Sync {
    /// Ask once for every outgoing request, including repeated requests after
    /// approval. The vault audits uses and may rotate material between calls.
    fn use_secret(
        &self,
        scope: &crate::secret::SecretScope,
        label: &str,
        target: &ApiKeyTarget,
    ) -> UseAnswer;

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

/// Standalone processes have no attested AFT identity and cannot ask the vault.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableVault;

impl SecretVault for UnavailableVault {
    fn use_secret(
        &self,
        _scope: &crate::secret::SecretScope,
        _label: &str,
        _target: &ApiKeyTarget,
    ) -> UseAnswer {
        UseAnswer::Unavailable
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
    fn secret_use(&self, params: serde_json::Value) -> Result<zeroize::Zeroizing<Vec<u8>>, String>;
}

/// The real client: `secret.use` over the daemon's transport (§15).
///
/// AFT is one of only two senders the vault admits, and it receives only API
/// keys. Each reply belongs to the real target supplied with that call.
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
        super::wire_owned::parse(&body)
    }
}

impl SecretVault for SubcSecretVault {
    fn use_secret(
        &self,
        scope: &crate::secret::SecretScope,
        label: &str,
        target: &ApiKeyTarget,
    ) -> UseAnswer {
        match self.call(scope, label, wire::Target::ApiKey(target.clone())) {
            Ok(SecretUseAnswer::Reply(SecretUseReply::ReleasedApiKey(key))) => {
                UseAnswer::Released(key)
            }
            Ok(SecretUseAnswer::Reply(SecretUseReply::ApprovalRequired(request))) => {
                UseAnswer::ApprovalRequired(request)
            }
            Ok(SecretUseAnswer::Refused(error)) if error.code == "not_found" => UseAnswer::NotFound,
            _ => UseAnswer::Unavailable,
        }
    }

    fn audit(&self, _record: &AuditRecordV1) -> Result<(), AuditError> {
        // The contract has no AFT-facing sink yet; the production stand-in's
        // counter is the honest record until one lands.
        UNRECORDED_AUDIT_RECORDS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The fake vault, compiled only for tests or an explicitly feature-enabled
/// build.
#[cfg(any(test, feature = "secret-test-fake"))]
pub(crate) mod fake;
