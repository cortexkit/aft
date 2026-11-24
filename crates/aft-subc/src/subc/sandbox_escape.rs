//! The sandbox-escape card adapter for the shared consent waiter.
//!
//! A successful answer supplies the existing one-shot `esc_` host grant to the
//! normal bash rerun. The waiter, cancellation handling and withdrawal path are
//! the same ones used by worker permission cards; this module supplies only the
//! closed wire shape, outcome decoding, execution report and audit record.

use super::*;
use crate::db::access_asks::{self as asks, Permission};
use crate::db::sandbox_escape_records::{self as records, NewRecord};
use aft_engine::sandbox_escape::{EscapePlan, REFUSED_CODE};
use std::collections::HashMap;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

const RUN_ONCE: &str = "run_once";
const DECLINE: &str = "decline";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    SandboxEscape,
}

/// The exact closed object accepted by Cingulate 0.28.0.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardFields {
    pub call_key: String,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub escapes_level: crate::config::SandboxMode,
    pub background: bool,
    pub reason: String,
    pub expires_in_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardRequest {
    pub kind: CardKind,
    pub sandbox_escape: CardFields,
}

impl CardFields {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.call_key.trim().is_empty() {
            return Err("call_key must be nonblank");
        }
        aft_engine::sandbox_escape::validate_fields(&self.command, &self.reason)
            .map_or(Ok(()), |refusal| Err(refusal.message))?;
        if !self.cwd.starts_with('/') || self.cwd.contains('\0') {
            return Err("cwd must be an absolute path without NUL");
        }
        if !self.shell.starts_with('/') || self.shell.contains('\0') {
            return Err("shell must be an absolute path without NUL");
        }
        if aft_engine::sandbox_escape::escaped_level_name(self.escapes_level).is_none() {
            return Err("escapes_level must name a confined level");
        }
        if !(1..=aft_engine::sandbox_escape::EXPIRES_IN_MS).contains(&self.expires_in_ms) {
            return Err("expires_in_ms is out of range");
        }
        Ok(())
    }
}

/// The requester record fields AFT reads. Other served card metadata is retained
/// verbatim so contract fixtures round-trip without dropping unknown metadata.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RequesterRecord {
    pub kind: CardKind,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_choice_id: Option<String>,
    pub sandbox_escape: CardFields,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}

/// Phone reads use camelCase chrome but preserve the nested request's
/// snake_case fields. AFT's contract tests decode those served answers too.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PhoneRecord {
    pub kind: CardKind,
    #[serde(rename = "sandboxEscape")]
    pub sandbox_escape: CardFields,
    #[serde(flatten)]
    pub metadata: serde_json::Map<String, Value>,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum AnswerRecord {
    Requester(RequesterRecord),
    Phone(PhoneRecord),
}

pub fn argument(arguments: &Value) -> Option<&Value> {
    arguments
        .get("escape_sandbox")
        .or_else(|| arguments.get("params")?.get("escape_sandbox"))
}

fn args(arguments: &Value) -> &Value {
    arguments.get("params").unwrap_or(arguments)
}

/// A codemode agent runs under an ephemeral lifetime stamp but files through
/// its stamped head owner. Workers and all other route kinds never file.
fn consent_identity(identity: &RouteIdentity) -> Option<&RouteIdentity> {
    if let Some(owner) = identity.run_owner.as_ref() {
        return matches!(owner.kind, super::codemode::RunKind::Agent { .. })
            .then_some(&owner.owner_route)
            .filter(|owner| head_stamp(owner).is_some());
    }
    head_stamp(identity).map(|_| identity)
}

fn head_stamp(identity: &RouteIdentity) -> Option<&ScopeStamp> {
    identity
        .scope
        .as_ref()
        .filter(|stamp| stamp.kind == ScopeKind::Head)
}

fn remote_requested(arguments: &Value) -> bool {
    args(arguments)
        .get("runon")
        .and_then(Value::as_str)
        .is_some_and(|runon| !runon.trim().is_empty())
}

/// Refuse inapplicable calls before dispatch, grant creation or card filing.
pub fn preflight_response(
    identity: &RouteIdentity,
    request_id: &str,
    arguments: &Value,
) -> Option<Response> {
    let reason = argument(arguments)?;
    let refuse = |message| Some(Response::error(request_id, REFUSED_CODE, message));
    if consent_identity(identity).is_none() {
        return refuse(aft_engine::sandbox_escape::HEAD_ONLY);
    }
    if remote_requested(arguments) {
        return refuse(aft_engine::sandbox_escape::LOCAL_ONLY);
    }
    if cfg!(windows) {
        return refuse(aft_engine::sandbox_escape::WINDOWS_REFUSED);
    }
    let Some(reason) = reason.as_str() else {
        return refuse(aft_engine::sandbox_escape::REASON_INVALID);
    };
    let command = args(arguments)
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    aft_engine::sandbox_escape::validate_fields(command, reason)
        .map(|refusal| Response::error(request_id, refusal.code, refusal.message))
}

/// Convert the command handler's pending plan into the permission carried by
/// the existing consent flow. The extra fields remain deserializable as an
/// ordinary Shell row by an older binary, which ignores unknown object fields.
pub fn requested_permission(response: &Response) -> Option<Permission> {
    if response.success
        || response.data.get("code")?.as_str()? != aft_engine::sandbox_escape::REQUIRED_CODE
    {
        return None;
    }
    let plan: EscapePlan =
        serde_json::from_value(response.data.get("sandbox_escape")?.clone()).ok()?;
    Some(Permission::SandboxEscape {
        command: plan.command,
        cwd: plan.cwd,
        permissions: vec![plan.grant_id],
        shell: plan.shell,
        escapes_level: plan.escapes_level.as_str().to_owned(),
        background: plan.background,
        reason: plan.reason,
    })
}

pub fn is_escape(permission: &Permission) -> bool {
    matches!(permission, Permission::SandboxEscape { .. })
}

/// Real sandbox_escape approval requests stay disabled. Cingulate is the daemon module
/// that shows approval cards on the person's approving device. It must first
/// stop accepting approvals from a key that is not a real approving device;
/// verify that route check before enabling this gate. Tests must install the
/// fake explicitly; there is no fallback to the process's real transport even
/// in a test build.
pub fn card_transport() -> Option<Arc<dyn ConsentTransport>> {
    #[cfg(test)]
    {
        super::TEST_TRANSPORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    #[cfg(not(test))]
    {
        None
    }
}

pub fn hold_budget(permission: &Permission) -> Option<Duration> {
    is_escape(permission).then_some(Duration::from_millis(
        aft_engine::sandbox_escape::EXPIRES_IN_MS,
    ))
}

fn fields(call_key: &str, permission: &Permission) -> CardFields {
    let Permission::SandboxEscape {
        command,
        cwd,
        shell,
        escapes_level,
        background,
        reason,
        ..
    } = permission
    else {
        unreachable!("only sandbox escapes use these card fields")
    };
    CardFields {
        call_key: call_key.into(),
        command: command.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        shell: shell.to_string_lossy().into_owned(),
        escapes_level: serde_json::from_value(json!(escapes_level)).expect("valid sandbox level"),
        background: *background,
        reason: reason.clone(),
        expires_in_ms: aft_engine::sandbox_escape::EXPIRES_IN_MS,
    }
}

pub fn request_params(call_key: &str, permission: &Permission) -> Value {
    let request = CardRequest {
        kind: CardKind::SandboxEscape,
        sandbox_escape: fields(call_key, permission),
    };
    debug_assert!(request.sandbox_escape.validate().is_ok());
    serde_json::to_value(request).expect("sandbox escape request serializes")
}

pub fn decode_elicitation_id(request: &Value, result: &Value) -> Result<String, ConsentError> {
    if request.get("kind").and_then(Value::as_str) != Some("sandbox_escape") {
        return super::decode_elicitation_id(result);
    }
    result
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("elicitation_id"))
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("el_") && id.len() > 3)
        .map(str::to_owned)
        .ok_or_else(|| super::malformed("consent.request did not return an el_ elicitation_id"))
}

pub fn decode_record(record: &Value) -> Result<Settlement, ConsentError> {
    let record: RequesterRecord = serde_json::from_value(record.clone())
        .map_err(|_| super::malformed("the sandbox escape record is not understood"))?;
    match record.state.as_str() {
        "pending" => Ok(Settlement::Pending),
        "answered" => match record.answered_choice_id.as_deref() {
            Some(RUN_ONCE) => Ok(Settlement::Allow {
                scope: false,
                by_user: true,
            }),
            Some(DECLINE) => Ok(Settlement::Refused(
                aft_engine::sandbox_escape::DECLINED_CODE.into(),
            )),
            _ => Err(super::malformed("the sandbox escape answer is unknown")),
        },
        "expired" => Ok(Settlement::Refused(
            aft_engine::sandbox_escape::EXPIRED_CODE.into(),
        )),
        "withdrawn" => Ok(Settlement::Refused(
            aft_engine::sandbox_escape::WITHDRAWN_CODE.into(),
        )),
        _ => Err(super::malformed("the sandbox escape state is unknown")),
    }
}

/// Prepare a sandbox card in the same access_asks row machinery as a shell
/// permission. The row's added variant remains readable as Shell by old builds.
pub(super) fn prepare(
    conn: &rusqlite::Connection,
    identity: &RouteIdentity,
    tool: &str,
    arguments: &Value,
    permission: &Permission,
    own_holder: Option<&str>,
) -> rusqlite::Result<Prep> {
    let consent = consent_identity(identity).expect("sandbox escape preflight requires a head");
    let stamp = head_stamp(consent).expect("a head identity has a head stamp");
    let key = AskKey {
        scope: access_grants::scope_key(consent),
        args_hash: args_hash(consent, tool, arguments),
    };
    let selector = selector(stamp);
    if let Some(record) = asks::get(conn, &key)? {
        if let (Some(call_key), Some(request), Some(grant)) = (
            record.call_key.clone(),
            record.request.clone(),
            record.pending.clone(),
        ) {
            return Ok(Prep::Ask(Prepared {
                key,
                call_key,
                reused_file_card: false,
                request,
                selector,
                grant,
            }));
        }
        if record.holder_is_live() && record.holder.as_deref() != own_holder {
            return Ok(Prep::Busy);
        }
        if !record.holder_is_live() {
            asks::discard(conn, &key)?;
        }
    }
    let call_key = new_call_key();
    let request = request_params(&call_key, permission);
    asks::open_permission(
        conn,
        &key,
        &call_key,
        &serde_json::to_value(&selector).expect("scope selector is JSON"),
        &request,
        permission,
        crate::db::call_ledger::now_ms(),
    )?;
    let Permission::SandboxEscape {
        command,
        cwd,
        shell,
        escapes_level,
        background,
        ..
    } = permission
    else {
        unreachable!()
    };
    records::insert(
        conn,
        &NewRecord {
            call_key: call_key.clone(),
            requested_at_ms: crate::db::call_ledger::now_ms(),
            caller_session: identity.session.clone(),
            caller_scope: identity
                .scope
                .as_ref()
                .map_or_else(|| "unknown".into(), |scope| scope.scope_ref.clone()),
            agent_id: stamp.attributes.agent_id.clone(),
            command: command.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            shell: shell.to_string_lossy().into_owned(),
            escapes_level: escapes_level.clone(),
            background: *background,
        },
    )?;
    Ok(Prep::Ask(Prepared {
        key,
        call_key,
        reused_file_card: false,
        request,
        selector,
        grant: permission.clone(),
    }))
}

// The post-wait execution/report step needs the call key that preparation may
// have recovered from an earlier detached call. Route and correlation identify
// this in-flight call without putting that mutable detail into the request row.
static ACTIVE: std::sync::LazyLock<StdMutex<HashMap<(RouteChannel, u64), String>>> =
    std::sync::LazyLock::new(|| StdMutex::new(HashMap::new()));

pub(super) fn track(call: &HeldCall, prep: &Prepared) {
    if !is_escape(&prep.grant) {
        return;
    }
    ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((call.route, call.corr), prep.call_key.clone());
}

fn take_call_key(call: &HeldCall) -> Option<String> {
    ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(call.route, call.corr))
}

pub(super) async fn note_filed(call: &HeldCall, prep: &Prepared, elicitation: &str) {
    if !is_escape(&prep.grant) {
        return;
    }
    let call_key = prep.call_key.clone();
    let id = elicitation.to_owned();
    if let Err(error) = with_db(call, move |conn, _| {
        records::note_elicitation(conn, &call_key, &id)
    })
    .await
    {
        log::warn!("sandbox escape filing record failed: {error}");
    }
}

pub(super) async fn note_settlement(
    call: &HeldCall,
    prep: &Prepared,
    settled: &Settlement,
    _elicitation: &str,
) {
    if !is_escape(&prep.grant) {
        return;
    }
    let decision = match settled {
        Settlement::Allow { .. } => RUN_ONCE,
        Settlement::Deny { .. } => DECLINE,
        Settlement::Refused(code) if code == aft_engine::sandbox_escape::DECLINED_CODE => DECLINE,
        Settlement::Refused(code) if code == aft_engine::sandbox_escape::EXPIRED_CODE => "expired",
        Settlement::Refused(_) => "withdrawn",
        Settlement::Pending => return,
    };
    let call_key = prep.call_key.clone();
    if let Err(error) = with_db(call, move |conn, _| {
        records::note_decision(conn, &call_key, decision)
    })
    .await
    {
        log::warn!("sandbox escape decision record failed: {error}");
    }
}

fn refusal_for_code(code: &str) -> (&'static str, &'static str) {
    match code {
        aft_engine::sandbox_escape::DECLINED_CODE | "access_declined" => (
            aft_engine::sandbox_escape::DECLINED_CODE,
            aft_engine::sandbox_escape::DECLINED,
        ),
        aft_engine::sandbox_escape::EXPIRED_CODE | ACCESS_TIMEOUT => (
            aft_engine::sandbox_escape::EXPIRED_CODE,
            aft_engine::sandbox_escape::EXPIRED,
        ),
        aft_engine::sandbox_escape::WITHDRAWN_CODE | "access_withdrawn" => (
            aft_engine::sandbox_escape::WITHDRAWN_CODE,
            aft_engine::sandbox_escape::WITHDRAWN,
        ),
        _ => (
            aft_engine::sandbox_escape::UNAVAILABLE_CODE,
            aft_engine::sandbox_escape::UNAVAILABLE,
        ),
    }
}

pub fn refused_answer(call: &HeldCall, code: &str) -> Response {
    let (code, message) = refusal_for_code(code);
    Response::error(&call.request_id, code, message)
}

pub fn unavailable_answer(call: &HeldCall) -> Response {
    Response::error(
        &call.request_id,
        aft_engine::sandbox_escape::UNAVAILABLE_CODE,
        aft_engine::sandbox_escape::UNAVAILABLE,
    )
}

pub fn expired_answer(call: &HeldCall) -> Response {
    refused_answer(call, aft_engine::sandbox_escape::EXPIRED_CODE)
}

struct ObservedResponse {
    success: bool,
    code: Option<String>,
    status: Option<String>,
    exit_code: Option<i64>,
    task_id: Option<String>,
}

#[derive(Debug)]
struct Completion {
    decision: String,
    elicitation_id: Option<String>,
    execution: Option<&'static str>,
    reason: Option<&'static str>,
}

/// Settle the additive record and, only for an answered run_once card, report
/// whether the approved command launched. Refusals and expired cards have no
/// answered grant and therefore receive no execution report.
pub async fn complete(call: &HeldCall, result: Option<&(Response, Option<String>)>) {
    let Some(call_key) = take_call_key(call) else {
        return;
    };
    let response = result.map(|result| ObservedResponse {
        success: result.0.success,
        code: result
            .0
            .data
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: result
            .0
            .data
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        exit_code: result.0.data.get("exit_code").and_then(Value::as_i64),
        task_id: result
            .0
            .data
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    });
    let scope_ended = call
        .identity
        .run_scope
        .get()
        .is_some_and(|scope| scope.revoked());
    let detached = call.connection_cancel.is_cancelled() || call.drain.is_active();
    let completed = match with_db(call, move |conn, _| {
        let Some(row) = records::get(conn, &call_key)? else {
            return Ok(None);
        };
        let approved = row.decision == RUN_ONCE;
        let response_code = response.as_ref().and_then(|response| {
            (!response.success)
                .then_some(response.code.as_deref())
                .flatten()
        });
        let decision = if approved {
            RUN_ONCE
        } else {
            match response_code {
                Some(aft_engine::sandbox_escape::DECLINED_CODE) => DECLINE,
                Some(aft_engine::sandbox_escape::EXPIRED_CODE) => "expired",
                Some(aft_engine::sandbox_escape::WITHDRAWN_CODE) => "withdrawn",
                Some(_) => "unavailable",
                None if detached => "requested",
                None => "withdrawn",
            }
        };
        let terminal_status = response.as_ref().and_then(|response| {
            response.status.as_deref().filter(|status| {
                matches!(
                    *status,
                    "completed" | "failed" | "killed" | "timed_out" | "fate_unknown"
                )
            })
        });
        let exit_status = approved.then(|| {
            response.as_ref().map_or("outcome_unknown", |response| {
                response
                    .status
                    .as_deref()
                    .or(response_code)
                    .unwrap_or("completed")
            })
        });
        let exit_code = response.as_ref().and_then(|response| response.exit_code);
        let task_id = response
            .as_ref()
            .and_then(|response| response.task_id.as_deref());
        records::finish(conn, &call_key, decision, exit_status, exit_code, task_id)?;
        let execution = approved.then(|| {
            if response.is_none() {
                "outcome_unknown"
            } else if response.as_ref().is_some_and(|response| {
                response.success || terminal_status.is_some() || response.task_id.is_some()
            }) {
                "executed"
            } else {
                "approved_not_executed"
            }
        });
        let reason = (execution == Some("approved_not_executed")).then(|| {
            if scope_ended {
                "session_ended"
            } else if !Path::new(&row.cwd).is_dir() {
                "target_gone"
            } else {
                "refused_at_dispatch"
            }
        });
        Ok(Some(Completion {
            decision: decision.to_owned(),
            elicitation_id: row.elicitation_id,
            execution,
            reason,
        }))
    })
    .await
    {
        Ok(completion) => completion,
        Err(error) => {
            log::warn!("sandbox escape completion record failed: {error}");
            None
        }
    };
    let Some(completed) = completed else {
        return;
    };
    debug_assert!(!completed.decision.is_empty());
    let (Some(elicitation_id), Some(execution)) = (completed.elicitation_id, completed.execution)
    else {
        return;
    };
    let Some(transport) = card_transport() else {
        log::warn!("sandbox escape execution report has no consent transport");
        return;
    };
    let Some(stamp) = consent_identity(&call.identity).and_then(head_stamp) else {
        return;
    };
    let mut params = json!({"elicitation_id": elicitation_id, "execution": execution});
    if let Some(reason) = completed.reason {
        params["reason"] = json!(reason);
    }
    let report = ConsentCall {
        scope: selector(stamp),
        project_root: call.identity.project_root.clone(),
        session: call.identity.session.clone(),
        method: "consent.report_execution",
        params,
        timeout: UNARY_TIMEOUT,
    };
    if let Err(error) = transport.call(report).await {
        // Once an answered card has been acknowledged, Cingulate may delete its
        // retained record (prune it). No card is then left to attach an execution
        // report to, so reporting is too late. Other failures are also not
        // retried here: a duplicate execution report is never a reason to rerun
        // the approved command.
        log::warn!("sandbox escape execution report failed: {error:?}");
    }
}
