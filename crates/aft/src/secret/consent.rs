//! V3: the secret consent entry point AFT's own tools dispatch to.
//!
//! A secret consent is not a filesystem permission. It is filed for every
//! eligible scope, including a bound codemode call, and it carries the vault's
//! own request id and statement so the phone screen can show what the vault
//! asked. Cingulate's allow alone grants nothing: the caller resolves again
//! afterwards, and only a vault grant allows a substitution.
//!
//! This entry point is deliberately none of the three existing consent paths:
//! [`super::access_grants::ask_route`] (which returns `Unavailable` for a
//! codemode call), `consent::run_with_consent` (Consent routes only) and
//! `consent::requested_ask` (filesystem `AccessGrant`s only). A secret
//! question never becomes an `AccessGrant`, and the filesystem paths are
//! untouched by it.

use crate::secret::SecretScope;

/// The consent kind a secret ask files under.
///
/// SUBC has not confirmed this name yet (the campaign's open question); the
/// adapter is the one place it is spelled, so the confirmation changes one
/// constant, not every call site.
pub const SECRET_CONSENT_KIND: &str = "secret_use";

/// What the consent adapter is asked to file for one label (V3).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretConsentRequest {
    /// The authorization scope the ask is filed under (V2).
    pub scope: SecretScope,
    /// The label the vault is waiting on.
    pub label: String,
    /// The vault's identifier for the pending decision.
    pub request_id: String,
    /// The vault's statement bytes, base64url without padding.
    pub statement_b64url: String,
}

impl std::fmt::Debug for SecretConsentRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretConsentRequest")
            .field("scope", &self.scope)
            .field("label", &self.label)
            .field("request_id", &self.request_id)
            .field("statement_b64url_len", &self.statement_b64url.len())
            .finish()
    }
}

/// What happened to a filed secret consent (V3).
#[derive(Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// Cingulate answered. Whatever it said, the caller must resolve again;
    /// the answer alone grants nothing.
    Answered,
    /// Still pending when the call's hold ended: the call ends
    /// `access_pending` and the retried call asks again.
    Pending,
    /// The ask could not be filed or awaited: the call ends
    /// `access_unavailable`.
    Unavailable {
        /// Plain-text detail with no key material.
        detail: String,
    },
}

impl std::fmt::Debug for ConsentOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsentOutcome::Answered => f.write_str("Answered"),
            ConsentOutcome::Pending => f.write_str("Pending"),
            ConsentOutcome::Unavailable { detail } => f
                .debug_struct("Unavailable")
                .field("detail", detail)
                .finish(),
        }
    }
}

/// What a dispatcher is asked to do with one secret consent.
///
/// This is the seam between the adapter and the transport: the dispatcher files
/// the ask through the daemon's Cingulate client and reports what happened.
/// The trait exists so the outcome table can be tested without a daemon, and
/// so a later transport shape changes only the installed dispatcher.
pub trait SecretConsentTransport: Send + Sync {
    /// File `request` and wait for the answer, or for the hold to end.
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome;

    /// Settle a pending ask the call left behind.
    fn withhold(&self, request: &SecretConsentRequest) -> ConsentOutcome;
}

/// The dispatcher installed for this process.
///
/// `None` means no secret consent transport is connected, which is the state
/// until SUBC confirms the consent kind. A pending vault answer then ends the
/// call with `access_unavailable`, never with a silent start.
static TRANSPORT: std::sync::LazyLock<
    std::sync::Mutex<Option<std::sync::Arc<dyn SecretConsentTransport>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Install the secret consent transport.
pub fn install_transport(transport: Option<std::sync::Arc<dyn SecretConsentTransport>>) {
    *TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = transport;
}

fn transport() -> Option<std::sync::Arc<dyn SecretConsentTransport>> {
    TRANSPORT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// File `request` and report what happened (V3).
///
/// The caller has already decided the asking call is eligible, so the scope in
/// `request` is the one the daemon stamped (V2).
pub fn file_consent(request: &SecretConsentRequest) -> ConsentOutcome {
    match transport() {
        Some(transport) => transport.file(request),
        None => ConsentOutcome::Unavailable {
            detail: "no secret consent transport is connected".into(),
        },
    }
}

/// Settle a pending secret ask (V3).
pub fn withhold_consent(request: &SecretConsentRequest) -> ConsentOutcome {
    match transport() {
        Some(transport) => transport.withhold(request),
        None => ConsentOutcome::Unavailable {
            detail: "no secret consent transport is connected".into(),
        },
    }
}

/// The `consent.request` parameter shape a secret ask carries (V3).
///
/// It is built here, not by the filesystem `request_params`, because a secret
/// ask has no path and names a vault request instead. The shape follows the
/// same envelope the Cingulate client already sends: `kind` plus the fields the
/// consent service accepts, with no unknown members.
pub fn request_params(request: &SecretConsentRequest) -> serde_json::Value {
    serde_json::json!({
        "kind": SECRET_CONSENT_KIND,
        "label": request.label,
        "request_id": request.request_id,
        "statement_b64url": request.statement_b64url,
    })
}
