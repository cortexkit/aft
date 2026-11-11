//! The secret vault seam as the subc dispatch path sees it.
//!
//! This module is where a stamped route becomes an eligibility answer. It
//! reads the daemon's [`RouteIdentity`], so it lives beside the rest of the
//! route policy instead of in the route-agnostic `crate::secret` types.
//!
//! A call is eligible in exactly three forms, and every other call is
//! ineligible and never asks the vault:
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
#[cfg(test)]
use crate::secret::{file_consent, withhold_consent};
use crate::secret::{
    CallScopes, ConsentOutcome, EligibilityForm, Ineligibility, LabelOutcome, SecretConsentRequest,
    SecretScope,
};
use std::sync::Arc;
use std::time::Duration;
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::ScopeKind;
#[cfg(test)]
use subc_protocol::scope::ScopeStamp;

const SECRET_CONSENT_TIMEOUT: Duration = Duration::from_secs(10);
const SECRET_AWAIT_CAP: Duration = Duration::from_secs(120);

/// Use the daemon connection that files filesystem-access questions with
/// Cingulate as its reserved AFT module principal. Retain each secret card's id
/// so a held call waits for the existing card instead of filing another.
pub(super) fn install_subc_transport(transport: Arc<dyn super::consent::ConsentTransport>) {
    crate::secret::consent::install_transport(Some(Arc::new(SubcSecretConsent {
        transport,
        runtime: tokio::runtime::Handle::current(),
        cards: Default::default(),
    })));
}

struct SubcSecretConsent {
    transport: Arc<dyn super::consent::ConsentTransport>,
    runtime: tokio::runtime::Handle,
    cards: tokio::sync::Mutex<
        std::collections::HashMap<(SecretScope, String), Arc<tokio::sync::Mutex<Card>>>,
    >,
}

struct Card {
    created: std::time::Instant,
    statement: String,
    elicitation: Option<String>,
    answered: Option<ConsentOutcome>,
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

fn decode_secret_record(record: &serde_json::Value, request_id: &str) -> ConsentOutcome {
    if record.get("kind").and_then(serde_json::Value::as_str) != Some("secret_use") {
        return consent_unavailable("Cingulate did not return a secret-use card");
    }
    match record.get("state").and_then(serde_json::Value::as_str) {
        Some("pending") => ConsentOutcome::Pending,
        Some("answered") => {
            let answer = &record["secret_use_answer"];
            let decision = if let Some(items) = answer["items"].as_array() {
                items
                    .iter()
                    .find(|item| item["request_id"].as_str() == Some(request_id))
                    .and_then(|item| item["decision"].as_str())
            } else {
                answer["decision"].as_str()
            };
            if decision == Some("allow") {
                if answer["duration"].as_str() == Some("once") {
                    ConsentOutcome::AnsweredOnce
                } else {
                    ConsentOutcome::Answered
                }
            } else {
                ConsentOutcome::Declined
            }
        }
        Some("expired" | "withdrawn") => ConsentOutcome::Expired,
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

    async fn file(&self, request: &SecretConsentRequest, hold: Duration) -> ConsentOutcome {
        self.file_cancellable(request, hold, None).await
    }

    async fn file_cancellable(
        &self,
        request: &SecretConsentRequest,
        hold: Duration,
        cancel: Option<&crate::secret::consent::Cancellation>,
    ) -> ConsentOutcome {
        let card = {
            let mut cards = self.cards.lock().await;
            cards.retain(|_, card| {
                Arc::strong_count(card) > 1
                    || card
                        .try_lock()
                        .is_ok_and(|card| card.created.elapsed() < Duration::from_secs(600))
            });
            cards
                .entry((request.scope.clone(), request.request_id.clone()))
                .or_insert_with(|| {
                    Arc::new(tokio::sync::Mutex::new(Card {
                        created: std::time::Instant::now(),
                        statement: request.statement_b64url.clone(),
                        elicitation: None,
                        answered: None,
                    }))
                })
                .clone()
        };
        let mut card = card.lock().await;
        if card.statement != request.statement_b64url {
            return consent_unavailable("Cingulate could not file the secret-use card");
        }
        if let Some(answer) = &card.answered {
            return answer.clone();
        }
        if card.elicitation.is_none() {
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                return ConsentOutcome::Expired;
            }
            // A lost filing reply must not cause the same request to be filed again.
            card.answered = Some(consent_unavailable(
                "Cingulate could not file the secret-use card",
            ));
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
                Err(_) => {
                    return consent_unavailable("Cingulate could not file the secret-use card")
                }
            };
            let Some(id) = decode_secret_elicitation(&filed) else {
                return consent_unavailable("Cingulate did not return a secret-use card id");
            };
            card.elicitation = Some(id.to_owned());
            card.answered = None;
        }
        let await_card = self
            .call(
                request,
                "consent.await",
                serde_json::json!({"elicitation_id": card.elicitation, "timeout_ms": hold.min(SECRET_AWAIT_CAP).as_millis() as u64}),
                hold.min(SECRET_AWAIT_CAP) + SECRET_CONSENT_TIMEOUT,
            );
        let cancelled = async {
            match cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let response = tokio::select! {
            biased;
            () = cancelled => {
                let result = self.call(request, "consent.withdraw", serde_json::json!({"elicitation_id": card.elicitation}), SECRET_CONSENT_TIMEOUT).await;
                if result.is_err() {
                    crate::slog_warn!("secret consent withdrawal failed");
                    return consent_unavailable("Cingulate could not settle the secret-use card");
                }
                card.answered = Some(ConsentOutcome::Expired);
                return ConsentOutcome::Expired;
            },
            response = await_card => response,
        };
        let outcome = match response {
            Ok(record) => decode_secret_record(&record, &request.request_id),
            Err(_) => consent_unavailable("Cingulate could not settle the secret-use card"),
        };
        if matches!(
            outcome,
            ConsentOutcome::Answered
                | ConsentOutcome::AnsweredOnce
                | ConsentOutcome::Declined
                | ConsentOutcome::Expired
        ) {
            card.answered = Some(outcome.clone());
        }
        outcome
    }

    fn dispatch(&self, request: &SecretConsentRequest, hold: Duration) -> ConsentOutcome {
        let future = self.file(request, hold);
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
    fn runtime(&self) -> Option<tokio::runtime::Handle> {
        Some(self.runtime.clone())
    }

    fn wait(
        self: Arc<Self>,
        request: SecretConsentRequest,
        cancel: crate::secret::consent::Cancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ConsentOutcome> + Send>> {
        Box::pin(async move {
            loop {
                let now = crate::secret::vault::now_ms();
                let expires = request.expires_at_ms.unwrap_or(now.saturating_add(600_000));
                if now >= expires {
                    cancel.cancel();
                }
                let outcome = self
                    .file_cancellable(
                        &request,
                        Duration::from_millis(expires.saturating_sub(now)).min(SECRET_AWAIT_CAP),
                        Some(&cancel),
                    )
                    .await;
                if outcome != ConsentOutcome::Pending {
                    return outcome;
                }
                // The Cingulate consent service returns pending at its per-await cap. Re-awaiting
                // the same card is read-only; it never files another card.
                tokio::task::yield_now().await;
            }
        })
    }
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        self.dispatch(request, super::consent::hold_budget(None))
    }

    fn hold(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        self.dispatch(request, Duration::from_secs(600))
    }

    fn withhold(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        let cancel = crate::secret::consent::Cancellation::default();
        cancel.cancel();
        let future = self.file_cancellable(request, Duration::ZERO, Some(&cancel));
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
    // inherit that head's authority through the vault. `owner_route` is the
    // head agent's route for an agent run.
    let Some(head_stamp) = owner.owner_route.scope.as_ref() else {
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
#[cfg(test)]
pub(crate) struct PendingSecretConsent {
    /// The ask filed under the authorization scope.
    pub(super) request: SecretConsentRequest,
    /// The label being waited on.
    pub(super) label: String,
    /// The exact message the held call returns.
    pub(crate) message: &'static str,
}

#[cfg(test)]
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
#[cfg(test)]
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
        expires_at_ms: None,
    };
    match file_consent(&request) {
        // No usable card or answer: nothing is held, nothing starts.
        ConsentOutcome::Unavailable { .. } | ConsentOutcome::Declined | ConsentOutcome::Expired => {
            Err(crate::secret::SECRET_ACCESS_UNAVAILABLE)
        }
        ConsentOutcome::Answered | ConsentOutcome::AnsweredOnce => {
            Ok(SecretConsentFiling::Answered)
        }
        ConsentOutcome::Pending => Ok(SecretConsentFiling::Pending(PendingSecretConsent {
            request,
            label: label.to_string(),
            message: crate::secret::SECRET_ACCESS_PENDING_MESSAGE,
        })),
    }
}

/// Settle a pending secret ask the call left behind.
///
/// Test-only until a production launch path needs to withdraw a filed ask:
/// nothing outside tests calls this yet.
#[cfg(test)]
pub(super) fn withdraw_secret_consent(pending: &PendingSecretConsent) {
    let _ = withhold_consent(&pending.request);
}

/// Result of the fetch's addressed vault call, before sending upstream.
#[derive(Debug)]
pub(crate) enum LaunchDecision {
    Proceed(crate::secret::UseAnswer),
    #[cfg(test)]
    Pending(PendingSecretConsent),
    Refuse(LabelOutcome),
}

/// Fetch tools already know the target. Check eligibility before asking once,
/// then use the existing held-call consent path if approval is required.
#[cfg(test)]
pub(crate) fn use_label_for_fetch(
    identity: &RouteIdentity,
    vault: &dyn crate::secret::SecretVault,
    label: &str,
    target: &crate::secret::ApiKeyTarget,
) -> LaunchDecision {
    let (scopes, _) = match call_scopes(identity) {
        Ok(scopes) => scopes,
        Err(reason) => {
            return LaunchDecision::Refuse(crate::secret::refuse_ineligible(label, reason))
        }
    };
    #[cfg(unix)]
    if vault.can_release_material() && crate::secret_proxy::protect_secret_process().is_err() {
        return LaunchDecision::Refuse(LabelOutcome::Refuse {
            code: crate::secret::SECRETS_UNAVAILABLE,
            message: crate::bash_background::secrets::PROXY_UNAVAILABLE.to_owned(),
        });
    }
    let answer = vault.use_secret(&scopes.authorization, label, target);
    if let crate::secret::UseAnswer::ApprovalRequired(pending) = answer {
        return match file_secret_consent(
            identity,
            &scopes,
            label,
            &pending.request_id,
            &pending.statement_b64url,
        ) {
            Ok(SecretConsentFiling::Pending(pending)) => LaunchDecision::Pending(pending),
            Ok(SecretConsentFiling::Answered) => {
                let answer = vault.use_secret(&scopes.authorization, label, target);
                let outcome = crate::secret::outcome_for_second_use(label, &answer);
                if outcome.proceeds() {
                    LaunchDecision::Proceed(answer)
                } else {
                    LaunchDecision::Refuse(outcome)
                }
            }
            Err(code) => LaunchDecision::Refuse(crate::secret::refuse(
                code,
                label,
                "the consent ask could not be filed",
            )),
        };
    }
    let outcome = crate::secret::outcome_for(label, &answer);
    if outcome.proceeds() {
        LaunchDecision::Proceed(answer)
    } else {
        LaunchDecision::Refuse(outcome)
    }
}

/// The eligibility answer for the tool call executing on this thread.
///
/// The dispatch path installs it once per call, before the call runs, so a
/// launch reads the answer without seeing the route identity. A launch that
/// finds nothing installed refuses: a scope that was never resolved is not an
/// eligible one.
pub(crate) fn current_call_scopes() -> Result<(CallScopes, EligibilityForm), Ineligibility> {
    CURRENT_SCOPES
        .with(|slot| slot.borrow().clone())
        .unwrap_or(Err(Ineligibility::NoScope))
}

thread_local! {
    static CURRENT_IDENTITY: std::cell::RefCell<Option<RouteIdentity>> = const { std::cell::RefCell::new(None) };
    /// `None` while no call is being dispatched on this thread.
    static CURRENT_SCOPES: std::cell::RefCell<Option<Result<(CallScopes, EligibilityForm), Ineligibility>>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn current_secret_consent_context() -> Option<(std::path::PathBuf, String)> {
    current_identity().map(|identity| (identity.project_root.clone(), identity.session.clone()))
}

pub(super) fn current_identity() -> Option<RouteIdentity> {
    CURRENT_IDENTITY.with(|slot| slot.borrow().clone())
}

pub(crate) fn current_secret_audit_scope() -> Option<SecretScope> {
    current_call_scopes()
        .ok()
        .map(|(scopes, _)| scopes.authorization)
        .or_else(|| {
            current_identity()
                .and_then(|identity| identity.scope.as_ref().map(SecretScope::from_stamp))
        })
}

#[cfg(test)]
pub(super) struct SecretContextRestoreCheck {
    scopes: Result<(CallScopes, EligibilityForm), Ineligibility>,
    had_identity: bool,
}

#[cfg(test)]
impl SecretContextRestoreCheck {
    pub(super) fn new() -> Self {
        Self {
            scopes: current_call_scopes(),
            had_identity: current_identity().is_some(),
        }
    }
}

#[cfg(test)]
impl Drop for SecretContextRestoreCheck {
    fn drop(&mut self) {
        assert_eq!(
            current_call_scopes(),
            self.scopes,
            "secret scopes leaked on the executor thread"
        );
        assert_eq!(
            current_identity().is_some(),
            self.had_identity,
            "secret identity leaked on the executor thread"
        );
    }
}

pub(super) struct SecretIdentityGuard(Option<RouteIdentity>);

impl SecretIdentityGuard {
    pub(super) fn install(identity: RouteIdentity) -> Self {
        Self::install_optional(Some(identity))
    }

    pub(super) fn install_optional(identity: Option<RouteIdentity>) -> Self {
        Self(CURRENT_IDENTITY.with(|slot| slot.replace(identity)))
    }
}

impl Drop for SecretIdentityGuard {
    fn drop(&mut self) {
        CURRENT_IDENTITY.with(|slot| slot.replace(self.0.take()));
    }
}

#[cfg(test)]
pub(crate) fn with_secret_identity_for_test<R>(
    root: &std::path::Path,
    stamp: ScopeStamp,
    run: impl FnOnce() -> R,
) -> R {
    let identity = RouteIdentity(Arc::new(test_identity_data(root, stamp)));
    let _scopes = SecretScopeGuard::install(Some(call_scopes(&identity)));
    let _identity = SecretIdentityGuard::install(identity);
    run()
}

#[cfg(test)]
fn test_identity_data(root: &std::path::Path, stamp: ScopeStamp) -> super::RouteIdentityData {
    super::RouteIdentityData {
        output_scope: Default::default(),
        root: crate::path_identity::ProjectRootId::from_path(root).unwrap(),
        project_root: root.to_owned(),
        harness: "runner".into(),
        session: "secret-bash".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: super::BindTrust::FirstParty,
        spawn_principal: crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: false,
        held_reply: false,
        held_update: false,
        call_keepalive: false,
        call_yield: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        scope: Some(stamp),
        call_mode: CallMode::Classic,
        run_scope: std::sync::OnceLock::new(),
        made_tool_call: std::sync::atomic::AtomicBool::new(false),
        run_owner: None,
    }
}

#[cfg(all(test, unix))]
pub(crate) fn with_secret_bound_identity_for_test<R>(
    root: &std::path::Path,
    head: ScopeStamp,
    lifetime: ScopeStamp,
    run: impl FnOnce() -> R,
) -> R {
    let head = RouteIdentity(Arc::new(test_identity_data(root, head)));
    let mut data = test_identity_data(root, lifetime);
    data.call_mode = CallMode::Codemode;
    data.run_owner = Some(Arc::new(super::codemode::RunOwner {
        kind: super::codemode::RunKind::Agent {
            agent_id: "secret-agent".into(),
        },
        run_id: "secret-run".into(),
        task_identity: None,
        owner_route: head.clone(),
        owner_route_channel: super::RouteChannel {
            channel: 1,
            epoch: 1,
        },
        run_route: super::RouteChannel {
            channel: 7,
            epoch: 1,
        },
        run_principal: head.spawn_principal.clone(),
    }));
    let identity = RouteIdentity(Arc::new(data));
    let _scopes = SecretScopeGuard::install(Some(call_scopes(&identity)));
    let _identity = SecretIdentityGuard::install(identity);
    run()
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

/// Fetch and proxy requests join the same scope/request-id cards, each with
/// one person-wait deadline shared by every concurrently requested label.
pub(crate) fn use_current_secret_labels(
    vault: Arc<dyn crate::secret::SecretVault>,
    labels: &[String],
    target: &crate::secret::ApiKeyTarget,
    request_target: &str,
) -> Vec<LaunchDecision> {
    if labels.is_empty() {
        return Vec::new();
    }
    let unavailable = || {
        labels
            .iter()
            .map(|label| {
                LaunchDecision::Refuse(crate::secret::refuse(
                    crate::secret::SECRET_ACCESS_UNAVAILABLE,
                    label,
                    "the consent ask could not be filed",
                ))
            })
            .collect()
    };
    let scopes = match current_call_scopes() {
        Ok((scopes, _)) => scopes,
        Err(reason) => {
            return labels
                .iter()
                .map(|label| {
                    LaunchDecision::Refuse(crate::secret::refuse_ineligible(label, reason))
                })
                .collect()
        }
    };
    #[cfg(unix)]
    if vault.can_release_material() && crate::secret_proxy::protect_secret_process().is_err() {
        return unavailable();
    }
    let coordinator = crate::secret::requests::RequestSecrets::new(
        vault,
        scopes.authorization,
        current_secret_consent_context(),
    );
    let labels = labels.to_vec();
    let target = target.clone();
    let request_target = request_target.to_owned();
    let future = async move {
        use futures_util::StreamExt;
        let mut budget = coordinator.budget();
        let mut pending = futures_util::stream::FuturesUnordered::new();
        for (index, label) in labels.iter().enumerate() {
            let target = &target;
            let request_target = &request_target;
            let coordinator = &coordinator;
            let expires = budget.expires_at_ms;
            pending.push(async move {
                (
                    index,
                    coordinator
                        .use_planned_in_request(
                            label,
                            target,
                            expires,
                            |location| {
                                let location =
                                    secret_rules::location::parse_vault_location(location)
                                        .ok_or(crate::secret::requests::UseError::Unavailable)?;
                                if !matches!(location, secret_rules::location::Location::Query(_)) {
                                    return Ok(());
                                }
                                let authority = secret_rules::target::ConnectAuthority {
                                    host: target.host.clone(),
                                    port: target.port.unwrap_or(443),
                                };
                                let rule = secret_rules::location::LabelRule {
                                    label: label.clone(),
                                    host: target.host.clone(),
                                    port: 443,
                                    path_prefix: target.path.clone(),
                                    location,
                                };
                                let view = secret_rules::location::RequestView::new(
                                    request_target,
                                    Vec::new(),
                                );
                                let dummy =
                                    [(label.clone(), b"proxy-planning-value-0000".to_vec())];
                                // Without a pending card, the vault reveals a deposit location only when releasing its key, so location-dependent refusals can follow that release.
                                if secret_rules::location::evaluate_request_with_connect(
                                    &view,
                                    &authority,
                                    &[rule],
                                    &dummy,
                                )
                                .refusal
                                .is_some()
                                {
                                    Err(crate::secret::requests::UseError::Unavailable)
                                } else {
                                    Ok(())
                                }
                            },
                            |_| Ok(()),
                            || async { Ok(()) },
                        )
                        .await,
                )
            });
        }
        let mut answers: Vec<Option<crate::secret::UseAnswer>> =
            (0..labels.len()).map(|_| None).collect();
        while !pending.is_empty() {
            let result = tokio::select! {
                biased;
                () = budget.expired() => None,
                result = pending.next() => result,
            };
            let Some((index, answer)) = result else {
                return None;
            };
            match answer {
                Ok(answer) => answers[index] = Some(answer),
                Err(_) => return None,
            }
        }
        Some(
            labels
                .iter()
                .zip(answers)
                .map(|(label, answer)| {
                    let answer = answer.unwrap_or(crate::secret::UseAnswer::Unavailable);
                    let outcome = crate::secret::outcome_for(label, &answer);
                    if outcome.proceeds() {
                        LaunchDecision::Proceed(answer)
                    } else {
                        LaunchDecision::Refuse(outcome)
                    }
                })
                .collect(),
        )
    };
    // Blocking fetches run on tool worker threads. A local runtime drives only
    // their coordinator; production card workers stay on the daemon runtime.
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(_) => None,
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| runtime.block_on(future)),
    };
    result.unwrap_or_else(unavailable)
}
