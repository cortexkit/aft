//! Daemon-side relay for governed `gh` bot writes.
//!
//! The `gh` shim runs as an agent's child with no subc credentials, so it
//! cannot mint an agent assertion or reach plexus itself: both require the
//! attested `reserved:aft` principal that only this daemon holds. The shim
//! instead sends its per-command ticket (see [`crate::gh_shim_ticket`]) and the
//! unchanged governed request envelope to one of the management operations
//! below. The daemon redeems the ticket to the session that spawned the
//! command, mints an assertion for that session from prefrontal, calls plexus
//! with it, and hands plexus's reply back unchanged.
//!
//! The ticket is the only authority. A session or agent field in the body is
//! never used to pick the speaker, and a missing or unknown ticket is refused
//! before anything leaves the daemon.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

/// Relay a governed bot write. Params: `{ticket, request_nonce, request}`.
pub const BOT_REQUEST_OPERATION: &str = "gh_shim.bot_request";
/// Relay plexus's repository-to-agent binding read for the shim's version
/// check. Params: `{ticket, connection_id}`.
pub const BINDINGS_READ_OPERATION: &str = "gh_shim.bindings_read";

pub(crate) const PREFRONTAL_MODULE_ID: &str = "prefrontal-core";
pub(crate) const PLEXUS_MODULE_ID: &str = "plexus";
const MINT_METHOD: &str = "agent.assertion_mint";
/// A cached assertion is re-minted this long before its `exp`, so a token
/// never expires between the cache check and plexus's verification.
const TOKEN_REFRESH_MARGIN_SECS: u64 = 5 * 60;
const RELAY_CALL_TIMEOUT: Duration = Duration::from_secs(20);

pub fn is_relay_operation(operation: &str) -> bool {
    matches!(operation, BOT_REQUEST_OPERATION | BINDINGS_READ_OPERATION)
}

/// Plexus answers a tool call either with its facade reply directly or wrapped
/// in a tool-call result whose first text block holds the reply as JSON.
/// Return the facade reply in both cases.
pub fn facade_reply(value: &Value) -> Value {
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
    /// The target answered with a subc Error frame carrying this code.
    Refused { code: String, message: String },
    /// Nothing was sent: no connection, no route, or the module is absent.
    Unavailable(String),
    /// The request was sent but no reply arrived; it may have executed.
    OutcomeUnknown(String),
}

/// The two outbound calls the relay makes. Production uses the daemon's own
/// `reserved:aft` subc connection; tests use fakes that record the bodies.
pub trait RelayTransport: Send + Sync {
    /// Send `body` to prefrontal-core and return the parsed JSON reply.
    fn prefrontal(
        &self,
        project_root: &str,
        session: &str,
        body: Value,
    ) -> impl std::future::Future<Output = Result<Value, TransportError>> + Send;
    /// Send `body` to plexus's tool provider and return the parsed JSON reply.
    fn plexus(
        &self,
        project_root: &str,
        session: &str,
        body: Value,
    ) -> impl std::future::Future<Output = Result<Value, TransportError>> + Send;
}

#[derive(Clone, Debug)]
struct CachedToken {
    token: Value,
    exp: u64,
}

/// Minted assertions kept in daemon memory, keyed by agent and session. The
/// map is only touched briefly from the relay task, never from the subc frame
/// loop.
#[derive(Default)]
pub struct TokenCache {
    tokens: Mutex<HashMap<(String, String), CachedToken>>,
}

impl TokenCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), CachedToken>> {
        self.tokens
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn fresh(&self, agent: &str, session: &str, now: u64) -> Option<Value> {
        self.lock()
            .get(&(agent.to_string(), session.to_string()))
            .filter(|cached| now.saturating_add(TOKEN_REFRESH_MARGIN_SECS) < cached.exp)
            .map(|cached| cached.token.clone())
    }

    fn store(&self, agent: &str, session: &str, token: Value, exp: u64) {
        self.lock().insert(
            (agent.to_string(), session.to_string()),
            CachedToken { token, exp },
        );
    }

    fn drop_token(&self, agent: &str, session: &str) {
        self.lock()
            .remove(&(agent.to_string(), session.to_string()));
    }
}

/// What the daemon sends back to the shim. `ok` replies carry plexus's reply
/// unchanged; refusals carry `{refusal_code, stage, message}`.
#[derive(Clone, Debug, PartialEq)]
pub struct RelayReply {
    pub ok: bool,
    pub data: Value,
}

impl RelayReply {
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
    /// Current Unix time in seconds, for the token cache.
    pub now: u64,
}

/// Run one relay operation. `log` receives exactly one line per call carrying
/// the redeemed session, task id, nonce, action, repository and outcome; the
/// ticket and the assertion never appear in it.
pub async fn relay<T: RelayTransport>(
    transport: &T,
    cache: &TokenCache,
    call: RelayCall<'_>,
    log: &(dyn Fn(&str) + Send + Sync),
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
    let reply = 'reply: {
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
        let Some(redeemed) = crate::gh_shim_ticket::redeem(ticket) else {
            break 'reply RelayReply::refused(
                "ticket_unknown",
                "ticket",
                "the command's session ticket is not live; it ended or was never issued",
            );
        };
        session = redeemed.session_id.clone();
        task = if redeemed.task_id.is_empty() {
            "-".to_string()
        } else {
            redeemed.task_id.clone()
        };
        match call.operation {
            BINDINGS_READ_OPERATION => bindings_read(transport, &redeemed, params).await,
            _ => bot_request(transport, cache, &redeemed, params, call.now).await,
        }
    };

    log(&format!(
        "gh_shim relay: op={} session={} task={} nonce={} action={} repository={} outcome={}",
        call.operation,
        session,
        task,
        if nonce.is_empty() { "-" } else { nonce },
        action,
        repository,
        reply.outcome_label(),
    ));
    reply
}

async fn bindings_read<T: RelayTransport>(
    transport: &T,
    redeemed: &crate::gh_shim_ticket::Redeemed,
    params: &Value,
) -> RelayReply {
    let Some(connection_id) = params.get("connection_id").and_then(Value::as_str) else {
        return RelayReply::refused(
            "request_malformed",
            "request",
            "bindings read needs a connection_id",
        );
    };
    let body = json!({
        "name": "github",
        "arguments": {"op": "bindings.read", "connection_id": connection_id},
    });
    match transport
        .plexus(&redeemed.project_root, &redeemed.session_id, body)
        .await
    {
        Ok(reply) => RelayReply::relayed(reply),
        Err(error) => transport_refusal("plexus", error),
    }
}

async fn bot_request<T: RelayTransport>(
    transport: &T,
    cache: &TokenCache,
    redeemed: &crate::gh_shim_ticket::Redeemed,
    params: &Value,
    now: u64,
) -> RelayReply {
    let Some(nonce) = params
        .get("request_nonce")
        .and_then(Value::as_str)
        .filter(|nonce| !nonce.is_empty())
    else {
        return RelayReply::refused("request_malformed", "request", "request_nonce is required");
    };
    let Some(request) = params.get("request").filter(|request| request.is_object()) else {
        return RelayReply::refused("request_malformed", "request", "request must be an object");
    };
    // The agent is the one the shim's signed manifest binds to the target
    // repository. It names whose bot to mint for; the session comes from the
    // ticket alone, and plexus re-checks the agent against its own bindings.
    let Some(agent_id) = request
        .get("metadata")
        .and_then(|metadata| metadata.get("agent_id"))
        .and_then(Value::as_str)
        .filter(|agent| !agent.is_empty())
    else {
        return RelayReply::refused(
            "request_malformed",
            "request",
            "request.metadata.agent_id is required",
        );
    };
    let session = redeemed.session_id.as_str();
    let token = match cache.fresh(agent_id, session, now) {
        Some(token) => token,
        None => {
            let body = json!({
                "method": MINT_METHOD,
                "params": {"agent_id": agent_id, "session": session},
            });
            let reply = match transport
                .prefrontal(&redeemed.project_root, session, body)
                .await
            {
                Ok(reply) => reply,
                Err(error) => return transport_refusal("mint", error),
            };
            let Some(token) = reply
                .get("result")
                .and_then(|result| result.get("token"))
                .filter(|token| token.is_object())
                .cloned()
            else {
                return RelayReply::refused(
                    "mint_reply_malformed",
                    "mint",
                    "prefrontal's assertion mint reply had no result.token",
                );
            };
            let exp = token
                .get("claims")
                .and_then(|claims| claims.get("exp"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            cache.store(agent_id, session, token.clone(), exp);
            token
        }
    };
    let body = json!({
        "name": "github",
        "arguments": {
            "op": "bot_request",
            "assertion": token,
            "request_nonce": nonce,
            "request": request,
        },
    });
    match transport
        .plexus(&redeemed.project_root, session, body)
        .await
    {
        Ok(reply) => {
            // A refusal that names the assertion means the cached token is no
            // longer good; the shim's retry must get a freshly minted one.
            if facade_refusal_code(&facade_reply(&reply))
                .is_some_and(|code| code.starts_with("assertion_"))
            {
                cache.drop_token(agent_id, session);
            }
            RelayReply::relayed(reply)
        }
        Err(error) => transport_refusal("plexus", error),
    }
}

fn transport_refusal(stage: &str, error: TransportError) -> RelayReply {
    match error {
        TransportError::Refused { code, message } => RelayReply::refused(&code, stage, message),
        TransportError::Unavailable(message) => {
            RelayReply::refused("relay_unavailable", stage, message)
        }
        TransportError::OutcomeUnknown(message) => {
            RelayReply::refused("outcome_unknown", stage, message)
        }
    }
}

/// The daemon's relay state: the token cache and the outbound transport.
pub struct GhRelay {
    pub cache: TokenCache,
    pub transport: SubcRelayTransport,
}

impl GhRelay {
    pub fn new(connection_file: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            cache: TokenCache::default(),
            transport: SubcRelayTransport::new(connection_file),
        })
    }
}

/// Outbound calls over the daemon's own authenticated subc connection. The
/// connection is opened lazily on first use and reopened after a failure.
pub struct SubcRelayTransport {
    connection_file: PathBuf,
    consumer: tokio::sync::Mutex<Option<Arc<subc_client_rs::SubcConsumer>>>,
}

impl SubcRelayTransport {
    fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: tokio::sync::Mutex::new(None),
        }
    }

    fn identity_available() -> bool {
        ["SUBC_MODULE_ID", "SUBC_LAUNCH_NONCE"].iter().all(|key| {
            std::env::var(key)
                .ok()
                .is_some_and(|value| !value.trim().is_empty())
        })
    }

    async fn consumer(&self) -> Result<Arc<subc_client_rs::SubcConsumer>, TransportError> {
        if !Self::identity_available() {
            return Err(TransportError::Unavailable(
                "the daemon has no subc module identity to relay with".to_string(),
            ));
        }
        let mut slot = self.consumer.lock().await;
        if let Some(consumer) = slot.as_ref() {
            return Ok(Arc::clone(consumer));
        }
        let options = subc_client_rs::ConsumerOptions {
            call_timeout: RELAY_CALL_TIMEOUT,
            ..subc_client_rs::ConsumerOptions::default()
        };
        let consumer =
            crate::fleet_status::connect_subc_consumer(&self.connection_file, options)
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
        body: Value,
    ) -> Result<Value, TransportError> {
        use subc_client_rs::{CallError, CallOptions, CloseRouteOptions};
        let consumer = self.consumer().await?;
        let identity = subc_protocol::BindIdentity::new(
            if project_root.is_empty() { "/" } else { project_root }.to_string(),
            "aft-gh-relay",
            session.to_string(),
        );
        let route = match consumer
            .open_route(target, identity, CallOptions::default())
            .await
        {
            Ok(route) => route,
            Err(CallError::Module(body)) => {
                return Err(TransportError::Refused {
                    code: body.code,
                    message: body.message,
                })
            }
            Err(error) => {
                // A broken connection is reopened on the next relay.
                *self.consumer.lock().await = None;
                return Err(TransportError::Unavailable(error.to_string()));
            }
        };
        let bytes = serde_json::to_vec(&body)
            .map_err(|error| TransportError::Unavailable(error.to_string()))?;
        let response = consumer
            .request(&route, bytes, CallOptions::default())
            .await;
        let _ = consumer
            .close_handle(&route, CloseRouteOptions::default())
            .await;
        match response {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
                TransportError::OutcomeUnknown("the reply was not JSON".to_string())
            }),
            Err(CallError::Module(body)) => Err(TransportError::Refused {
                code: body.code,
                message: body.message,
            }),
            Err(CallError::NotSent(error)) => {
                *self.consumer.lock().await = None;
                Err(TransportError::Unavailable(error.to_string()))
            }
            Err(error) => {
                *self.consumer.lock().await = None;
                Err(TransportError::OutcomeUnknown(error.to_string()))
            }
        }
    }
}

impl RelayTransport for SubcRelayTransport {
    async fn prefrontal(
        &self,
        project_root: &str,
        session: &str,
        body: Value,
    ) -> Result<Value, TransportError> {
        self.call(
            subc_protocol::RouteTarget::ManagementSurface {
                module_id: PREFRONTAL_MODULE_ID.to_string(),
            },
            project_root,
            session,
            body,
        )
        .await
    }

    async fn plexus(
        &self,
        project_root: &str,
        session: &str,
        body: Value,
    ) -> Result<Value, TransportError> {
        self.call(
            subc_protocol::RouteTarget::ToolProvider {
                module_id: PLEXUS_MODULE_ID.to_string(),
            },
            project_root,
            session,
            body,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
