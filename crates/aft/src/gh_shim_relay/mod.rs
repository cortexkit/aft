//! Daemon-side relay for governed `gh` bot writes.
//!
//! The `gh` shim runs as an agent's child with no subc credentials, so it
//! cannot reach plexus itself: that requires the attested `reserved:aft`
//! principal that only this daemon holds. The shim instead sends its
//! per-command ticket (see [`crate::gh_shim_ticket`]) and the unchanged
//! governed request envelope to one of the management operations below. The
//! daemon redeems the ticket to the session that spawned the command, calls
//! plexus on that session's behalf, and hands plexus's reply back unchanged.
//!
//! The ticket is the only authority. A session or agent field in the body is
//! never used to pick the speaker, and a missing or unknown ticket is refused
//! before anything leaves the daemon.
//!
//! A bot write speaks for the agent named by the session's scope stamp. The
//! session's tool route must have been bound under a daemon scope that lets
//! providers act as the session's agent (see [`delegating_agent`]). The daemon
//! then opens its plexus route under that same scope and sends the write with
//! no assertion; plexus reads the agent from the stamp on that route and
//! re-checks the scope before each write it sends. A session without such a
//! stamp has no agent to speak as, so its bot write is refused here
//! (`bot_write_requires_scope`) before anything leaves the daemon. The daemon
//! never mints agent assertions.
//!
//! This needs a transport that can open a scoped route
//! ([`RelayTransport::opens_scoped_routes`]). The production transport can:
//! it opens the route with the subc client's scoped open, and never falls back
//! to an unscoped route when that open is refused.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Relay a governed bot write. Params: `{ticket, request_nonce, request}`.
pub const BOT_REQUEST_OPERATION: &str = "gh_shim.bot_request";
/// Relay plexus's repository-to-agent binding read for the shim's version
/// check. Params: `{ticket}`; an `agent_id` the shim still sends is ignored,
/// because the agent is captured from the ticket's authenticated launch scope.
pub const BINDINGS_READ_OPERATION: &str = "gh_shim.bindings_read";
/// Inspect the ticket's daemon-stamped session kind, without contacting Plexus.
pub const SESSION_CONTEXT_OPERATION: &str = "gh_shim.session_context";
/// Relay a credential-free github_read request under the ticket's scoped route.
pub const READ_REQUEST_OPERATION: &str = "gh_shim.read_request";

pub(crate) const PLEXUS_MODULE_ID: &str = "plexus";
const AGENT_BINDINGS_READ_OPERATION: &str = "bindings.read_for_agent";
const RELAY_CALL_TIMEOUT: Duration = Duration::from_secs(20);
const RELAY_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
static RELAY_OPERATIONS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(64)));

pub fn is_relay_operation(operation: &str) -> bool {
    matches!(
        operation,
        BOT_REQUEST_OPERATION
            | BINDINGS_READ_OPERATION
            | SESSION_CONTEXT_OPERATION
            | READ_REQUEST_OPERATION
    )
}

/// Plexus answers a tool call either with its facade reply directly or wrapped
/// in a tool-call result whose first text block holds the reply as JSON.
/// Return the facade reply in both cases.
pub fn facade_reply(value: &Value) -> Value {
    // Direct facade fields are authoritative: a tool-call result may also
    // carry text containing only the inner result, not the whole reply.
    if ["result", "error", "repo_binding_generation"]
        .iter()
        .any(|field| value.get(field).is_some())
    {
        return value.clone();
    }
    let wrapped = value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find_map(|block| block.get("text").and_then(Value::as_str))
        })
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    wrapped.unwrap_or_else(|| value.clone())
}

/// The refusal code in a plexus facade reply, if the reply is a refusal.
pub fn facade_refusal_code(reply: &Value) -> Option<&str> {
    reply
        .get("result")
        .and_then(|result| result.get("refusal_code"))
        .and_then(Value::as_str)
}

/// How a call to prefrontal or plexus failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportError {
    /// The daemon refused an epoch-pinned route open with SCOPE_ENDED.
    ScopeEnded {
        selector: ScopeSelector,
        message: String,
    },
    /// The target answered with a subc Error frame carrying this code.
    Refused { code: String, message: String },
    /// The target refused with this code for a reason expected to clear, so
    /// the same request may be tried again shortly.
    RefusedRetryable { code: String, message: String },
    /// Nothing was sent: no connection, no route, or the module is absent.
    Unavailable(String),
    /// The request was sent but no reply arrived; it may have executed.
    OutcomeUnknown(String),
}

/// The daemon scope stamped on a route at bind.
pub type ScopeStamp = subc_protocol::scope::ScopeStamp;
/// The scope a route open asks to be admitted under.
pub type ScopeSelector = subc_protocol::scope::ScopeSelector;

/// The agent a stamp lets a provider act as, if any.
///
/// This is exactly the rule plexus applies to a bot write that carries no
/// assertion: the scope must delegate, its owner must be one the daemon trusts
/// to set agent identity (`owner_authorized`), and it must name an agent. Any
/// other stamp grants plexus no one to act as, so a write without an assertion
/// would be refused (`assertion_absent`).
pub fn delegating_agent(stamp: &ScopeStamp) -> Option<&str> {
    if stamp.attributes.delegates && stamp.owner_authorized {
        stamp.attributes.agent_id.as_deref()
    } else {
        None
    }
}

/// The selector that opens a route under the scope a stamp names, pinned to
/// the stamp's epoch so a newer session under the same ref is not picked up.
fn scope_selector(stamp: &ScopeStamp) -> ScopeSelector {
    ScopeSelector::new(
        stamp.owner.clone(),
        stamp.scope_ref.clone(),
        Some(stamp.scope_epoch),
    )
}

/// The scope stamps of AFT's bound tool routes, keyed by the session each
/// route was bound for. The subc frame loop copies them when a relay request
/// arrives, because the relay runs on its own task and cannot read the loop's
/// route table.
#[derive(Clone, Debug, Default)]
pub struct RouteScopes {
    routes: Vec<(String, Option<ScopeStamp>)>,
    origins: Vec<(
        crate::gh_shim_speech::model::RouteOrigin,
        Option<ScopeStamp>,
    )>,
    pub(crate) speech: Option<Arc<crate::gh_shim_speech::service::Service>>,
}

impl RouteScopes {
    pub(crate) fn push_origin(
        &mut self,
        origin: crate::gh_shim_speech::model::RouteOrigin,
        stamp: Option<ScopeStamp>,
    ) {
        self.origins.push((origin, stamp));
    }
    pub(crate) fn origin_live(
        &self,
        origin: Option<&crate::gh_shim_speech::model::RouteOrigin>,
        stamp: &ScopeStamp,
    ) -> bool {
        let Some(origin) = origin else { return false };
        self.origins
            .iter()
            .any(|(key, value)| key == origin && value.as_ref() == Some(stamp))
    }
    /// Record one bound tool route and the stamp the daemon put on it.
    pub fn push(&mut self, session: &str, stamp: Option<ScopeStamp>) {
        self.routes.push((session.to_string(), stamp));
    }

    /// The stamp of the session's tool route, when there is exactly one answer.
    ///
    /// A ticket names a session, not a route, and a session can have more than
    /// one route bound at a time. When those routes carry different stamps, or
    /// one of them carries none, the relay cannot tell which route launched the
    /// command, so this returns `None` and the write is refused as unscoped,
    /// which is what an unstamped session gets.
    pub fn for_session(&self, session: &str) -> Option<&ScopeStamp> {
        let mut stamps = self
            .routes
            .iter()
            .filter(|(bound, _)| bound == session)
            .map(|(_, stamp)| stamp.as_ref());
        let first = stamps.next()??;
        stamps.all(|other| other == Some(first)).then_some(first)
    }
}

/// The outbound call the relay makes. Production uses the daemon's own
/// `reserved:aft` subc connection; tests use fakes that record the bodies.
pub trait RelayTransport: Send + Sync {
    /// Validate the immutable ticket-time repository against the signed local
    /// manifest. Neither request data nor later Git config can select a target.
    fn read_repository(&self, ticket: &crate::gh_shim_ticket::Redeemed) -> Result<String, String> {
        crate::gh_shim::verified_worker_read_repository(ticket.read_repository.as_deref())
    }
    /// Send `body` to plexus's tool provider and return the parsed JSON reply.
    /// With `scope`, the route to plexus must be opened under that scope, so
    /// plexus sees its stamp; without one the route is unscoped.
    fn plexus(
        &self,
        project_root: &str,
        session: &str,
        scope: Option<&ScopeSelector>,
        body: Value,
    ) -> impl std::future::Future<Output = Result<Value, TransportError>> + Send;
    /// Whether [`RelayTransport::plexus`] can open its route under a scope.
    /// A bot write is only sent when this is true: plexus refuses a write
    /// without an assertion on an unscoped route.
    fn opens_scoped_routes(&self) -> bool;
}

/// What the daemon sends back to the shim. `ok` replies carry plexus's reply
/// unchanged; refusals carry `{refusal_code, stage, message}`.
#[derive(Clone, Debug, PartialEq)]
pub struct RelayReply {
    pub ok: bool,
    pub data: Value,
}

impl RelayReply {
    pub(crate) fn task_refused(refusal: crate::gh_shim_speech::Refusal) -> Self {
        Self {
            ok: false,
            data: refusal.wire_data(),
        }
    }
    fn relayed(reply: Value) -> Self {
        Self {
            ok: true,
            data: reply,
        }
    }

    fn refused(code: &str, stage: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: json!({
                "refusal_code": code,
                "stage": stage,
                "message": message.into(),
            }),
        }
    }

    fn outcome_label(&self) -> String {
        if self.ok {
            return match facade_refusal_code(&facade_reply(&self.data)) {
                Some(code) => code.to_string(),
                None => "completed".to_string(),
            };
        }
        self.data
            .get("refusal_code")
            .and_then(Value::as_str)
            .unwrap_or("refused")
            .to_string()
    }
}

/// One relay request as seen by the relay core.
pub struct RelayCall<'a> {
    pub operation: &'a str,
    pub params: &'a Value,
    /// Whether the management route this arrived on belongs to a first-party
    /// principal. Anything else is refused even with a valid ticket.
    pub first_party: bool,
    /// Scope stamps of the daemon's bound tool routes when the request arrived.
    pub route_scopes: &'a RouteScopes,
}

mod reservation;

/// How a call reached plexus, for the relay log line: under the session's
/// scope stamp, or (for the binding read) on an unscoped route.
const PATH_STAMP: &str = "stamp";
const PATH_UNSCOPED: &str = "unscoped";

/// Run one relay operation. `log` receives exactly one line per call carrying
/// the redeemed session, task id, nonce, action, repository, the path the
/// call took (`stamp` or `unscoped`, `-` when refused before one was chosen)
/// and the outcome; the ticket and the scope never appear in it.
pub async fn relay<T: RelayTransport>(
    transport: &T,
    call: RelayCall<'_>,
    log: &(dyn Fn(&str) + Send + Sync),
) -> RelayReply {
    relay_inner(
        transport,
        call,
        log,
        #[cfg(test)]
        None,
    )
    .await
}

// This fixture exercises the existing awaited scoped transport, not posting
// admission or the SDK reserved-send API. Authority tests use relay.
#[cfg(test)]
async fn relay_transport_contract<T: RelayTransport>(
    transport: &T,
    call: RelayCall<'_>,
    log: &(dyn Fn(&str) + Send + Sync),
) -> RelayReply {
    let stamp = call
        .params
        .get("ticket")
        .and_then(Value::as_str)
        .and_then(crate::gh_shim_ticket::redeem)
        .and_then(|ticket| call.route_scopes.for_session(&ticket.session_id).cloned());
    relay_inner(transport, call, log, Some(stamp)).await
}

async fn relay_inner<T: RelayTransport>(
    transport: &T,
    call: RelayCall<'_>,
    log: &(dyn Fn(&str) + Send + Sync),
    #[cfg(test)] wire_fixture: Option<Option<ScopeStamp>>,
) -> RelayReply {
    let params = call.params;
    let nonce = params
        .get("request_nonce")
        .and_then(Value::as_str)
        .unwrap_or("");
    let request = params.get("request");
    let action = match call.operation {
        BINDINGS_READ_OPERATION => "bindings.read",
        _ => request
            .and_then(|request| request.get("action").or_else(|| request.get("verb")))
            .and_then(Value::as_str)
            .unwrap_or("-"),
    };
    let repository = request
        .and_then(|request| request.get("repository"))
        .and_then(Value::as_str)
        .unwrap_or("-");

    let mut session = "-".to_string();
    let mut task = "-".to_string();
    let mut path = "-";
    let reply = 'reply: {
        if !is_relay_operation(call.operation) {
            break 'reply RelayReply::refused(
                "operation_unknown",
                "admission",
                "the gh shim relay does not serve this operation",
            );
        }
        if !call.first_party {
            break 'reply RelayReply::refused(
                "untrusted_principal",
                "admission",
                "gh shim relay is only served to first-party principals",
            );
        }
        let Some(ticket) = params
            .get("ticket")
            .and_then(Value::as_str)
            .filter(|ticket| !ticket.is_empty())
        else {
            break 'reply RelayReply::refused(
                "ticket_absent",
                "ticket",
                "no agent session is attached to this command",
            );
        };
        let memory_ticket = crate::gh_shim_ticket::redeem(ticket);
        let durable_background =
            crate::gh_shim_ticket::speech_for_ticket(ticket).is_some_and(|speech| {
                speech.background.load(std::sync::atomic::Ordering::SeqCst)
                    && speech.stored.load(std::sync::atomic::Ordering::SeqCst)
            });
        if call.operation == BOT_REQUEST_OPERATION
            && (memory_ticket.is_none() || durable_background)
        {
            if let (Some(service), Some(digest), Some(nonce), Some(request)) = (
                call.route_scopes.speech.as_ref(),
                crate::gh_shim_ticket::speech_digest(ticket),
                params
                    .get("request_nonce")
                    .and_then(Value::as_str)
                    .filter(|nonce| !nonce.is_empty()),
                params.get("request").filter(|request| request.is_object()),
            ) {
                let payload = blake3::derive_key(
                    "aft-gh-bound-request-v1",
                    &serde_json::to_vec(request).expect("a JSON value serializes"),
                );
                match service.store.legacy_replay_for_digest(&digest, nonce, &payload) {
                    Ok(true) => break 'reply RelayReply::refused("outcome_unknown", "plexus", "this GitHub write was already queued and its outcome cannot be confirmed; do not repeat it with a new request key"),
                    Ok(false) => {},
                    Err(_) => break 'reply RelayReply::refused("outcome_unknown", "plexus", "AFT could not check this GitHub write's prior dispatch state; do not repeat it with a new request key"),
                }
            }
        }
        // Keep a previously queued write outcome-unknown before admitting a
        // retry. A stored ticket digest alone never authorizes a write.
        if memory_ticket.is_none() {
            let cold_operation = call.operation == BOT_REQUEST_OPERATION
                || (call.operation == BINDINGS_READ_OPERATION
                    && params.get("purpose").and_then(Value::as_str) == Some("speech"));
            let recognized = match (
                cold_operation,
                call.route_scopes.speech.as_ref(),
                crate::gh_shim_ticket::speech_digest(ticket),
            ) {
                (true, Some(service), Some(digest)) => {
                    match service.store.recognizes_ticket_digest(&digest) {
                        Ok(recognized) => recognized,
                        Err(error) => break 'reply RelayReply::task_refused(error),
                    }
                }
                _ => false,
            };
            if !recognized {
                break 'reply RelayReply::refused(
                    "ticket_unknown",
                    "ticket",
                    "the command's session ticket is not live; it ended or was never issued",
                );
            }
        }
        let redeemed = match memory_ticket {
            Some(redeemed) => redeemed,
            None if call.operation == BOT_REQUEST_OPERATION
                || (call.operation == BINDINGS_READ_OPERATION
                    && params.get("purpose").and_then(Value::as_str) == Some("speech")) =>
            {
                let Some(service) = call.route_scopes.speech.as_ref() else {
                    break 'reply RelayReply::refused(
                        "ticket_unknown",
                        "ticket",
                        "the command's session ticket is not live; it ended or was never issued",
                    );
                };
                if !service.try_serve() {
                    break 'reply RelayReply::task_refused(crate::gh_shim_speech::Refusal::new(
                        crate::gh_shim_speech::Code::AuthorityUnavailable,
                        "serving_lock_unavailable",
                    ));
                }
                match crate::gh_shim_ticket::redeem_speech_cold(ticket, service) {
                    Ok(redeemed) => redeemed,
                    Err(error) => break 'reply RelayReply::task_refused(error),
                }
            }
            None => {
                break 'reply RelayReply::refused(
                    "ticket_unknown",
                    "ticket",
                    "the command's session ticket is not live; it ended or was never issued",
                )
            }
        };
        session = redeemed.session_id.clone();
        task = if redeemed.task_id.is_empty() {
            "-".to_string()
        } else {
            redeemed.task_id.clone()
        };
        match call.operation {
            READ_REQUEST_OPERATION => {
                let stamp = call.route_scopes.for_session(&redeemed.session_id);
                let (reply, taken) = read_request(transport, &redeemed, stamp, params).await;
                path = taken;
                reply
            }
            SESSION_CONTEXT_OPERATION => session_context(
                &redeemed,
                redeemed
                    .lifecycle_scope
                    .as_ref()
                    .or_else(|| call.route_scopes.for_session(&redeemed.session_id)),
            ),
            BINDINGS_READ_OPERATION => {
                path = PATH_UNSCOPED;
                bindings_read(transport, &redeemed).await
            }
            BOT_REQUEST_OPERATION => {
                let captured_background = needs_captured_background(&redeemed, ticket);
                let stamp = if captured_background {
                    bot_authority(&redeemed, call.route_scopes)
                } else if let Some(launch) = redeemed.launch.as_ref().filter(|launch| {
                    launch.run.is_some()
                        && launch.required_scopes().into_iter().all(|(role, scope)| {
                            let origin = if role == crate::gh_shim_speech::ScopeRole::Head {
                                launch.head_origin.as_ref()
                            } else {
                                launch.origin.as_ref()
                            };
                            call.route_scopes.origin_live(origin, scope)
                        })
                }) {
                    Some(launch.authority.clone())
                } else {
                    call.route_scopes.for_session(&redeemed.session_id).cloned()
                };
                // A route can prove the launching authority is still present,
                // but it cannot replace the authority captured in the ticket.
                let check_launch =
                    !captured_background && !worker_write_refused(&redeemed, stamp.as_ref());
                #[cfg(test)]
                let check_launch = check_launch && wire_fixture.is_none();
                if check_launch {
                    use crate::gh_shim_speech::{Code, Refusal};
                    let Some(launch) = &redeemed.launch else {
                        break 'reply RelayReply::task_refused(Refusal::new(
                            Code::AuthorityUnavailable,
                            "owner_unresolved",
                        ));
                    };
                    if stamp.as_ref() != Some(&launch.authority) {
                        break 'reply RelayReply::task_refused(Refusal::new(
                            Code::IdentityConflict,
                            "request_mismatch",
                        ));
                    }
                }
                #[cfg(test)]
                let stamp = wire_fixture.clone().unwrap_or(stamp);
                let (reply, taken) = bot_request(
                    transport,
                    &redeemed,
                    stamp.as_ref(),
                    params,
                    call.route_scopes,
                    #[cfg(test)]
                    wire_fixture.is_some(),
                )
                .await;
                path = taken;
                reply
            }
            _ => RelayReply::refused("operation_unknown", "admission", "unknown relay operation"),
        }
    };

    log(&format!(
        "gh_shim relay: op={} session={} task={} nonce={} action={} repository={} path={} outcome={}",
        call.operation.escape_debug(),
        session.escape_debug(),
        task.escape_debug(),
        if nonce.is_empty() { "-" } else { nonce }.escape_debug(),
        action.escape_debug(),
        repository.escape_debug(),
        path,
        reply.outcome_label().escape_debug(),
    ));
    reply
}

fn needs_captured_background(redeemed: &crate::gh_shim_ticket::Redeemed, ticket: &str) -> bool {
    if redeemed.launch.is_none() {
        return false;
    }
    // Route presence is a liveness shortcut, never permission to omit the
    // expected bot binding or captured codemode run guard on a mutation.
    crate::gh_shim_ticket::speech_for_ticket(ticket)
        .map(|speech| speech.background.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or_else(|| crate::gh_shim_ticket::redeem(ticket).is_none())
}

fn bot_authority(
    ticket: &crate::gh_shim_ticket::Redeemed,
    routes: &RouteScopes,
) -> Option<ScopeStamp> {
    let _ = routes;
    ticket
        .launch
        .as_ref()
        .map(|launch| launch.authority.clone())
}

/// Match the worker rule used for access grants: parented runs inherit worker
/// policy, while an unparented run belongs to the head.
pub(crate) fn worker_scope(stamp: &ScopeStamp) -> bool {
    use subc_protocol::scope::ScopeKind;
    use subc_protocol::Principal;
    stamp.owner_authorized
        && matches!(&stamp.owner, Principal::Reserved { module_id } if module_id == "prefrontal-core")
        && (stamp.kind == ScopeKind::Worker
            || (stamp.kind == ScopeKind::Ephemeral && stamp.parent.is_some()))
}

fn head_scope(stamp: &ScopeStamp) -> bool {
    use subc_protocol::scope::ScopeKind;
    stamp.owner_authorized
        && matches!(&stamp.owner, subc_protocol::Principal::Reserved {module_id} if module_id == "prefrontal-core")
        && stamp.parent.is_none()
        && matches!(stamp.kind, ScopeKind::Head | ScopeKind::Ephemeral)
}

pub(crate) fn session_context(
    redeemed: &crate::gh_shim_ticket::Redeemed,
    stamp: Option<&ScopeStamp>,
) -> RelayReply {
    match stamp {
        Some(stamp) if worker_scope(stamp) || head_scope(stamp) => RelayReply::relayed(
            json!({"worker": worker_scope(stamp), "head": head_scope(stamp), "repository": redeemed.read_repository}),
        ),
        _ => RelayReply::refused(
            "session_unscoped",
            "scope",
            "the ticket has no unambiguous daemon-stamped session",
        ),
    }
}

pub(crate) async fn read_request<T: RelayTransport>(
    transport: &T,
    redeemed: &crate::gh_shim_ticket::Redeemed,
    stamp: Option<&ScopeStamp>,
    params: &Value,
) -> (RelayReply, &'static str) {
    let request = params.get("request").cloned().unwrap_or(Value::Null);
    let parsed = serde_json::from_value::<crate::gh_shim::read_contract::Request>(request.clone());
    let parsed = match parsed
        .map_err(|error| error.to_string())
        .and_then(|request| {
            request.validate()?;
            Ok(request)
        }) {
        Ok(parsed) => parsed,
        Err(message) => {
            return (
                RelayReply::refused("request_malformed", "request", message),
                "-",
            );
        }
    };
    read_validated_request(transport, redeemed, stamp, &parsed).await
}

/// Check that the typed request targets the repository bound to the redeemed
/// ticket and require an authenticated worker scope before relaying it.
pub(crate) async fn read_validated_request<T: RelayTransport>(
    transport: &T,
    redeemed: &crate::gh_shim_ticket::Redeemed,
    stamp: Option<&ScopeStamp>,
    parsed: &crate::gh_shim::read_contract::Request,
) -> (RelayReply, &'static str) {
    if let Err(message) = parsed.validate() {
        return (
            RelayReply::refused("request_malformed", "request", message),
            "-",
        );
    }
    let Some(stamp) = stamp.filter(|stamp| worker_scope(stamp) && transport.opens_scoped_routes())
    else {
        return (
            RelayReply::refused(
                "agent_identity_required",
                "scope",
                "governed reads require the ticket's authenticated worker scope",
            ),
            "-",
        );
    };
    let bound = match transport.read_repository(redeemed) {
        Ok(bound) => bound,
        Err(message) => {
            return (
                RelayReply::refused("repository_not_visible", "binding", message),
                "-",
            )
        }
    };
    if parsed.repository() != bound {
        return (
            RelayReply::refused(
                "repository_not_visible",
                "binding",
                "worker reads are limited to the ticket session's bound checkout repository",
            ),
            "-",
        );
    }
    // The selector reopens the redeemed session's scope at the same epoch.
    // Plexus receives its own target-specific stamp, including worker delegation;
    // AFT neither reads nor reconstructs the bot identity in that stamp.
    let selector = scope_selector(stamp);
    let body = serde_json::to_value(subc_protocol::tool_call::ToolCallRequest {
        name: "github_read".into(),
        arguments: serde_json::to_value(parsed).expect("a validated read request serializes"),
        tool_call_id: None,
        progress_token: None,
        call_key: None,
        schema_pin: None,
        origin: None,
        preset: None,
        output_refs: None,
    })
    .expect("a read tool call serializes to JSON");
    let reply = match transport
        .plexus(
            &redeemed.project_root,
            &redeemed.session_id,
            Some(&selector),
            body,
        )
        .await
    {
        Ok(reply) => RelayReply::relayed(reply),
        Err(error) => transport_refusal("plexus", error),
    };
    (reply, PATH_STAMP)
}

/// Read plexus's repository-to-agent bindings for the shim's version check.
async fn bindings_read<T: RelayTransport>(
    transport: &T,
    redeemed: &crate::gh_shim_ticket::Redeemed,
) -> RelayReply {
    let agent = match redeemed.launch.as_ref().map(|launch| launch.agent()) {
        Some(Ok(agent)) => agent,
        _ => {
            return RelayReply::task_refused(crate::gh_shim_speech::Refusal::new(
                crate::gh_shim_speech::Code::AuthorityUnavailable,
                "start_delegation_missing",
            ))
        }
    };
    let body = agent_bindings_call(agent);
    match transport
        .plexus(&redeemed.project_root, &redeemed.session_id, None, body)
        .await
    {
        Ok(reply) => RelayReply::relayed(reply),
        Err(error) => transport_refusal("plexus", error),
    }
}

fn worker_write_refused(
    redeemed: &crate::gh_shim_ticket::Redeemed,
    stamp: Option<&ScopeStamp>,
) -> bool {
    let admitted_run = redeemed
        .launch
        .as_ref()
        .filter(|launch| launch.validate().is_ok())
        .and_then(|launch| launch.run.as_ref());
    // A parented ephemeral is a worker unless it is the exact program admitted
    // with a validated Head launch. A Worker-kind stamp is never exempt.
    [
        stamp,
        redeemed.lifecycle_scope.as_ref(),
        redeemed.launch.as_ref().map(|launch| &launch.authority),
    ]
    .into_iter()
    .flatten()
    .any(|scope| match scope.kind {
        subc_protocol::scope::ScopeKind::Worker => true,
        subc_protocol::scope::ScopeKind::Ephemeral if scope.parent.is_some() => {
            Some(scope) != admitted_run
        }
        _ => false,
    })
}

/// Relay one bot write, returning plexus's reply and the path it took.
async fn bot_request<T: RelayTransport>(
    transport: &T,
    redeemed: &crate::gh_shim_ticket::Redeemed,
    stamp: Option<&ScopeStamp>,
    params: &Value,
    routes: &RouteScopes,
    #[cfg(test)] wire_contract: bool,
) -> (RelayReply, &'static str) {
    if worker_write_refused(redeemed, stamp) {
        return (
            RelayReply::refused(
                crate::gh_shim_speech::WORKER_WRITE_CODE,
                "worker_policy",
                crate::gh_shim_speech::WORKER_WRITE_REFUSED,
            ),
            "-",
        );
    }
    let Some(nonce) = params
        .get("request_nonce")
        .and_then(Value::as_str)
        .filter(|nonce| !nonce.is_empty())
    else {
        return (
            RelayReply::refused("request_malformed", "request", "request_nonce is required"),
            "-",
        );
    };
    let Some(request) = params.get("request").filter(|request| request.is_object()) else {
        return (
            RelayReply::refused("request_malformed", "request", "request must be an object"),
            "-",
        );
    };
    // The agent is the one the shim's signed manifest binds to the target
    // repository. It must match the agent the session's scope acts as; the
    // session comes from the ticket alone, and plexus re-checks the agent
    // against its own bindings.
    let Some(agent_id) = request
        .get("metadata")
        .and_then(|metadata| metadata.get("agent_id"))
        .and_then(Value::as_str)
        .filter(|agent| !agent.is_empty())
    else {
        return (
            RelayReply::refused(
                "request_malformed",
                "request",
                "request.metadata.agent_id is required",
            ),
            "-",
        );
    };

    // The session's scope must let plexus act as its agent, and the route to
    // plexus must be opened under that scope so plexus sees it. Otherwise the
    // write has no agent to speak as, so it is refused with
    // `bot_write_requires_scope`.
    let Some((stamp_agent, _stamp)) = stamp
        .filter(|_| transport.opens_scoped_routes())
        .and_then(|stamp| delegating_agent(stamp).map(|agent| (agent, stamp)))
    else {
        let mut refusal = RelayReply::refused(
            "bot_write_requires_scope",
            "scope",
            "this session is not bound under its agent's scope, so it has no bot \
             identity to speak as; bot comments need a scoped session. Restart the \
             host session (it binds under its agent's scope when it starts), or ask \
             the operator",
        );
        refusal.data["repository"] = request.get("repository").cloned().unwrap_or(Value::Null);
        refusal.data["action"] = request
            .get("action")
            .or_else(|| request.get("verb"))
            .cloned()
            .unwrap_or(Value::Null);
        return (refusal, "-");
    };

    // Plexus takes the agent from the stamp, so a request naming another
    // agent would speak as the wrong bot. Refuse it with
    // `agent_identity_conflict`, the code plexus uses for this mismatch.
    if stamp_agent != agent_id {
        return (
            RelayReply::refused(
                "agent_identity_conflict",
                "scope",
                format!(
                    "the session's scope acts as agent {stamp_agent}, but the request names agent {agent_id}"
                ),
            ),
            PATH_STAMP,
        );
    }
    // Protocol fixtures deliberately omit authority capture. Production writes
    // of every ticket kind use the captured policy and repository ceiling below.
    #[cfg(test)]
    if wire_contract {
        let session = redeemed.session_id.as_str();
        let selector = scope_selector(_stamp);
        let body = github_call(json!({"op":"bot_request","request_nonce":nonce,"request":request}));
        let response_generation = routes.speech.as_ref().map(|service| service.generation());
        let route_role = if redeemed
            .launch
            .as_ref()
            .is_some_and(|launch| launch.run.as_ref() == Some(_stamp))
        {
            crate::gh_shim_speech::ScopeRole::Run
        } else if redeemed
            .launch
            .as_ref()
            .is_some_and(|launch| launch.run.is_some())
        {
            crate::gh_shim_speech::ScopeRole::Head
        } else {
            crate::gh_shim_speech::ScopeRole::Authority
        };
        let reply = match transport
            .plexus(&redeemed.project_root, session, Some(&selector), body)
            .await
        {
            Ok(reply) => {
                if let (Some(service), Some(generation)) = (&routes.speech, &response_generation) {
                    let facade = facade_reply(&reply);
                    let result = facade.get("result").unwrap_or(&facade);
                    if crate::gh_shim_speech::partner::scope_end_role(result, route_role, false)
                        .is_some()
                    {
                        let _ = service.record_rpc_end(
                            _stamp,
                            route_role,
                            &selector,
                            generation,
                            crate::gh_shim_speech::scope::ScopedRpcEndSource::Provider,
                        );
                    }
                }
                RelayReply::relayed(reply)
            }
            Err(error) => {
                if let (
                    TransportError::ScopeEnded { selector, .. },
                    Some(service),
                    Some(generation),
                ) = (&error, &routes.speech, &response_generation)
                {
                    let _ = service.record_rpc_end(
                        _stamp,
                        route_role,
                        selector,
                        generation,
                        crate::gh_shim_speech::scope::ScopedRpcEndSource::RouteOpen,
                    );
                }
                transport_refusal("plexus", error)
            }
        };
        return (reply, PATH_STAMP);
    }
    let reply = match post_captured(redeemed, params, routes, nonce, request).await {
        Ok(reply) => reply,
        Err(error) => RelayReply::task_refused(error),
    };
    (reply, PATH_STAMP)
}

async fn post_captured(
    redeemed: &crate::gh_shim_ticket::Redeemed,
    params: &Value,
    routes: &RouteScopes,
    nonce: &str,
    request: &Value,
) -> Result<RelayReply, crate::gh_shim_speech::Refusal> {
    use crate::gh_shim_speech::partner;
    use crate::gh_shim_speech::{Code, Refusal, ScopeRole};
    let probe_deadline = tokio::time::Instant::now() + Duration::from_millis(9_750);
    let invalid = || Refusal::new(Code::AuthorityUnavailable, "snapshot_invalid");
    let launch = redeemed.launch.as_ref().ok_or_else(invalid)?;
    launch.validate()?;
    let service = routes
        .speech
        .as_ref()
        .ok_or_else(|| Refusal::new(Code::AuthorityUnavailable, "not_serving_mode"))?;
    let _runtime_work = service.begin_post()?;
    service.check_pending(None, launch, &redeemed.session_id, &redeemed.task_id)?;
    if !service.try_serve() {
        return Err(Refusal::new(
            Code::AuthorityUnavailable,
            "serving_lock_unavailable",
        ));
    }
    service.require_posting_guards(launch.run.is_some())?;
    let ticket = params
        .get("ticket")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let warm = crate::gh_shim_ticket::speech_for_ticket(ticket);
    let stored = if let Some(warm) = &warm {
        if let Some(error) = warm
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            return Err(error);
        }
        if !warm.stored.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Refusal::new(
                Code::AuthorityUnavailable,
                "start_policy_missing",
            ));
        }
        if warm.prepared.context.service.store.path() != service.store.path() {
            return Err(invalid());
        }
        service.store.get(&warm.incarnation)?
    } else {
        service.store.find_digest(
            &crate::gh_shim_ticket::speech_digest(ticket).ok_or_else(invalid)?,
            crate::gh_shim_speech::service::run_nonce().unwrap_or_default(),
        )?
    };
    if &stored.record.launch != launch {
        return Err(invalid());
    }
    let foreground = warm
        .as_ref()
        .is_some_and(|warm| !warm.background.load(std::sync::atomic::Ordering::SeqCst));
    if foreground
        && !routes.origin_live(
            launch.origin.as_ref(),
            launch.run.as_ref().unwrap_or(&launch.authority),
        )
    {
        return Err(Refusal::new(Code::AuthorityUnavailable, "owner_unresolved"));
    }
    for (role, scope) in launch.required_scopes() {
        let origin = if role == ScopeRole::Head {
            launch.head_origin.as_ref()
        } else {
            launch.origin.as_ref()
        };
        if !routes.origin_live(origin, scope) {
            service.check_scope(scope, role, probe_deadline).await?;
        }
        service.check_pending(
            Some(&stored.record.incarnation),
            launch,
            &redeemed.session_id,
            &redeemed.task_id,
        )?;
        service.store.get(&stored.record.incarnation)?;
    }
    let start = stored
        .record
        .policy
        .as_ref()
        .ok_or_else(|| Refusal::new(Code::AuthorityUnavailable, "start_policy_missing"))?;
    let (current, desired) = service
        .register_accepted_policy()?
        .ok_or_else(|| Refusal::new(Code::AuthorityUnavailable, "current_manifest_unverifiable"))?;
    let policy_generation = service.store.advance_policy(&current)?;
    let invocation: crate::gh_shim_speech::invocation::Invocation =
        serde_json::from_value(params.get("invocation").cloned().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    crate::gh_shim::validate_speech_policy(start, &current, request, &invocation)?;
    let repo = request
        .get("repository")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?
        .to_ascii_lowercase();
    let initial = stored
        .record
        .bindings
        .get(&repo)
        .ok_or_else(|| Refusal::new(Code::PolicyDenied, "repository_denied"))?;
    let reply = bindings_read_for_service(
        service,
        &redeemed.project_root,
        &redeemed.session_id,
        launch.agent()?,
    )
    .await?;
    service.check_pending(
        Some(&stored.record.incarnation),
        launch,
        &redeemed.session_id,
        &redeemed.task_id,
    )?;
    service.store.get(&stored.record.incarnation)?;
    let current_bindings = crate::gh_shim_speech::service::parse_bindings(
        &desired,
        &facade_reply(&reply),
        launch.agent()?,
    )?;
    let expect = current_bindings.get(&repo).ok_or_else(|| {
        crate::gh_shim_speech::service::binding_refusal(
            Code::IdentityConflict,
            "binding_changed",
            format!(
                "no_binding_row: repository={repo:?} launch_agent={:?}",
                launch.agent().ok()
            ),
        )
    })?;
    if expect.app_handle_id != initial.app_handle_id {
        return Err(crate::gh_shim_speech::service::binding_refusal(Code::IdentityConflict, "binding_changed", format!("binding_handle_mismatch: repository={repo:?} captured_handle={:?} returned_handle={:?}", initial.app_handle_id, expect.app_handle_id)));
    }
    let body =
        partner::BotRequest::new(request.clone(), nonce, expect.clone(), launch.run.as_ref())?;
    let route_role = if launch.run.is_some() {
        ScopeRole::Head
    } else {
        ScopeRole::Authority
    };
    let reserve_generation = service.generation();
    let slot = match service
        .reserve(
            &redeemed.project_root,
            &redeemed.session_id,
            &launch.authority,
            github_call(body.value()),
            probe_deadline,
        )
        .await
    {
        Ok(slot) => slot,
        Err(mut error) => {
            if error.code == Code::ScopeEnded && error.reason == "ended" {
                if error.role.is_some_and(|role| role != route_role) {
                    return Err(Refusal::scoped(
                        Code::ScopeUnknown,
                        "unverifiable",
                        route_role,
                    ));
                }
                if let Err(proof_error) = service.record_rpc_end(
                    &launch.authority,
                    route_role,
                    &scope_selector(&launch.authority),
                    &reserve_generation,
                    crate::gh_shim_speech::scope::ScopedRpcEndSource::RouteOpen,
                ) {
                    if proof_error.code == Code::ScopeUnknown {
                        return Err(proof_error);
                    }
                }
                error.role = Some(route_role);
            }
            return Err(error);
        }
    };
    service.check_pending(
        Some(&stored.record.incarnation),
        launch,
        &redeemed.session_id,
        &redeemed.task_id,
    )?;
    service.store.get(&stored.record.incarnation)?;
    service.store.prepare_request(
        nonce,
        &stored.record.incarnation,
        &body.payload_digest(),
        &body.expect_json(),
    )?;
    let dispatch_generation = service.generation();
    let queued = {
        let _local = service
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        service.check_pending(
            Some(&stored.record.incarnation),
            launch,
            &redeemed.session_id,
            &redeemed.task_id,
        )?;
        let send = || {
            for (role, scope) in launch.required_scopes() {
                let origin = if role == ScopeRole::Head {
                    launch.head_origin.as_ref()
                } else {
                    launch.origin.as_ref()
                };
                if !routes.origin_live(origin, scope) {
                    if tokio::time::Instant::now() >= probe_deadline {
                        return Err(Refusal::scoped(Code::ScopeUnknown, "timeout", role));
                    }
                    service.check_cached_scope(scope, role)?;
                }
            }
            slot.send()
        };
        #[cfg(test)]
        service.test_before_final_gate();
        let queued = if foreground {
            service.store.final_foreground_gate(
                &stored.record.incarnation,
                nonce,
                stored.generation,
                policy_generation,
                &current,
                send,
            )
        } else {
            service.store.final_gate(
                &stored.record.incarnation,
                nonce,
                stored.generation,
                policy_generation,
                &current,
                send,
            )
        };
        queued
    };
    let queued = match queued {
        Ok(queued) => queued,
        Err(crate::db::gh_speech::GateError::BeforeSend(error)) => return Err(error),
        Err(crate::db::gh_speech::GateError::AfterSend) => {
            return Ok(RelayReply::refused(
                "outcome_unknown",
                "plexus",
                "the request was queued but its final database commit failed",
            ))
        }
    };
    let reply = match queued.response().await {
        Ok(reply) => reply,
        Err(crate::gh_shim_speech::partner::QueuedResponseError::NotSent(refusal)) => {
            return Ok(RelayReply::task_refused(refusal));
        }
        Err(crate::gh_shim_speech::partner::QueuedResponseError::Unknown(cause)) => {
            return Ok(RelayReply::refused(
                "outcome_unknown",
                "plexus",
                format!("the queued write returned no verifiable result ({cause})"),
            ))
        }
    };
    let facade = facade_reply(&reply);
    let result = facade.get("result").unwrap_or(&facade);
    if let Some(role) = partner::scope_end_role(result, route_role, launch.run.is_some()) {
        let scope = if role == ScopeRole::Run {
            launch.run.as_ref()
        } else {
            Some(&launch.authority)
        };
        if let Some(scope) = scope {
            let proof = service.record_rpc_end(
                scope,
                role,
                &scope_selector(scope),
                &dispatch_generation,
                crate::gh_shim_speech::scope::ScopedRpcEndSource::Provider,
            );
            if let Err(error) = proof {
                if error.code == Code::ScopeUnknown
                    && !result
                        .get("legs_sent")
                        .and_then(Value::as_array)
                        .is_some_and(|legs| !legs.is_empty())
                {
                    return Err(error);
                }
            }
        }
    } else if facade_refusal_code(&facade)
        .is_some_and(|code| code.split('{').next() == Some("scope_ended"))
    {
        service.observe_unqualified_end(
            launch
                .required_scopes()
                .into_iter()
                .map(|(role, scope)| (role, scope.clone()))
                .collect(),
        );
    }
    // Posted legs and terminal replays are returned intact, even after revocation.
    if let Some(refusal) = partner::map_refusal(
        result,
        if launch.run.is_some() {
            ScopeRole::Head
        } else {
            ScopeRole::Authority
        },
    ) {
        return Ok(RelayReply::task_refused(refusal));
    }
    Ok(RelayReply::relayed(reply))
}

async fn bindings_read_for_service(
    service: &crate::gh_shim_speech::service::Service,
    project: &str,
    session: &str,
    agent: &str,
) -> Result<Value, crate::gh_shim_speech::Refusal> {
    service.read_bindings(project, session, agent).await
}

// A connection-selected read returns only that handle's repositories. Select by
// the authenticated registry agent instead, including all of its handles.
fn agent_bindings_call(agent: &str) -> Value {
    github_call(json!({"op": AGENT_BINDINGS_READ_OPERATION, "agent_id": agent}))
}

/// The body of AFT's call to plexus's `github` tool. AFT is the caller here:
/// it is not relaying a tool call some other principal made, and the agent a
/// write speaks for reaches plexus through the scope stamp. So the request
/// carries no `origin`, which keeps the body the
/// plain `{name, arguments}` shape plexus already reads.
fn github_call(arguments: Value) -> Value {
    serde_json::to_value(subc_protocol::tool_call::ToolCallRequest {
        name: "github".into(),
        arguments,
        tool_call_id: None,
        progress_token: None,
        call_key: None,
        schema_pin: None,
        origin: None,
        preset: None,
        output_refs: None,
    })
    .expect("a tool call serializes to JSON")
}

fn transport_refusal(stage: &str, error: TransportError) -> RelayReply {
    match error {
        TransportError::ScopeEnded { message, .. } => {
            RelayReply::refused(subc_protocol::error_codes::SCOPE_ENDED, stage, message)
        }
        TransportError::Refused { code, message } => RelayReply::refused(&code, stage, message),
        TransportError::RefusedRetryable { code, message } => {
            let mut reply = RelayReply::refused(&code, stage, message);
            reply.data["retryable"] = Value::Bool(true);
            reply
        }
        TransportError::Unavailable(message) => {
            RelayReply::refused("relay_unavailable", stage, message)
        }
        TransportError::OutcomeUnknown(message) => {
            RelayReply::refused("outcome_unknown", stage, message)
        }
    }
}

/// The daemon's relay state: the outbound transport.
pub struct GhRelay {
    pub transport: SubcRelayTransport,
}

impl GhRelay {
    pub fn new(connection_file: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            transport: SubcRelayTransport::new(connection_file),
        })
    }

    pub(crate) async fn read_bindings(
        &self,
        project: &str,
        session: &str,
        agent: &str,
    ) -> Result<Value, TransportError> {
        self.transport
            .plexus(project, session, None, agent_bindings_call(agent))
            .await
    }
}

/// Route-open refusals of a scoped route that end the call at once. The subc
/// client returns these four without retrying. Each goes back to the shim
/// under its own code, the way plexus's own scope refusals do, and the write
/// is never retried another way.
///
/// This is also how a scope the daemon closed stays closed. The daemon's
/// `route.closed` notice does not say which route it closed, so the relay
/// keeps no record of ended scopes. Each relay call opens its route afresh and
/// never reopens it within the call; the next call's open under a removed
/// scope is refused by the daemon (`scope_not_live`, `scope_ended`), and that
/// refusal is relayed as it is.
const TERMINAL_SCOPE_OPEN_REFUSALS: [&str; 4] = [
    subc_protocol::error_codes::SCOPE_NOT_LIVE,
    subc_protocol::error_codes::SCOPE_ENDED,
    subc_protocol::error_codes::SCOPE_EPOCH_REQUIRED,
    subc_protocol::error_codes::SCOPE_NOT_CARRIER,
];

/// Route-open refusals of a scoped route that the subc client retries inside
/// the call's deadline: the scope's owner has not synced since the daemon
/// started, or the scope changed while the open was in flight. When they
/// outlast the deadline the shim gets the code, marked retryable, rather than
/// a generic `relay_unavailable`.
const RETRYABLE_SCOPE_OPEN_REFUSALS: [&str; 2] = [
    subc_protocol::error_codes::SCOPE_NOT_SYNCED,
    subc_protocol::error_codes::SCOPE_CHANGED,
];

/// The typed refusal for a failed route open, when the daemon refused it for
/// one of the scope reasons above.
fn scope_open_refusal(
    error: &subc_client_rs::CallError,
    selector: &ScopeSelector,
) -> Option<TransportError> {
    let body = error.route_open_refusal()?;
    let code = body.code.as_str();
    if code == subc_protocol::error_codes::SCOPE_ENDED {
        Some(TransportError::ScopeEnded {
            selector: selector.clone(),
            message: body.message.clone(),
        })
    } else if TERMINAL_SCOPE_OPEN_REFUSALS.contains(&code) {
        Some(TransportError::Refused {
            code: body.code.clone(),
            message: body.message.clone(),
        })
    } else if RETRYABLE_SCOPE_OPEN_REFUSALS.contains(&code) {
        Some(TransportError::RefusedRetryable {
            code: body.code.clone(),
            message: body.message.clone(),
        })
    } else {
        None
    }
}

/// The relay calls currently using each route, so a route shared by
/// overlapping calls is closed only when the last of them finishes.
///
/// The subc client caches a route per target, bind identity and scope, and
/// hands the same route to every open with that key. Two overlapping relay
/// calls for one session therefore share a route, and closing it when the
/// first call finishes would fail the other's request in flight. The client
/// has no uncached scoped open, so the count lives here.
#[derive(Default)]
struct RouteLeases {
    /// Per route key: how many calls hold it, and every handle they were given.
    /// A route that died mid-call is replaced by a fresh one under the same
    /// key, so one key can have handed out more than one handle.
    holders: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<LeaseHolders>>>>,
    #[cfg(test)]
    pause: std::sync::Mutex<Option<Arc<LeaseTestPause>>>,
}

#[cfg(test)]
#[derive(Default)]
struct LeaseTestPause {
    acquired: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    finished: tokio::sync::Notify,
}

#[derive(Default)]
struct LeaseHolders {
    count: usize,
    handles: Vec<subc_client_rs::RouteHandle>,
}

impl RouteLeases {
    /// Count a call as holding the route under `key`, before it opens it.
    async fn acquire(&self, key: &str) {
        let entry = {
            let mut holders = self.holders.lock().await;
            Arc::clone(holders.entry(key.to_string()).or_default())
        };
        entry.lock().await.count += 1;
    }

    /// Record the handle the client gave a holder of `key`.
    async fn opened(&self, key: &str, route: subc_client_rs::RouteHandle) {
        let entry = self.holders.lock().await.get(key).cloned();
        if let Some(entry) = entry {
            let mut holder = entry.lock().await;
            if !holder.handles.contains(&route) {
                holder.handles.push(route);
            }
        }
    }

    /// Stop counting a call as holding `key`. The last holder closes every
    /// handle given out under it, through the handle: the client's close by
    /// route key only finds unscoped routes. The close holds only the per-key
    /// mutex: callers for the same key wait for its cached handles to close,
    /// while unrelated keys can still acquire and release their routes.
    async fn release(&self, key: &str, consumer: &subc_client_rs::SubcConsumer) {
        self.release_with(key, |handles| async move {
            let _ = tokio::time::timeout(RELAY_CLEANUP_TIMEOUT, async {
                for route in handles {
                    let _ = consumer
                        .close_handle(&route, subc_client_rs::CloseRouteOptions::default())
                        .await;
                }
            })
            .await;
        })
        .await;
    }

    async fn release_with<F, Fut>(&self, key: &str, close: F)
    where
        F: FnOnce(Vec<subc_client_rs::RouteHandle>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let entry = self.holders.lock().await.get(key).cloned();
        let Some(entry) = entry else {
            return;
        };
        let mut holder = entry.lock().await;
        holder.count = holder
            .count
            .checked_sub(1)
            .expect("each acquired route lease is released once");
        if holder.count > 0 {
            return;
        }
        close(std::mem::take(&mut holder.handles)).await;
        let mut holders = self.holders.lock().await;
        // With the map mutex held, only this releaser and the map own the entry
        // unless an acquire has cloned it and is waiting for the per-key mutex.
        // Keep that entry so the waiter cannot open a detached route lease.
        if Arc::strong_count(&entry) == 2 {
            holders.remove(key);
        }
    }
}

/// A failed outbound call, and whether the connection should be reopened
/// before the next one.
struct CallFailure {
    error: TransportError,
    reconnect: bool,
}

struct RequestCancellation(tokio::sync::watch::Sender<bool>);
impl Drop for RequestCancellation {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// Open a route to `target` (or share the one another call holds), send `body`
/// on it, and release it again.
///
/// With `scope`, the route is opened under that scope with the subc client's
/// scoped open. A refused scoped open is returned as it is and never retried
/// as an unscoped open: that would send the write without the stamp that
/// names its agent.
async fn route_request(
    consumer: &Arc<subc_client_rs::SubcConsumer>,
    leases: &Arc<RouteLeases>,
    target: subc_protocol::RouteTarget,
    identity: subc_protocol::BindIdentity,
    scope: Option<&ScopeSelector>,
    mut options: subc_client_rs::CallOptions,
    body: &Value,
) -> Result<Value, CallFailure> {
    let bytes = serde_json::to_vec(body).map_err(|error| CallFailure {
        error: TransportError::Unavailable(error.to_string()),
        reconnect: false,
    })?;
    let key =
        route_lease_key(consumer, &target, &identity, scope).map_err(|error| CallFailure {
            error: TransportError::Unavailable(error.to_string()),
            reconnect: false,
        })?;
    let permit = RELAY_OPERATIONS
        .clone()
        .try_acquire_owned()
        .map_err(|_| CallFailure {
            error: TransportError::Unavailable("the relay operation budget is busy".into()),
            reconnect: false,
        })?;
    let consumer = Arc::clone(consumer);
    let leases = Arc::clone(leases);
    let scope = scope.cloned();
    options.timeout = options.timeout.min(RELAY_CALL_TIMEOUT);
    options.route_retry_deadline = options.route_retry_deadline.min(RELAY_CALL_TIMEOUT);
    let (cancel, mut canceled) = tokio::sync::watch::channel(false);
    let _cancellation = RequestCancellation(cancel);
    // The transport owns open/register/release, not the caller. Dropping the
    // caller signals cancellation but cannot interrupt lease bookkeeping or
    // strand a handle between a successful open and registration.
    let operation = tokio::spawn(async move {
        let _permit = permit;
        leases.acquire(&key).await;
        #[cfg(test)]
        let pause = leases.pause.lock().unwrap().clone();
        #[cfg(test)]
        if let Some(pause) = &pause {
            pause.acquired.notify_one();
            pause.resume.notified().await;
        }
        let mut opened = Vec::new();
        let was_canceled = *canceled.borrow();
        let outcome = if was_canceled {
            Err(CallFailure {
                error: TransportError::Unavailable("relay requester canceled".into()),
                reconnect: false,
            })
        } else {
            // SDK-owned open/retry deadlines bound this phase. Do not drop an
            // opening future on the broker's shorter deadline: finish the open,
            // register its handle, then honor cancellation before forwarding.
            open_and_send(
                &consumer,
                &leases,
                &key,
                target,
                identity,
                scope.as_ref(),
                options,
                bytes,
                &mut opened,
                &mut canceled,
            )
            .await
        };
        // Finalization outlives the caller, including asynchronous handle
        // registration. Closing is bounded without canceling map bookkeeping.
        for route in opened {
            leases.opened(&key, route).await;
        }
        leases.release(&key, &consumer).await;
        #[cfg(test)]
        if let Some(pause) = pause {
            pause.finished.notify_one();
        }
        outcome
    });
    operation.await.map_err(|error| CallFailure {
        error: TransportError::Unavailable(format!("relay operation failed: {error}")),
        reconnect: false,
    })?
}

#[allow(clippy::too_many_arguments)]
async fn open_and_send(
    consumer: &subc_client_rs::SubcConsumer,
    leases: &RouteLeases,
    key: &str,
    target: subc_protocol::RouteTarget,
    identity: subc_protocol::BindIdentity,
    scope: Option<&ScopeSelector>,
    options: subc_client_rs::CallOptions,
    bytes: Vec<u8>,
    opened_handles: &mut Vec<subc_client_rs::RouteHandle>,
    canceled: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Value, CallFailure> {
    use subc_client_rs::{CallError, CallOptions};
    let opened = match scope {
        Some(scope) => {
            consumer
                .open_route_scoped(target, identity, scope.clone(), options)
                .await
        }
        None => consumer.open_route(target, identity, options).await,
    };
    let route = match opened {
        Ok(route) => route,
        Err(error) => {
            if let Some(refusal) = scope.and_then(|selector| scope_open_refusal(&error, selector)) {
                return Err(CallFailure {
                    error: refusal,
                    reconnect: false,
                });
            }
            return Err(match error {
                CallError::Module(body) => CallFailure {
                    error: TransportError::Refused {
                        code: body.code,
                        message: body.message,
                    },
                    reconnect: false,
                },
                // A broken connection is reopened on the next relay.
                error => CallFailure {
                    error: TransportError::Unavailable(error.to_string()),
                    reconnect: true,
                },
            });
        }
    };
    opened_handles.push(route.clone());
    leases.opened(key, route).await;
    if *canceled.borrow() {
        return Err(CallFailure {
            error: TransportError::Unavailable("relay requester canceled".into()),
            reconnect: false,
        });
    }
    let reply = tokio::select! {
        biased;
        _ = canceled.changed() => return Err(CallFailure { error:TransportError::Unavailable("relay requester canceled".into()), reconnect:false }),
        reply = consumer.request(&route, bytes, CallOptions { timeout:RELAY_CALL_TIMEOUT, ..CallOptions::default() }) => reply,
    };
    match reply {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| CallFailure {
            error: TransportError::OutcomeUnknown("the reply was not JSON".to_string()),
            reconnect: false,
        }),
        Err(CallError::Module(body)) => Err(CallFailure {
            error: TransportError::Refused {
                code: body.code,
                message: body.message,
            },
            reconnect: false,
        }),
        Err(CallError::NotSent(error)) => Err(CallFailure {
            error: TransportError::Unavailable(error.to_string()),
            reconnect: true,
        }),
        Err(error) => Err(CallFailure {
            error: TransportError::OutcomeUnknown(error.to_string()),
            reconnect: true,
        }),
    }
}

/// Outbound calls over the daemon's own authenticated subc connection. The
/// connection is opened lazily on first use and reopened after a failure.
pub struct SubcRelayTransport {
    connection_file: PathBuf,
    consumer: Arc<tokio::sync::Mutex<Option<Arc<subc_client_rs::SubcConsumer>>>>,
    leases: Arc<RouteLeases>,
    /// Replaces the subc client's route-open deadline (30 s, with retries for
    /// up to 90 s) when set. Production leaves it unset; tests shorten it so a
    /// retried refusal runs out quickly.
    route_open_deadline: Option<Duration>,
}

impl SubcRelayTransport {
    fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: Arc::new(tokio::sync::Mutex::new(None)),
            leases: Arc::new(RouteLeases::default()),
            route_open_deadline: None,
        }
    }

    /// A transport already holding `consumer`, for tests that serve it from a
    /// fake daemon without a module identity.
    #[cfg(test)]
    fn with_consumer(
        consumer: subc_client_rs::SubcConsumer,
        route_open_deadline: Option<Duration>,
    ) -> Self {
        let mut transport = Self::new(PathBuf::from("/nonexistent/aft-gh-relay-test"));
        transport.route_open_deadline = route_open_deadline;
        *transport
            .consumer
            .try_lock()
            .expect("a new transport's consumer slot is free") = Some(Arc::new(consumer));
        transport
    }

    fn identity_available() -> bool {
        crate::launch_nonce::consumer_identity().is_some()
    }

    async fn consumer(&self) -> Result<Arc<subc_client_rs::SubcConsumer>, TransportError> {
        let mut slot = self.consumer.lock().await;
        if let Some(consumer) = slot.as_ref() {
            return Ok(Arc::clone(consumer));
        }
        // The identity is fixed for the life of the process, so checking it
        // before each new connection is the same as checking it on every call.
        if !Self::identity_available() {
            return Err(TransportError::Unavailable(
                "the daemon has no subc module identity to relay with".to_string(),
            ));
        }
        let options = subc_client_rs::ConsumerOptions {
            call_timeout: RELAY_CALL_TIMEOUT,
            ..subc_client_rs::ConsumerOptions::default()
        };
        let consumer = crate::fleet_status::connect_subc_consumer(&self.connection_file, options)
            .await
            .map_err(|error| TransportError::Unavailable(error.to_string()))?;
        let consumer = Arc::new(consumer);
        *slot = Some(Arc::clone(&consumer));
        Ok(consumer)
    }

    async fn call(
        &self,
        target: subc_protocol::RouteTarget,
        project_root: &str,
        session: &str,
        scope: Option<&ScopeSelector>,
        body: Value,
    ) -> Result<Value, TransportError> {
        let consumer = self.consumer().await?;
        let identity = subc_protocol::BindIdentity::new(
            if project_root.is_empty() {
                "/"
            } else {
                project_root
            }
            .to_string(),
            "aft-gh-relay",
            session.to_string(),
        );
        let mut options = subc_client_rs::CallOptions {
            consumer_identity: crate::launch_nonce::consumer_identity(),
            ..subc_client_rs::CallOptions::default()
        };
        if let Some(deadline) = self.route_open_deadline {
            options.timeout = deadline;
            options.route_retry_deadline = deadline;
        }
        match route_request(
            &consumer,
            &self.leases,
            target,
            identity,
            scope,
            options,
            &body,
        )
        .await
        {
            Ok(reply) => Ok(reply),
            Err(failure) => {
                if failure.reconnect {
                    self.clear_failed_consumer(&consumer).await;
                }
                Err(failure.error)
            }
        }
    }

    async fn clear_failed_consumer(&self, failed: &Arc<subc_client_rs::SubcConsumer>) {
        let mut slot = self.consumer.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            *slot = None;
        }
    }
}

impl RelayTransport for SubcRelayTransport {
    async fn plexus(
        &self,
        project_root: &str,
        session: &str,
        scope: Option<&ScopeSelector>,
        body: Value,
    ) -> Result<Value, TransportError> {
        self.call(
            subc_protocol::RouteTarget::ToolProvider {
                module_id: PLEXUS_MODULE_ID.to_string(),
            },
            project_root,
            session,
            scope,
            body,
        )
        .await
    }

    /// True: the subc client opens a route to plexus under the session's scope
    /// (`SubcConsumer::open_route_scoped`, since subc-client-rs 0.23.2), so a
    /// session whose scope delegates to its agent can send bot writes.
    fn opens_scoped_routes(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "../gh_shim/plexus_bindings_contract.rs"]
pub(crate) mod bindings_contract;
#[cfg(test)]
pub(crate) mod tests;

fn route_lease_key(
    consumer: &Arc<subc_client_rs::SubcConsumer>,
    target: &subc_protocol::RouteTarget,
    identity: &subc_protocol::BindIdentity,
    scope: Option<&ScopeSelector>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&(Arc::as_ptr(consumer) as usize, target, identity, scope))
}

impl GhRelay {
    pub(crate) async fn reserve_bot(
        &self,
        project: &str,
        session: &str,
        scope: &ScopeStamp,
        body: Value,
        deadline: tokio::time::Instant,
    ) -> Result<crate::gh_shim_speech::partner::ReservedBotRequest, crate::gh_shim_speech::Refusal>
    {
        self.transport
            .reserve_scoped(project, session, scope, body, deadline)
            .await
    }
}
