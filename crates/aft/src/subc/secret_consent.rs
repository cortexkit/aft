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
use crate::secret::consent::{CancellationReason, CardKey};
use crate::secret::eligibility::{alfonso_of_kind, COMMAND_SCOPE_KINDS};
#[cfg(test)]
use crate::secret::LabelOutcome;
#[cfg(test)]
use crate::secret::{file_consent, withhold_consent};
use crate::secret::{
    CallScopes, ConsentOutcome, EligibilityForm, Ineligibility, SecretConsentRequest, SecretScope,
};
use std::sync::Arc;
use std::time::Duration;
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::ScopeKind;
#[cfg(test)]
use subc_protocol::scope::ScopeStamp;

const SECRET_CONSENT_TIMEOUT: Duration = Duration::from_secs(10);
const SECRET_AWAIT_CAP: Duration = Duration::from_secs(120);
const SECRET_PENDING_BACKOFF: Duration = Duration::from_millis(50);

pub(super) use crate::secret::call_context::FetchReplyDeadline;

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
    cards: tokio::sync::Mutex<std::collections::HashMap<CardKey, Arc<tokio::sync::Mutex<Card>>>>,
}

struct Card {
    expiry: tokio::time::Instant,
    statement: String,
    elicitation: Option<String>,
    answered: Option<ConsentOutcome>,
    warned_unreadable: bool,
    bus_watch: Option<crate::late_result_bus::readers::CardWatch>,
}

fn consent_unavailable(detail: &str) -> ConsentOutcome {
    ConsentOutcome::Unavailable {
        detail: detail.into(),
    }
}

fn log_secret_card(request: &SecretConsentRequest, id: &str, reason: Option<CancellationReason>) {
    use base64::Engine;
    #[derive(serde::Deserialize)]
    struct Target {
        host: String,
    }
    #[derive(serde::Deserialize)]
    struct Metadata {
        #[serde(rename = "use")]
        target: Option<Target>,
        request_expires_at_ms: Option<u64>,
    }
    // Decode only public card metadata; neither the statement nor unknown
    // fields belong in diagnostics, including when parsing fails.
    let metadata = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&request.statement_b64url)
        .ok()
        .map(zeroize::Zeroizing::new)
        .and_then(|bytes| serde_json::from_slice::<Metadata>(&bytes).ok());
    let host = metadata
        .as_ref()
        .and_then(|metadata| metadata.target.as_ref())
        .map_or("unknown", |target| target.host.as_str());
    let expires = metadata
        .as_ref()
        .and_then(|metadata| metadata.request_expires_at_ms)
        .or(request.expires_at_ms)
        .map_or_else(|| "unknown".to_owned(), |expires| expires.to_string());
    let event = if reason.is_some() {
        "withdrawn"
    } else {
        "filed"
    };
    let reason = reason.map_or(String::new(), |reason| format!(" reason={}", reason.code()));
    crate::slog_info!(
        "secret consent card {event}: request_id={:?} card_id={id:?} label={:?} host={host:?} expires_at_ms={expires}{reason}",
        request.request_id,
        request.label,
    );
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
            let decision = if let Some(items) = answer.get("items") {
                let Some(items) = items.as_array() else {
                    return ConsentOutcome::Indeterminate;
                };
                let mut matching = items
                    .iter()
                    .filter(|item| item["request_id"].as_str() == Some(request_id));
                let decision = matching.next().and_then(|item| item["decision"].as_str());
                if matching.next().is_some() {
                    return ConsentOutcome::Indeterminate;
                }
                decision
            } else {
                answer["decision"].as_str()
            };
            if decision == Some("allow") {
                if answer["duration"].as_str() == Some("once") {
                    ConsentOutcome::AnsweredOnce
                } else {
                    ConsentOutcome::Answered
                }
            } else if decision == Some("decline") {
                ConsentOutcome::Declined
            } else {
                ConsentOutcome::Indeterminate
            }
        }
        Some("expired") => ConsentOutcome::Expired,
        Some("withdrawn") => ConsentOutcome::Cancelled,
        _ => consent_unavailable("Cingulate returned an unknown secret-use card state"),
    }
}

impl SubcSecretConsent {
    async fn withdraw_card(
        &self,
        request: &SecretConsentRequest,
        id: &str,
        reason: CancellationReason,
    ) {
        let result = self
            .call(
                request,
                "consent.withdraw",
                serde_json::json!({"elicitation_id": id}),
                SECRET_CONSENT_TIMEOUT,
            )
            .await;
        let withdrawn = if matches!(result, Err(super::consent::ConsentError::ScopeGone(_))) {
            // The authenticated filer owns the card even after its scoped
            // route can no longer bind. Withdraw without reopening the scope.
            tokio::time::timeout(
                SECRET_CONSENT_TIMEOUT,
                self.transport
                    .withdraw_idle(id.to_owned(), SECRET_CONSENT_TIMEOUT),
            )
            .await
            .is_ok_and(|result| result.is_ok())
        } else {
            result.is_ok()
        };
        if !withdrawn {
            crate::slog_warn!("secret consent withdrawal failed");
        } else {
            log_secret_card(request, id, Some(reason));
        }
    }

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

    async fn recover_lost_filing(
        &self,
        request: &SecretConsentRequest,
        params: serde_json::Value,
        card: &mut Card,
        cancel: Option<&crate::secret::consent::Cancellation>,
    ) -> ConsentOutcome {
        if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
            // An unknown card id cannot be withdrawn. Never create or replay
            // an ask after cancellation; its request-bounded expiry is the
            // backstop for a card whose filing reply was lost.
            card.answered = Some(ConsentOutcome::Cancelled);
            return ConsentOutcome::Cancelled;
        }
        // Cingulate deduplicates an identical secret_use body under the same
        // stamped scope, including its epoch. Recover its id once, then withdraw
        // rather than leaving an unobserved card able to approve a lost caller.
        let recovered = self
            .call(request, "consent.request", params, SECRET_CONSENT_TIMEOUT)
            .await;
        let id = recovered
            .as_ref()
            .ok()
            .and_then(decode_secret_elicitation)
            .map(str::to_owned);
        let Some(id) = id else {
            crate::slog_warn!("secret consent filing recovery failed");
            return consent_unavailable("Cingulate could not file the secret-use card");
        };
        card.elicitation = Some(id.clone());
        log_secret_card(request, &id, None);
        card.answered = Some(ConsentOutcome::Cancelled);
        self.withdraw_card(request, &id, CancellationReason::Cancelled)
            .await;
        ConsentOutcome::Cancelled
    }

    async fn file_cancellable(
        &self,
        request: &SecretConsentRequest,
        hold: Duration,
        cancel: Option<&crate::secret::consent::Cancellation>,
    ) -> ConsentOutcome {
        let key = CardKey(request.scope.clone(), request.request_id.clone());
        let entry = {
            let mut cards = self.cards.lock().await;
            cards.retain(|_, card| {
                Arc::strong_count(card) > 1
                    || card
                        .try_lock()
                        .is_ok_and(|card| tokio::time::Instant::now() < card.expiry)
            });
            cards
                .entry(key.clone())
                .or_insert_with(|| {
                    Arc::new(tokio::sync::Mutex::new(Card {
                        expiry: tokio::time::Instant::now()
                            + Duration::from_millis(request.expires_at_ms.map_or(
                                600_000,
                                |deadline| {
                                    deadline
                                        .saturating_sub(crate::secret::vault::now_ms())
                                        .min(600_000)
                                },
                            )),
                        statement: request.statement_b64url.clone(),
                        elicitation: None,
                        answered: None,
                        warned_unreadable: false,
                        bus_watch: None,
                    }))
                })
                .clone()
        };
        let mut card = entry.lock().await;
        if card.statement != request.statement_b64url {
            return consent_unavailable("Cingulate could not file the secret-use card");
        }
        if let Some(answer) = &card.answered {
            return answer.clone();
        }
        let existing = card.elicitation.is_some();
        if card.elicitation.is_none() {
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                card.answered = Some(ConsentOutcome::Cancelled);
                return ConsentOutcome::Cancelled;
            }
            // Reserve the failure before filing so a failed bounded recovery is
            // terminal in this cache and cannot start another recovery loop.
            card.answered = Some(consent_unavailable(
                "Cingulate could not file the secret-use card",
            ));
            let params = crate::secret::request_params(request);
            let filed = match self
                .call(
                    request,
                    "consent.request",
                    params.clone(),
                    SECRET_CONSENT_TIMEOUT,
                )
                .await
            {
                Ok(filed) => filed,
                Err(super::consent::ConsentError::Unavailable(_)) => {
                    return self
                        .recover_lost_filing(request, params, &mut card, cancel)
                        .await
                }
                Err(error) => {
                    // Cingulate stores nothing for a refused filing, so this
                    // line is the only record of why no card reached the phone.
                    // Log the code alone: a refusal message can quote request
                    // fields, and the label, host and scope identify the call.
                    let code = match &error {
                        super::consent::ConsentError::Refused { code, .. } => code.as_str(),
                        super::consent::ConsentError::ScopeGone(_) => "scope_gone",
                        super::consent::ConsentError::Unavailable(_) => "unavailable",
                    };
                    crate::slog_warn!(
                        "secret consent filing refused: code={code} request_id={} scope={:?}",
                        request.request_id,
                        request.scope
                    );
                    // A named refusal leaves no card behind, so nothing can be
                    // waited on or withdrawn. Drop the reserved failure: callers
                    // already waiting on this entry still share it, but the next
                    // request files again instead of reading this failure for
                    // the rest of the cache's ten minutes. (A lost reply keeps
                    // its entry, because a card may exist that recovery owns.)
                    let mut cards = self.cards.lock().await;
                    if cards
                        .get(&key)
                        .is_some_and(|cached| Arc::ptr_eq(cached, &entry))
                    {
                        cards.remove(&key);
                    }
                    return consent_unavailable("Cingulate could not file the secret-use card");
                }
            };
            let Some(id) = decode_secret_elicitation(&filed) else {
                return self
                    .recover_lost_filing(request, params, &mut card, cancel)
                    .await;
            };
            card.elicitation = Some(id.to_owned());
            log_secret_card(request, id, None);
            card.bus_watch = Some(crate::late_result_bus::readers::watch_card(id));
            card.answered = None;
        }
        let await_card = async {
            if existing {
                if let Some(watch) = &card.bus_watch {
                    watch.wait().await;
                }
            }
            let hold = if card
                .bus_watch
                .as_ref()
                .is_some_and(|watch| watch.attached())
            {
                Duration::ZERO
            } else {
                hold.min(SECRET_AWAIT_CAP)
            };
            self
            .call(
                request,
                "consent.await",
                serde_json::json!({"elicitation_id": card.elicitation, "timeout_ms": hold.min(SECRET_AWAIT_CAP).as_millis() as u64}),
                hold.min(SECRET_AWAIT_CAP) + SECRET_CONSENT_TIMEOUT,
            ).await
        };
        let cancelled = async {
            match cancel {
                Some(cancel) => cancel.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let response = tokio::select! {
            biased;
            () = cancelled => {
                let reason = cancel.and_then(|cancel| cancel.reason()).unwrap_or(CancellationReason::Cancelled);
                self.withdraw_card(request, card.elicitation.as_deref().unwrap(), reason).await;
                card.answered = Some(ConsentOutcome::Cancelled);
                if let Some(watch) = card.bus_watch.take() { watch.receipt().applied(); }
                return ConsentOutcome::Cancelled;
            },
            response = await_card => response,
        };
        let outcome = match response {
            Ok(record) => {
                let outcome = decode_secret_record(&record, &request.request_id);
                if matches!(
                    outcome,
                    ConsentOutcome::Indeterminate | ConsentOutcome::Unavailable { .. }
                ) {
                    let code = if record["state"].as_str() == Some("answered") {
                        let answer = &record["secret_use_answer"];
                        answer["items"]
                            .as_array()
                            .and_then(|items| {
                                items
                                    .iter()
                                    .find(|item| {
                                        item["request_id"].as_str() == Some(&request.request_id)
                                    })
                                    .and_then(|item| item["decision"].as_str())
                            })
                            .or_else(|| answer["decision"].as_str())
                    } else {
                        record["state"].as_str()
                    };
                    crate::secret::consent::record_outcome_code(
                        &request.scope,
                        &request.request_id,
                        code.unwrap_or(""),
                    );
                }
                if outcome == ConsentOutcome::Indeterminate && !card.warned_unreadable {
                    card.warned_unreadable = true;
                    // Peer decision text is untrusted and can contain a key.
                    // Fingerprint its value instead of copying it into logs.
                    use sha2::{Digest, Sha256};
                    let value = zeroize::Zeroizing::new(
                        serde_json::to_vec(&record["secret_use_answer"]).unwrap_or_default(),
                    );
                    crate::slog_warn!(
                        "unreadable secret consent answer: card={} decision_sha256={:x}",
                        card.elicitation.as_deref().unwrap_or("unknown"),
                        Sha256::digest(&*value)
                    );
                }
                outcome
            }
            Err(_) => consent_unavailable("Cingulate could not settle the secret-use card"),
        };
        if matches!(
            outcome,
            ConsentOutcome::Answered
                | ConsentOutcome::AnsweredOnce
                | ConsentOutcome::Declined
                | ConsentOutcome::Expired
                | ConsentOutcome::Cancelled
        ) {
            card.answered = Some(outcome.clone());
            if let Some(watch) = card.bus_watch.take() {
                watch.receipt().applied();
            }
        }
        if cancel.is_some() && matches!(outcome, ConsentOutcome::Unavailable { .. }) {
            // An unsuccessful await still leaves a live phone card. Withdraw it
            // before returning an error, using the same authorization scope as consent.request.
            self.withdraw_card(
                request,
                card.elicitation.as_deref().unwrap(),
                CancellationReason::Cancelled,
            )
            .await;
            // A known id means the card was filed for the person. A failed
            // await is cancellation, never evidence of a decline or expiry.
            card.answered = Some(ConsentOutcome::Cancelled);
            if let Some(watch) = card.bus_watch.take() {
                watch.receipt().applied();
            }
            return ConsentOutcome::Cancelled;
        }
        if outcome != ConsentOutcome::Pending {
            card.bus_watch.take();
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
            let mut request = request;
            let expires = request
                .expires_at_ms
                .unwrap_or(u64::MAX)
                .min(crate::secret::vault::now_ms().saturating_add(600_000));
            request.expires_at_ms = Some(expires);
            loop {
                let now = crate::secret::vault::now_ms();
                if now >= expires {
                    cancel.cancel_with_reason(CancellationReason::Expired);
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
                // Re-await the same card, but a service that returns pending
                // early must not turn a held request into a tight polling loop.
                let delay = SECRET_PENDING_BACKOFF.min(Duration::from_millis(
                    expires.saturating_sub(crate::secret::vault::now_ms()),
                ));
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {},
                    () = tokio::time::sleep(delay) => {},
                }
            }
        })
    }
    fn file(&self, request: &SecretConsentRequest) -> ConsentOutcome {
        self.dispatch(request, super::consent::hold_budget(None))
    }

    fn hold(
        &self,
        request: &SecretConsentRequest,
        cancel: &crate::secret::consent::Cancellation,
    ) -> ConsentOutcome {
        let future = self.file_cancellable(request, Duration::from_secs(600), Some(cancel));
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

    fn forget_expired(
        &self,
        request: &SecretConsentRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let key = CardKey(request.scope.clone(), request.request_id.clone());
        Box::pin(async move {
            self.cards.lock().await.remove(&key);
        })
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
        ScopeKind::Federated => Err(Ineligibility::NotACommandScope),
        _ => Err(Ineligibility::NotACommandScope),
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
    // head here; that is what makes the run Prefrontal-owned.
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
        ConsentOutcome::Unavailable { .. }
        | ConsentOutcome::Declined
        | ConsentOutcome::Expired
        | ConsentOutcome::Indeterminate
        | ConsentOutcome::Cancelled => Err(crate::secret::SECRET_ACCESS_UNAVAILABLE),
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

/// Test-only result of the addressed vault call before sending upstream.
#[cfg(test)]
#[derive(Debug)]
pub(crate) enum LaunchDecision {
    Proceed(crate::secret::UseAnswer),
    #[cfg(test)]
    Pending(PendingSecretConsent),
    Refuse(LabelOutcome),
}

#[cfg(test)]
use crate::secret::fetch::fetch_consent_refusal;

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
            Err(_) => LaunchDecision::Refuse(fetch_consent_refusal(
                label,
                &target.host,
                crate::secret::requests::UseError::Unavailable,
                &Default::default(),
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

#[cfg(test)]
use crate::context::call_context::current_call_scopes;
pub(crate) use crate::context::call_context::SecretScopeGuard;

thread_local! {
    static CURRENT_IDENTITY: std::cell::RefCell<Option<RouteIdentity>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn current_identity() -> Option<RouteIdentity> {
    CURRENT_IDENTITY.with(|slot| slot.borrow().clone())
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

pub(super) struct SecretIdentityGuard {
    previous: Option<RouteIdentity>,
    _context: crate::context::call_context::IdentityGuard,
}

impl SecretIdentityGuard {
    pub(super) fn install(identity: RouteIdentity) -> Self {
        Self::install_optional(Some(identity))
    }

    pub(super) fn install_optional(identity: Option<RouteIdentity>) -> Self {
        let audit_scope = identity
            .as_ref()
            .and_then(|identity| identity.scope.as_ref().map(SecretScope::from_stamp));
        let consent_context = identity
            .as_ref()
            .map(|identity| (identity.project_root.clone(), identity.session.clone()));
        Self {
            previous: CURRENT_IDENTITY.with(|slot| slot.replace(identity)),
            _context: crate::context::call_context::install_identity(consent_context, audit_scope),
        }
    }
}

impl Drop for SecretIdentityGuard {
    fn drop(&mut self) {
        CURRENT_IDENTITY.with(|slot| slot.replace(self.previous.take()));
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
        owner_route_channel: Some(super::RouteChannel {
            channel: 1,
            epoch: 1,
        }),
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

#[cfg(test)]
#[path = "secret_consent_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) fn use_current_secret_label(
    vault: &dyn crate::secret::SecretVault,
    label: &str,
    target: &crate::secret::ApiKeyTarget,
) -> LaunchDecision {
    CURRENT_IDENTITY.with(|slot| match slot.borrow().as_ref() {
        Some(identity) => use_label_for_fetch(identity, vault, label, target),
        None => LaunchDecision::Refuse(crate::secret::refuse_ineligible(
            label,
            Ineligibility::NoScope,
        )),
    })
}

#[cfg(all(test, unix))]
#[path = "secret_proxy_vault_tests.rs"]
mod vault_tests;
