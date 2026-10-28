//! Partner wire contracts and the validated reserved-send boundary.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_protocol::scope::ScopeStamp;

use super::model::Binding;
use super::{Code, Refusal, ScopeRole};

// Placement is a release decision, never a command argument or inherited env flag.
const PLEX_EXPECT_PLACED_COMMIT: Option<&str> = Some("ee7e5acf2b0b");
const PLEX_RUN_SCOPE_PLACED_COMMIT: Option<&str> = Some("ee7e5acf2b0b");
pub(crate) const SUBC_RESERVATION_LAST_CHECKED: &str = "0.32.7";

pub(crate) fn require_plex_guards(codemode: bool) -> Result<(), Refusal> {
    if PLEX_EXPECT_PLACED_COMMIT.is_none() || (codemode && PLEX_RUN_SCOPE_PLACED_COMMIT.is_none()) {
        log::debug!("PLEX atomic guard placement not confirmed");
        return Err(Refusal::new(
            Code::AuthorityUnavailable,
            "guard_unsupported",
        ));
    }
    Ok(())
}

pub(crate) fn require_reserved_send() -> Result<(), Refusal> {
    log::trace!("SUBC reserved writer API is available in client {SUBC_RESERVATION_LAST_CHECKED}");
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunSelector {
    pub(crate) owner: String,
    #[serde(rename = "ref")]
    pub(crate) scope_ref: String,
    pub(crate) epoch: u64,
}

impl RunSelector {
    pub(crate) fn from_stamp(stamp: &ScopeStamp) -> Self {
        let owner = match &stamp.owner {
            subc_protocol::Principal::Direct => "direct".to_string(),
            subc_protocol::Principal::Unverified => "unverified".to_string(),
            subc_protocol::Principal::Reserved { module_id } => format!("reserved:{module_id}"),
        };
        Self {
            owner,
            scope_ref: stamp.scope_ref.clone(),
            epoch: stamp.scope_epoch,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BotRequest {
    op: &'static str,
    request: Value,
    request_nonce: String,
    expect: Binding,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_scope: Option<RunSelector>,
}

impl BotRequest {
    pub(crate) fn new(
        request: Value,
        nonce: &str,
        expect: Binding,
        run: Option<&ScopeStamp>,
    ) -> Result<Self, Refusal> {
        if nonce.is_empty() || expect.app_handle_id.is_empty() {
            return Err(Refusal::new(Code::AuthorityUnavailable, "snapshot_invalid"));
        }
        Ok(Self {
            op: "bot_request",
            request,
            request_nonce: nonce.to_string(),
            expect,
            run_scope: run.map(RunSelector::from_stamp),
        })
    }

    pub(crate) fn value(&self) -> Value {
        json!(self)
    }

    pub(crate) fn expect_json(&self) -> String {
        json!(self.expect).to_string()
    }

    /// PLEX excludes run_scope from the nonce fingerprint; retries keep expect fixed.
    pub(crate) fn payload_digest(&self) -> [u8; 32] {
        let bytes = serde_json::to_vec(&json!({"request":self.request,"expect":self.expect}))
            .expect("JSON values serialize");
        blake3::derive_key("aft-gh-bot-request-v1", &bytes)
    }
}

/// The closure may only be built around the partner's real reserved queue slot.
pub(crate) struct ReservedBotRequest {
    commit: Box<dyn FnOnce() -> Result<QueuedBotResponse, Refusal> + Send>,
}

impl ReservedBotRequest {
    pub(crate) fn new(
        commit: impl FnOnce() -> Result<QueuedBotResponse, Refusal> + Send + 'static,
    ) -> Self {
        Self {
            commit: Box::new(commit),
        }
    }
    #[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn for_test(
        commit: impl FnOnce() -> Result<QueuedBotResponse, Refusal> + Send + 'static,
    ) -> Self {
        Self {
            commit: Box::new(commit),
        }
    }

    pub(crate) fn send(self) -> Result<QueuedBotResponse, Refusal> {
        (self.commit)()
    }
}

pub(crate) struct QueuedBotResponse {
    response: Pin<Box<dyn Future<Output = Result<Value, QueuedResponseError>> + Send>>,
}

#[derive(Debug)]
pub(crate) enum QueuedResponseError {
    NotSent(Refusal),
    Unknown(String),
}

impl QueuedBotResponse {
    pub(crate) fn new(
        response: impl Future<Output = Result<Value, QueuedResponseError>> + Send + 'static,
    ) -> Self {
        Self {
            response: Box::pin(response),
        }
    }
    #[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn for_test(reply: Value) -> Self {
        Self {
            response: Box::pin(async move { Ok(reply) }),
        }
    }

    pub(crate) async fn response(self) -> Result<Value, QueuedResponseError> {
        self.response.await
    }
}

pub(crate) fn map_refusal(result: &Value, route_role: ScopeRole) -> Option<Refusal> {
    if result.get("status").and_then(Value::as_str) != Some("refused")
        || result
            .get("legs_sent")
            .and_then(Value::as_array)
            .is_some_and(|legs| !legs.is_empty())
        || result.get("transmission").and_then(Value::as_str) != Some("not_sent")
    {
        return None;
    }
    let explicit = result.get("code").and_then(Value::as_str);
    let decorated = result.get("refusal_code").and_then(Value::as_str);
    let prefix = decorated.map(|code| code.split('{').next().unwrap_or(code));
    if explicit.zip(prefix).is_some_and(|(a, b)| a != b) {
        return None;
    }
    let code = explicit.or(prefix)?;
    let role = match result.get("scope_role").and_then(Value::as_str) {
        Some("run") => ScopeRole::Run,
        Some("route") | None => route_role,
        _ => return None,
    };
    Some(match code {
        "stale_authority" => match result.get("dimension").and_then(Value::as_str) {
            Some("expected_binding") => Refusal::new(Code::AuthorityUnavailable, "admission_raced"),
            Some("expected_handle") => Refusal::new(Code::IdentityConflict, "binding_changed"),
            _ => return None,
        },
        "invalid_request" | "nonce_payload_mismatch" => {
            Refusal::new(Code::AuthorityUnavailable, "snapshot_invalid")
        }
        "delegation_withdrawn" => Refusal::new(Code::AuthorityUnavailable, "delegation_withdrawn"),
        "scope_ended" => Refusal::scoped(Code::ScopeEnded, "ended", role),
        "scope_not_synced" => Refusal::scoped(Code::ScopeUnknown, "not_synced", role),
        "scope_unverifiable" => Refusal::scoped(Code::ScopeUnknown, "unverifiable", role),
        "run_scope_not_child" if role == ScopeRole::Run => {
            Refusal::scoped(Code::ScopeUnknown, "unverifiable", role)
        }
        _ => return None,
    })
}

/// End evidence is independent of transmission outcome: a completed mutation leg
/// stays posted even when the same authenticated reply ends the selected scope.
pub(crate) fn scope_end_role(
    result: &Value,
    route_role: ScopeRole,
    codemode: bool,
) -> Option<ScopeRole> {
    if result.get("status").and_then(Value::as_str) != Some("refused") {
        return None;
    }
    let direct = result.get("code").and_then(Value::as_str);
    let decorated = result
        .get("refusal_code")
        .and_then(Value::as_str)
        .map(|code| code.split('{').next().unwrap_or(code));
    if direct.zip(decorated).is_some_and(|(a, b)| a != b)
        || direct.or(decorated) != Some("scope_ended")
    {
        return None;
    }
    match result.get("scope_role").and_then(Value::as_str) {
        Some("run") if codemode => Some(ScopeRole::Run),
        Some("route") => Some(route_role),
        None if !codemode => Some(route_role),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
