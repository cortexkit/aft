//! The secret vault seam as the subc dispatch path sees it.
//!
//! This module is where a stamped route becomes an eligibility answer. It
//! reads the daemon's [`RouteIdentity`], so it lives beside the rest of the
//! route policy instead of in the route-agnostic `crate::secret` types.
//!
//! A call is eligible in exactly three forms, and every other call is
//! ineligible and never reaches `resolve`:
//!
//! 1. **Direct Head**: a Classic call whose route carries the head agent's own
//!    stamp. Both scopes are that stamp.
//! 2. **Direct Worker**: the same for a worker's stamp. Both scopes are the
//!    worker's own `(owner, Worker, ref, epoch)`, so a worker's grant never
//!    covers its head or a sibling.
//! 3. **Bound codemode**: a Codemode call `codemode::admit` already bound to a
//!    live head route. The lifetime scope is the run's own Ephemeral stamp; the
//!    authorization scope is the head route's.
//!
//! Ineligible: no stamp, a standalone call, a plugin-only route, another
//! owner, a flow-read or rich-UI route, and any Ephemeral stamp on a
//! non-codemode identity (parented or not).
//!
//! Both scopes are rebuilt from the daemon's stamp on every call. A background
//! command resolves them once at launch and keeps the owned values, which is
//! why [`CallScopes`] holds plain data rather than borrowing a route.

use super::{carrier_read_only, flow_read, tool_provider, RouteIdentity};
use crate::secret::eligibility::{alfonso_of_kind, COMMAND_SCOPE_KINDS};
use crate::secret::{
    file_consent, withhold_consent, CallScopes, ConsentOutcome, EligibilityForm, Ineligibility,
    LabelOutcome, Resolution, SecretConsentRequest, SecretScope,
};
use std::sync::Arc;
use std::time::Duration;
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

const SECRET_CONSENT_TIMEOUT: Duration = Duration::from_secs(10);

/// Use the same scoped daemon connection and reserved AFT filer as permission
/// asks. The production vault remains the fail-closed stand-in.
pub(super) fn install_subc_transport(transport: Arc<dyn super::consent::ConsentTransport>) {
    crate::secret::consent::install_transport(Some(Arc::new(SubcSecretConsent {
        transport,
        runtime: tokio::runtime::Handle::current(),
    })));
}

struct SubcSecretConsent {
    transport: Arc<dyn super::consent::ConsentTransport>,
    runtime: tokio::runtime::Handle,
}

fn consent_unavailable(detail: &str) -> ConsentOutcome {
    ConsentOutcome::Unavailable {
        detail: detail.into(),
    }
}

/// Generic secret cards have `el_` ids and their own states; filesystem
/// permission cards use a different response contract.
fn decode_secret_elicitation(result: &serde_json::Value) -> Option<&str> {
    result
        .as_object()
        .filter(|object| object.len() == 1)?
        .get("elicitation_id")?
        .as_str()
        .filter(|id| id.starts_with("el_") && id.len() > 3)
}

fn decode_secret_record(record: &serde_json::Value) -> ConsentOutcome {
    if record.get("kind").and_then(serde_json::Value::as_str) != Some("secret_use") {
        return consent_unavailable("Cingulate did not return a secret-use card");
    }
    match record.get("state").and_then(serde_json::Value::as_str) {
        Some("pending") => ConsentOutcome::Pending,
        // The answer is only a cue to retry the vault, never a grant or denial
        // to apply locally. In particular, a decline also retries secret.use.
        Some("answered") => ConsentOutcome::Answered,
        Some("expired") => consent_unavailable("the secret-use card ended without an answer"),
        _ => consent_unavailable("Cingulate returned an unknown secret-use card state"),
    }
}

impl SubcSecretConsent {
    async fn call(
        &self,
        request: &SecretConsentRequest,
        method: &'static str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, super::consent::ConsentError> {
        let stamp = request.scope.stamp();
        let call = super::consent::ConsentCall {
            scope: subc_protocol::scope::ScopeSelector::new(
                stamp.owner.clone(),
                stamp.scope_ref.clone(),
                Some(stamp.scope_epoch),
            ),
            project_root: request.project_root.clone(),
            session: request.session.clone(),
            method,
            params,
            timeout,
        };
        tokio::time::timeout(timeout, self.transport.call(call))
            .await
            .map_err(|_| {
                super::consent::ConsentError::Unavailable("secret consent timed out".into())
            })?
    }

    async fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        // Cingulate deduplicates the exact ordered items for this filer. Refile
        // unchanged to recover the same card across retries or a lost reply,
        // without refreshing its deadline or introducing a local idempotency key.
        let filed = match self
            .call(
                request,
                "consent.request",
                crate::secret::request_params(request),
                SECRET_CONSENT_TIMEOUT,
            )
            .await
        {
            Ok(filed) => filed,
            Err(_) => return consent_unavailable("Cingulate could not file the secret-use card"),
        };
        let Some(id) = decode_secret_elicitation(&filed) else {
            return consent_unavailable("Cingulate did not return a secret-use card id");
        };
        let hold = super::consent::hold_budget(None);
        match self
            .call(
                request,
                "consent.await",
                serde_json::json!({"elicitation_id": id, "timeout_ms": hold.as_millis() as u64}),
                hold + SECRET_CONSENT_TIMEOUT,
            )
            .await
        {
            Ok(record) => decode_secret_record(&record),
            Err(_) => consent_unavailable("Cingulate could not settle the secret-use card"),
        }
    }

    fn dispatch(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        let future = self.file(request);
        match tokio::runtime::Handle::try_current() {
            Ok(current)
                if current.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
            {
                tokio::task::block_in_place(|| self.runtime.block_on(future))
            }
            Ok(_) => consent_unavailable("secret consent cannot block a single-threaded runtime"),
            Err(_) => self.runtime.block_on(future),
        }
    }
}

impl crate::secret::SecretConsentTransport for SubcSecretConsent {
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        self.dispatch(request)
    }

    fn withhold(&self, _request: &SecretConsentRequest) -> ConsentOutcome {
        // Cingulate has not published a withdrawal method for secret_use cards
        // yet. Do not send the filesystem permission method; the vault still
        // governs release and the card's original deadline is unchanged.
        consent_unavailable("secret-use card withdrawal is not supported")
    }
}

/// Decide whether the call on `identity` may ask the vault, and under which
/// scopes.
///
/// The caller must have run `codemode::admit` already: a bound codemode call is
/// recognized by `run_owner`, which admission is what sets.
pub(super) fn call_scopes(
    identity: &RouteIdentity,
) -> Result<(CallScopes, EligibilityForm), Ineligibility> {
    // A rich-UI route only reads, and a flow-read route answers a scheduled
    // flow's file question. Neither is a command whose lifetime a grant could
    // follow, and a flow may run outside every session an answer would reach.
    if carrier_read_only::restricted(identity)
        || flow_read::principal_is_flow_read_carrier(&identity.spawn_principal)
    {
        return Err(Ineligibility::FlowRead);
    }
    if identity.run_owner.is_some() {
        return bound_codemode(identity);
    }
    let Some(stamp) = identity.scope.as_ref() else {
        return Err(match identity.role {
            tool_provider::RouteRole::Legacy => Ineligibility::Standalone,
            tool_provider::RouteRole::ToolProviderV1 => Ineligibility::PluginOnlyRoute,
        });
    };
    if !alfonso_of_kind(stamp, &COMMAND_SCOPE_KINDS) {
        return Err(Ineligibility::OtherOwner);
    }
    if identity.call_mode != CallMode::Classic {
        return Err(Ineligibility::NotACommandScope);
    }
    let own = SecretScope::from_stamp(stamp);
    match stamp.kind {
        ScopeKind::Head => Ok((
            CallScopes {
                authorization: own.clone(),
                lifetime: own,
            },
            EligibilityForm::DirectHead,
        )),
        ScopeKind::Worker => Ok((
            CallScopes {
                authorization: own.clone(),
                lifetime: own,
            },
            EligibilityForm::DirectWorker,
        )),
        // An Ephemeral stamp on a non-codemode identity is a run nobody bound,
        // or a worker's run: neither may reach the vault.
        ScopeKind::Ephemeral => Err(if stamp.parent.is_some() {
            Ineligibility::WorkerRunSession
        } else {
            Ineligibility::EphemeralOutsideCodemode
        }),
    }
}

/// Form 3: a codemode call bound to a live head route.
fn bound_codemode(
    identity: &RouteIdentity,
) -> Result<(CallScopes, EligibilityForm), Ineligibility> {
    let owner = identity
        .run_owner
        .as_ref()
        .expect("a bound codemode call carries its run owner");
    let Some(run_stamp) = identity.scope.as_ref() else {
        return Err(Ineligibility::NoScope);
    };
    if identity.call_mode != CallMode::Codemode
        || !alfonso_of_kind(run_stamp, &[ScopeKind::Ephemeral])
    {
        return Err(Ineligibility::EphemeralOutsideCodemode);
    }
    // Every codemode run is linked to the head it runs for, and that link must
    // still be live. A run whose parent is missing or not `linked` has no head
    // lifetime to follow, so it never reaches the vault.
    let Some(parent) = run_stamp.parent.as_ref() else {
        return Err(Ineligibility::RunNotBound);
    };
    if run_stamp.parent_state != Some(subc_protocol::scope::ParentState::Linked) {
        return Err(Ineligibility::RunNotBound);
    }
    // The parent link must name the very head the daemon bound this call to.
    // Otherwise a run parented to a worker (which `resolve_owner` refuses with
    // `head_route_not_bound`) could arrive here carrying a head route and
    // inherit that head's authority through the vault.
    let Some(head_stamp) = owner.head.scope.as_ref() else {
        return Err(Ineligibility::HeadScopeNotAlfonso);
    };
    if parent.owner != head_stamp.owner
        || parent.scope_ref != head_stamp.scope_ref
        || parent.scope_epoch != head_stamp.scope_epoch
    {
        return Err(Ineligibility::WorkerRunSession);
    }
    // `resolve_owner` only compares the bound head's owner with the run's
    // parent owner, so another authorized owner could otherwise bind a route
    // that reaches a non-prefrontal head stamp. Authorization re-checks the
    // head here; that is what makes the run prefrontal-core.
    if !alfonso_of_kind(head_stamp, &[ScopeKind::Head]) {
        return Err(Ineligibility::HeadScopeNotAlfonso);
    }
    Ok((
        CallScopes {
            authorization: SecretScope::from_stamp(head_stamp),
            lifetime: SecretScope::from_stamp(run_stamp),
        },
        EligibilityForm::BoundCodemode,
    ))
}

/// A launch outcome that may end the call with `access_pending`, carrying
/// the ask it needs answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingSecretConsent {
    /// The ask filed under the authorization scope.
    pub(super) request: SecretConsentRequest,
    /// The label being waited on.
    pub(super) label: String,
    /// The exact message the held call returns.
    pub(super) message: &'static str,
}

pub(super) enum SecretConsentFiling {
    Answered,
    Pending(PendingSecretConsent),
}

/// File the consent a `FileConsent` outcome asks for, under the call's
/// authorization scope.
///
/// A filing failure or expired card ends the call with `access_unavailable`;
/// a pending card holds it with `access_pending`. An answer only tells the
/// launch to ask the vault again before starting any child, job or proxy.
pub(super) fn file_secret_consent(
    identity: &RouteIdentity,
    scopes: &CallScopes,
    label: &str,
    request_id: &str,
    statement_b64url: &str,
) -> Result<SecretConsentFiling, &'static str> {
    let request = SecretConsentRequest {
        scope: scopes.authorization.clone(),
        project_root: identity.project_root.clone(),
        session: identity.session.clone(),
        label: label.to_string(),
        request_id: request_id.to_string(),
        statement_b64url: statement_b64url.to_string(),
    };
    match file_consent(&request) {
        // No usable card or answer: nothing is held, nothing starts.
        ConsentOutcome::Unavailable { .. } => Err(crate::secret::SECRET_ACCESS_UNAVAILABLE),
        ConsentOutcome::Answered => Ok(SecretConsentFiling::Answered),
        ConsentOutcome::Pending => Ok(SecretConsentFiling::Pending(PendingSecretConsent {
            request,
            label: label.to_string(),
            message: crate::secret::SECRET_ACCESS_PENDING_MESSAGE,
        })),
    }
}

/// Settle a pending secret ask the call left behind.
pub(super) fn withdraw_secret_consent(pending: &PendingSecretConsent) {
    let _ = withhold_consent(&pending.request);
}

/// Whether a stamp is one this seam may ever hand to a vault.
pub(super) fn is_command_stamp(stamp: &ScopeStamp) -> bool {
    alfonso_of_kind(stamp, &COMMAND_SCOPE_KINDS)
}

/// What one launch decided for one label, before anything starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LaunchDecision {
    /// The label may be used: the rule to follow and the grant to ask for
    /// material with.
    Proceed {
        /// The label's rule, as the vault resolved it.
        rule: secret_rules::location::LabelRule,
        /// The grant the material call names.
        grant_id: String,
    },
    /// The launch is held until the user answers the filed ask.
    Pending(PendingSecretConsent),
    /// The launch ends with this outcome, whose code names the reason.
    Refuse(LabelOutcome),
}

/// Resolve one scanned label for the launch on `identity`: the one entry a
/// bash or fetch launch calls for each label it found in the command text.
///
/// The order is the outcome table's: an ineligible scope refuses with
/// `secret_refused` before the vault is asked anything, then resolution decides
/// between proceeding, holding the call for consent, and refusing. A label the
/// vault needs an answer for files one consent ask under the call's
/// authorization scope. An answer retries the vault immediately; a pending
/// card holds the launch until the caller retries. Neither answer grants here.
pub(crate) fn resolve_label_for_launch(
    identity: &RouteIdentity,
    vault: &dyn crate::secret::SecretVault,
    label: &str,
) -> LaunchDecision {
    let (scopes, _) = match call_scopes(identity) {
        Ok(scopes) => scopes,
        Err(reason) => {
            return LaunchDecision::Refuse(crate::secret::refuse_ineligible(label, reason))
        }
    };
    let outcome = crate::secret::outcome_for(label, &vault.resolve(&scopes.authorization, label));
    match outcome {
        LabelOutcome::Proceed => match vault.resolve(&scopes.authorization, label) {
            Resolution::Granted { rule, grant_id, .. } => {
                LaunchDecision::Proceed { rule, grant_id }
            }
            // A vault that grants on one call and then does not on the next is
            // not a vault this launch can proceed on.
            other => LaunchDecision::Refuse(crate::secret::outcome_for(label, &other)),
        },
        LabelOutcome::FileConsent {
            request_id,
            statement_b64url,
        } => match file_secret_consent(identity, &scopes, label, &request_id, &statement_b64url) {
            Ok(SecretConsentFiling::Pending(pending)) => LaunchDecision::Pending(pending),
            Ok(SecretConsentFiling::Answered) => {
                match vault.resolve(&scopes.authorization, label) {
                    Resolution::Granted { rule, grant_id, .. } => {
                        LaunchDecision::Proceed { rule, grant_id }
                    }
                    other => LaunchDecision::Refuse(crate::secret::outcome_for_second_resolve(
                        label, &other,
                    )),
                }
            }
            Err(code) => LaunchDecision::Refuse(crate::secret::refuse(
                code,
                label,
                "the consent ask could not be filed",
            )),
        },
        refusal => LaunchDecision::Refuse(refusal),
    }
}

/// The eligibility answer for the tool call executing on this thread.
///
/// The dispatch path installs it once per call, before the call runs, so a
/// launch reads the answer without seeing the route identity. A launch that
/// finds nothing installed refuses: a scope that was never resolved is not an
/// eligible one.
pub(super) fn current_call_scopes() -> Result<(CallScopes, EligibilityForm), Ineligibility> {
    CURRENT_SCOPES
        .with(|slot| slot.borrow().clone())
        .unwrap_or(Err(Ineligibility::NoScope))
}

thread_local! {
    /// `None` while no call is being dispatched on this thread.
    static CURRENT_SCOPES: std::cell::RefCell<Option<Result<(CallScopes, EligibilityForm), Ineligibility>>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs the eligibility answer for one call and restores the previous one
/// when it drops, so the executor lane never carries a stale answer into the
/// next call.
pub(super) struct SecretScopeGuard(Option<Result<(CallScopes, EligibilityForm), Ineligibility>>);

impl SecretScopeGuard {
    pub(super) fn install(
        scopes: Option<Result<(CallScopes, EligibilityForm), Ineligibility>>,
    ) -> Self {
        SecretScopeGuard(CURRENT_SCOPES.with(|slot| slot.replace(scopes)))
    }
}

impl Drop for SecretScopeGuard {
    fn drop(&mut self) {
        CURRENT_SCOPES.with(|slot| slot.replace(self.0.take()));
    }
}

#[cfg(test)]
#[path = "secret_consent_tests.rs"]
mod tests;
