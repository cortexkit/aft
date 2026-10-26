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
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

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

/// File the consent a `FileConsent` outcome asks for, under the call's
/// authorization scope.
///
/// A filing failure ends the call with `access_unavailable`; a filed ask ends
/// it with `access_pending` and the label's own message, so no child, job or
/// proxy starts while the answer is outstanding.
pub(super) fn file_secret_consent(
    scopes: &CallScopes,
    label: &str,
    request_id: &str,
    statement_b64url: &str,
) -> Result<PendingSecretConsent, &'static str> {
    let request = SecretConsentRequest {
        scope: scopes.authorization.clone(),
        label: label.to_string(),
        request_id: request_id.to_string(),
        statement_b64url: statement_b64url.to_string(),
    };
    match file_consent(&request) {
        // Cingulate could not place the ask: nothing is held, nothing starts.
        ConsentOutcome::Unavailable { .. } => Err(crate::secret::SECRET_ACCESS_UNAVAILABLE),
        // Answered or still open, the ask is filed and the call is held. Either
        // way the retried call resolves again; Cingulate's answer alone has
        // granted nothing.
        ConsentOutcome::Answered | ConsentOutcome::Pending => Ok(PendingSecretConsent {
            request,
            label: label.to_string(),
            message: crate::secret::SECRET_ACCESS_PENDING_MESSAGE,
        }),
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
/// authorization scope, and Cingulate's answer alone never grants: the retried
/// launch resolves again.
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
        } => match file_secret_consent(&scopes, label, &request_id, &statement_b64url) {
            Ok(pending) => LaunchDecision::Pending(pending),
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
