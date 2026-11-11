//! Handing back a waiting call: the tool-provider contract's `call-yield/v1`.
//!
//! A session runner sends `tool.yield` when a person sends a message while a
//! tool call is still running, so the agent can read the message instead of
//! waiting for the call. AFT hands back two kinds of call, and never cancels
//! the work either one waits for:
//!
//! - a `bash` call waiting for its command in the foreground: its command
//!   becomes an ordinary background task, the call answers at once with the
//!   task id, and the task's completion later reaches the launching session as
//!   a notice, like any background task's;
//! - a `bash_watch` call: the watch ends with what it has seen so far, as when
//!   its own time limit passes, and the task is left untouched.
//!
//! A runner restart instead retains a running held reply and delivers the
//! command or watch's terminal result through the call ledger, without a notice.
//!
//! Every other call answers `not_yieldable`. The contract is §4.6 of
//! `CONTRACT.md` in `cortexkit-role-tool-provider`; its strings, request
//! parsing and answer shapes come from that crate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use cortexkit_role_tool_provider::call_yield::YieldAnswer as WireAnswer;
use cortexkit_role_tool_provider::errors;
use serde_json::Value;
use subc_protocol::ErrorBody;

use super::RouteChannel;

/// The provider capability (manifest and `role.describe`) promising
/// `tool.yield`, and the route-bind capability a consumer declares to opt in.
pub(crate) use cortexkit_role_tool_provider::CALL_YIELD_V1 as CALL_YIELD;

/// The catalog tag on each tool whose waiting calls can be handed back.
pub(crate) use cortexkit_role_tool_provider::CALL_YIELD_TOOL_TAG;

/// The role operation asking a waiting call to hand back.
pub(crate) use cortexkit_role_tool_provider::ops::TOOL_YIELD;

/// A person's steer uses the ordinary background handoff, not a held reply.
pub(crate) use cortexkit_role_tool_provider::call_yield::PERSON_MESSAGE;

pub(super) const RUNNER_RESTART: &str = "runner_restart";

/// Whether a route's consumer opted in to call yielding. The contract asks
/// for all three of `tool-result/v1`, `held-reply/v1` and `call-yield/v1`.
pub(crate) use cortexkit_role_tool_provider::consumer_reads_call_yield_v1 as consumer_reads_call_yield;

/// The tools whose waiting calls AFT can hand back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum YieldableTool {
    Bash,
    BashWatch,
}

impl YieldableTool {
    pub(super) fn of(name: &str) -> Option<Self> {
        match name {
            "bash" => Some(Self::Bash),
            "bash_watch" => Some(Self::BashWatch),
            _ => None,
        }
    }
}

/// A decoded `tool.yield` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct YieldRequest {
    pub(super) call_key: String,
    pub(super) reason: String,
}

/// Decode `{name: "tool.yield", arguments: {call_key, reason}}` with the
/// role crate's checks. A malformed request is refused as `invalid_request`
/// naming the field.
pub(super) fn parse_request(body: &Value) -> Result<YieldRequest, ErrorBody> {
    let request: cortexkit_role_tool_provider::call::ToolCallRequest =
        serde_json::from_value(body.clone())
            .map_err(|error| errors::invalid_request("arguments", error.to_string()))?;
    let arguments = cortexkit_role_tool_provider::call_yield::parse_yield_request(&request)?;
    Ok(YieldRequest {
        call_key: arguments.call_key,
        reason: arguments.reason,
    })
}

/// Why an open call cannot hand back now. Each is sent as the
/// `not_yieldable` answer's `reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotYieldable {
    /// The route's consumer did not declare `call-yield/v1` (with
    /// `tool-result/v1` and `held-reply/v1`) when it opened the route.
    NotNegotiated,
    /// The call is not a `bash` or `bash_watch` call.
    Tool,
    /// The call is not waiting: a `bash` call still starting its command or
    /// already sending its result, or a watch that already ended.
    NotWaiting,
    /// The call belongs to a delegated worker or a codemode run, which keep
    /// holding their calls.
    Scope,
    /// The route never receives a task's completion notice, so a handed-back
    /// command's result would not reach the session.
    NoNotices,
    /// A reason other than [`PERSON_MESSAGE`].
    Reason,
}

impl NotYieldable {
    fn code(self) -> &'static str {
        match self {
            Self::NotNegotiated => "not_negotiated",
            Self::Tool => "tool_not_yieldable",
            Self::NotWaiting => "not_waiting",
            Self::Scope => "scope_not_yieldable",
            Self::NoNotices => "no_completion_notices",
            Self::Reason => "reason_not_supported",
        }
    }
}

/// Why no call on this route can hand back, or `None` when its calls may.
///
/// A route qualifies when its consumer opted in, it receives task completion
/// notices (so a handed-back command's result still reaches the session), and
/// it is a head agent's route. A delegated worker's or a codemode run's route
/// keeps holding its calls as before: the contract leaves scopes to the
/// provider, and those callers have no session that a notice could wake.
pub(super) fn route_refusal(identity: &super::RouteIdentity) -> Option<NotYieldable> {
    if !identity.call_yield {
        return Some(NotYieldable::NotNegotiated);
    }
    if !identity.trust.allows_bash_observation() {
        return Some(NotYieldable::NoNotices);
    }
    let head = identity
        .scope
        .as_ref()
        .is_some_and(|scope| scope.kind == subc_protocol::scope::ScopeKind::Head);
    if !head || identity.run_owner.is_some() || identity.run_scope.get().is_some() {
        return Some(NotYieldable::Scope);
    }
    None
}

pub(super) fn restart_route_refusal(identity: &super::RouteIdentity) -> Option<NotYieldable> {
    (!identity.call_yield).then_some(NotYieldable::NotNegotiated)
}

/// The bare answer to `tool.yield`; never a route error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum YieldAnswer {
    /// The call's final frame follows within five seconds, the contract's
    /// deadline for an accepted yield.
    Accepted,
    NotYieldable(NotYieldable),
    /// No open call under that key for this carrier on this route: never
    /// admitted, already answered, or already handed back.
    UnknownCall,
}

impl YieldAnswer {
    pub(super) fn body(self) -> Value {
        let answer = match self {
            Self::Accepted => WireAnswer::Accepted,
            Self::NotYieldable(reason) => WireAnswer::NotYieldable {
                reason: Some(reason.code().to_owned()),
            },
            Self::UnknownCall => WireAnswer::UnknownCall,
        };
        serde_json::to_value(answer).expect("yield answers are JSON")
    }
}

/// The yield signals of the `bash_watch` calls waiting right now, by request.
///
/// The watch's own thread holds the only strong reference, so an entry stops
/// upgrading once that watch has ended; dead entries are pruned whenever a
/// new watch registers.
#[derive(Default)]
pub(super) struct WatchYields {
    watches: Mutex<HashMap<(RouteChannel, u64), Weak<crate::commands::bash_watch::YieldSignal>>>,
}

impl WatchYields {
    /// A new signal for the watch answering request `corr` on `route`.
    pub(super) fn register(
        &self,
        route: RouteChannel,
        corr: u64,
    ) -> Arc<crate::commands::bash_watch::YieldSignal> {
        let signal = Arc::new(crate::commands::bash_watch::YieldSignal::default());
        let mut watches = self.lock();
        watches.retain(|_, watch| watch.strong_count() > 0);
        watches.insert((route, corr), Arc::downgrade(&signal));
        signal
    }

    /// Ask the watch answering `corr` on `route` to end now. False when no
    /// such watch is still waiting, or it was already asked.
    pub(super) fn request(&self, route: RouteChannel, corr: u64) -> bool {
        let signal = self.lock().get(&(route, corr)).and_then(Weak::upgrade);
        signal.is_some_and(|signal| signal.request())
    }

    pub(super) fn waiting(
        &self,
        route: RouteChannel,
        corr: u64,
    ) -> Option<Arc<crate::commands::bash_watch::YieldSignal>> {
        self.lock().get(&(route, corr)).and_then(Weak::upgrade)
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        HashMap<(RouteChannel, u64), Weak<crate::commands::bash_watch::YieldSignal>>,
    > {
        self.watches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
use crate::commands::bash_watch::restart::{copy_watch_response, watch_is_held};
pub(crate) use crate::commands::bash_watch::restart::{
    watch_can_hold, watch_finished, watch_unknown, WatchWait,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn hold_watch(
    signal: Arc<crate::commands::bash_watch::YieldSignal>,
    key: crate::db::call_ledger::Key,
    identity: &super::RouteIdentity,
    route: RouteChannel,
    corr: u64,
    ver: u8,
    flags: subc_protocol::Flags,
) -> Result<subc_protocol::Frame, super::SubcError> {
    let frame = restart_held_frame(ver, route, corr, flags, None)?;
    let retained = frame.body.clone();
    let structured = !matches!(identity.trust, super::BindTrust::Untrusted);
    let reset_signal = signal.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut state = signal.restart.lock().unwrap();
        let wait = state.wait.as_ref().ok_or("watch is no longer waiting")?;
        let db = wait.ctx.db().ok_or("Call ledger is unavailable")?;
        let mut body: Value = serde_json::from_slice(&retained).map_err(|e| e.to_string())?;
        body["_aft_restart_watch"] = wait.checkpoint();
        body["_aft_watch_incarnation"] =
            serde_json::json!(crate::db::call_ledger::provider_incarnation());
        body["_aft_include_structured"] = serde_json::json!(structured);
        {
            let conn = db.lock().map_err(|e| e.to_string())?;
            crate::db::call_ledger::hold_watch(&conn, &key, &body.to_string().into_bytes())
                .map_err(|e| e.to_string())?;
        }
        if state.unknown {
            let conn = db.lock().map_err(|e| e.to_string())?;
            crate::db::call_ledger::settle(
                &conn,
                &key,
                &crate::db::call_ledger::unknown_held_result(),
                "outcome_unknown",
                None,
                crate::db::call_ledger::now_ms(),
            )
            .map_err(|e| e.to_string())?;
        } else if let Some(response) = state.response.take() {
            let wait = state.wait.take().unwrap();
            settle_watch(&wait, &key, structured, response);
        } else {
            state.held = Some((key, structured));
        }
        Ok(())
    })
    .await
    .map_err(|_| super::SubcError::WriterClosed)?
    .map_err(|error| {
        reset_signal.restart.lock().unwrap().reserved = false;
        log::warn!("watch hold persistence failed: {error}");
        super::SubcError::WriterClosed
    })?;
    Ok(frame)
}

pub(super) fn restart_held_frame(
    ver: u8,
    route: RouteChannel,
    corr: u64,
    flags: subc_protocol::Flags,
    detail: Option<Value>,
) -> Result<subc_protocol::Frame, super::SubcError> {
    let frame = super::wire::build_held_frame(ver, route, corr, flags, "running", detail)?;
    let mut body: Value = serde_json::from_slice(&frame.body).map_err(super::SubcError::Json)?;
    body["held"]["yielded"] = serde_json::json!(true);
    subc_protocol::Frame::build_with_version(
        ver,
        frame.header.ty,
        flags,
        route.channel,
        route.epoch,
        corr,
        serde_json::to_vec(&body).map_err(super::SubcError::Json)?,
    )
    .map_err(super::SubcError::FrameBuild)
}

pub(crate) fn settle_watch(
    wait: &WatchWait,
    key: &crate::db::call_ledger::Key,
    structured: bool,
    response: crate::protocol::Response,
) {
    let root = wait.ctx.config().project_root.clone().unwrap_or_default();
    let format = crate::subc_format::FormatContext::from_tool_call(
        "bash_watch",
        &wait.request.params,
        &root,
    );
    let finalizer = |response: &mut crate::protocol::Response, text: &mut String| {
        crate::response_finalize::finalize_tool_response(
            response,
            text,
            &wait.ctx,
            wait.request.session(),
            "bash_watch",
            structured,
        );
    };
    let result = crate::run_tool_call::finish_tool_call_response(
        "bash_watch",
        &format,
        response,
        false,
        Some(&finalizer),
        None,
    );
    let stored = (|| -> Result<(), String> {
        let mut outgoing = super::build_tool_response_frame(
            subc_protocol::PROTOCOL_VERSION,
            super::route_key(1, 1),
            1,
            super::control_flags(),
            &result,
            super::wire::ToolResponsePolicy::for_retention(),
        )
        .map_err(|e| e.to_string())?;
        let source = outgoing
            .tool_result_source_for_retention()
            .map_err(|e| e.to_string())?;
        outgoing.project_tool_result().map_err(|e| e.to_string())?;
        let frame = crate::db::call_ledger::RecordedFrame {
            ty: outgoing.header.ty,
            body: outgoing.frame.body,
        };
        let frame = super::project_held_ledger_result(&frame, source.as_deref(), structured)?;
        let db = wait.ctx.db().ok_or("Call ledger is unavailable")?;
        let conn = db.lock().map_err(|e| e.to_string())?;
        crate::db::call_ledger::settle_with_tool_result_source(
            &conn,
            key,
            &frame,
            &crate::db::call_ledger::outcome(&frame),
            None,
            crate::db::call_ledger::now_ms(),
            source.as_deref(),
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(error) = stored {
        log::warn!("held watch settlement failed: {error}");
    }
}

/// Recreate only waits whose original deadline is still knowable. If a timer
/// or pattern could have fired while AFT was down, reporting a fresh wait's
/// result would invent an outcome; retain the existing unknown-outcome result.
pub(super) fn recover_watches(ctx: Arc<crate::context::AppContext>) {
    use crate::db::call_ledger as ledger;
    let Some(db) = ctx.db() else {
        return;
    };
    let rows = (|| -> rusqlite::Result<Vec<(ledger::Key, Value)>> {
        let conn = db.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        let conn =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        let mut stmt = conn.prepare("SELECT carrier,call_key,frame_body FROM call_ledger WHERE state='DispatchStarted' AND frame_body IS NOT NULL")?;
        let candidates = stmt
            .query_map([], |row| {
                Ok((
                    ledger::Key {
                        carrier: row.get(0)?,
                        call_key: row.get(1)?,
                    },
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut waits = Vec::new();
        for (key, bytes) in candidates {
            let Ok(mut body) = serde_json::from_slice::<Value>(&bytes) else {
                continue;
            };
            if body.get("_aft_restart_watch").is_none()
                || body["_aft_watch_incarnation"] == ledger::provider_incarnation()
            {
                continue;
            }
            body["_aft_watch_incarnation"] = serde_json::json!(ledger::provider_incarnation());
            ledger::hold_watch(&conn, &key, &body.to_string().into_bytes())?;
            waits.push((key, body));
        }
        drop(stmt);
        conn.commit()?;
        Ok(waits)
    })();
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            log::warn!("held watch recovery failed: {error}");
            return;
        }
    };
    for (key, body) in rows {
        let saved = &body["_aft_restart_watch"];
        let now_ms = ledger::now_ms();
        let timing = (|| {
            let started_ms = saved["started_ms"].as_i64()?;
            let deadline_ms = saved["deadline_ms"].as_i64()?;
            let limit_ms = saved["limit_ms"].as_u64()?;
            if now_ms < started_ms
                || now_ms >= deadline_ms
                || now_ms - started_ms >= i64::try_from(limit_ms).ok()?
                || saved["params"]["pattern"].is_object()
                || saved["params"]["pattern"].is_string()
                || saved["worker_limit_ms"]
                    != crate::commands::bash_orchestrate::worker_wait_max_ms(&ctx)
                || saved["primary_cap_ms"] != ctx.config().bash.watch_sync_max_ms
            {
                return None;
            }
            let now = std::time::Instant::now();
            Some((
                crate::commands::blocking_wait::ReplyBudget {
                    received_at: now.checked_sub(std::time::Duration::from_millis(
                        (now_ms - started_ms) as u64,
                    ))?,
                    deadline: now.checked_add(std::time::Duration::from_millis(
                        (deadline_ms - now_ms) as u64,
                    ))?,
                },
                limit_ms,
            ))
        })();
        let Some((budget, limit_ms)) = timing else {
            if let Ok(conn) = db.lock() {
                let _ = ledger::settle(
                    &conn,
                    &key,
                    &ledger::unknown_held_result(),
                    "outcome_unknown",
                    None,
                    now_ms,
                );
            }
            continue;
        };
        let request = crate::protocol::RawRequest {
            id: saved["id"].as_str().unwrap_or_default().to_owned(),
            command: "bash_watch".into(),
            lsp_hints: None,
            session_id: saved["session"].as_str().map(str::to_owned),
            params: saved["params"].clone(),
        };
        let signal = Arc::new(crate::commands::bash_watch::YieldSignal::default());
        {
            let mut state = signal.restart.lock().unwrap();
            state.reserved = true;
            state.held = Some((key.clone(), body["_aft_include_structured"] == true));
            state.resume = Some((budget, limit_ms));
            state.wait = Some(WatchWait::new(
                ctx.clone(),
                request.clone(),
                budget,
                limit_ms,
            ));
        }
        let principal = serde_json::from_value(saved["principal"].clone());
        let Ok(principal) = principal else {
            watch_unknown(&signal);
            continue;
        };
        let outcome = crate::sandbox_spawn::with_authenticated_principal(principal, || {
            crate::commands::bash_watch::handle_deferred_yieldable(
                &request,
                ctx.clone(),
                budget,
                false,
                Some(signal.clone()),
            )
        });
        if let crate::response_finalize::DispatchOutcome::Immediate(response) = outcome {
            watch_finished(&signal, &response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn recovery_context(
        root: &std::path::Path,
        storage: &std::path::Path,
    ) -> Arc<crate::context::AppContext> {
        let ctx = Arc::new(crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.into()),
                storage_dir: Some(storage.into()),
                harness: Some(crate::harness::Harness::Runner),
                experimental_bash_background: true,
                ..Default::default()
            },
        ));
        ctx.update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
        let db = Arc::new(Mutex::new(
            crate::db::open(&storage.join("aft.db")).unwrap(),
        ));
        ctx.set_db(db.clone());
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Runner);
        ctx.bash_background().set_db_pool(db);
        ctx
    }

    #[test]
    fn runner_restart_watch_late_result_is_byte_identical_to_an_ordinary_final_frame() {
        use crate::db::call_ledger as ledger;
        let dir = tempfile::tempdir().unwrap();
        let ctx = recovery_context(dir.path(), &dir.path().join("storage"));
        let request = serde_json::from_value(json!({"id":"watch", "command":"bash_watch", "session_id":"worker", "task_id":"bash-fixture", "timeout_ms":3000})).unwrap();
        let wait = WatchWait::new(
            ctx.clone(),
            request,
            crate::commands::blocking_wait::ReplyBudget::new(std::time::Instant::now(), None),
            3000,
        );
        let key = ledger::Key {
            carrier: "broca".into(),
            call_key: "watch".into(),
        };
        let scope = ledger::ScopeIdentity {
            owner: "core".into(),
            scope_ref: "worker".into(),
            scope_epoch: 1,
        };
        ledger::admit(
            &ctx.db().unwrap().lock().unwrap(),
            &key,
            "digest",
            Some(&scope),
            ledger::State::DispatchStarted,
        )
        .unwrap();
        let response = crate::protocol::Response::success(
            "watch",
            json!({"output":"real watch output", "status":"completed", "waited":{"elapsed_ms":123,"limit_ms":3000,"reason":"exited"}}),
        );
        let format = crate::subc_format::FormatContext::from_tool_call(
            "bash_watch",
            &wait.request.params,
            dir.path(),
        );
        let finalizer = |response: &mut crate::protocol::Response, text: &mut String| {
            crate::response_finalize::finalize_tool_response(
                response,
                text,
                &ctx,
                "worker",
                "bash_watch",
                true,
            );
        };
        let ordinary = crate::run_tool_call::finish_tool_call_response(
            "bash_watch",
            &format,
            copy_watch_response(&response),
            false,
            Some(&finalizer),
            None,
        );
        let mut ordinary = super::super::build_tool_response_frame(
            subc_protocol::PROTOCOL_VERSION,
            super::super::route_key(1, 1),
            1,
            super::super::control_flags(),
            &ordinary,
            super::super::wire::ToolResponsePolicy::for_retention(),
        )
        .unwrap();
        ordinary.project_tool_result().unwrap();
        settle_watch(&wait, &key, true, response);
        let row = ledger::get(&ctx.db().unwrap().lock().unwrap(), &key)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, ledger::State::Settled);
        assert_eq!(row.frame.unwrap().body, ordinary.frame.body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_restart_watch_reattaches_after_aft_restart_without_resetting_its_timer() {
        crate::subc::lower_hooks::register();
        use crate::db::call_ledger as ledger;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&root).unwrap();
        let old = recovery_context(&root, &storage);
        let release = root.join("release");
        let command = format!(
            "while [ ! -f '{}' ]; do sleep 0.02; done; echo restart-released",
            release.display()
        );
        let req = serde_json::from_value(json!({"id":"launch", "command":"bash", "session_id":"restart-worker", "params":{"command":command,"background":true,"timeout":20000}})).unwrap();
        let launched = crate::commands::bash::handle(&req, &old);
        assert!(launched.success, "{launched:?}");
        let task = launched.data["task_id"].as_str().unwrap().to_owned();
        let metadata = crate::bash_background::persistence::read_task(
            &old.bash_background()
                .task_json_path(&task, "restart-worker")
                .unwrap(),
        )
        .unwrap();
        struct StopChild(i32);
        impl Drop for StopChild {
            fn drop(&mut self) {
                let _ = crate::bash_background::process::terminate_pgid(self.0, None);
            }
        }
        let _stop = StopChild(metadata.pgid.unwrap());
        let watch: crate::protocol::RawRequest = serde_json::from_value(json!({"id":"watch", "command":"bash_watch", "session_id":"restart-worker", "task_id":task,"timeout_ms":3000})).unwrap();
        let budget = crate::commands::blocking_wait::ReplyBudget {
            received_at: std::time::Instant::now() - std::time::Duration::from_millis(750),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
        };
        let wait = WatchWait::new(old.clone(), watch, budget, 3000);
        let key = ledger::Key {
            carrier: "broca".into(),
            call_key: "held-watch".into(),
        };
        let scope = ledger::ScopeIdentity {
            owner: "core".into(),
            scope_ref: "worker".into(),
            scope_epoch: 1,
        };
        let mut held = json!({"content":[{"type":"text","text":""}],"held":{"reason":"running","yielded":true},"_aft_restart_watch":wait.checkpoint(),"_aft_watch_incarnation":"old-process","_aft_include_structured":true});
        {
            let db = old.db().unwrap();
            let conn = db.lock().unwrap();
            ledger::admit(
                &conn,
                &key,
                "digest",
                Some(&scope),
                ledger::State::DispatchStarted,
            )
            .unwrap();
            ledger::hold_watch(&conn, &key, &serde_json::to_vec(&held).unwrap()).unwrap();
        }
        old.bash_background().detach();
        drop(wait);
        drop(old);
        let restarted = recovery_context(&root, &storage);
        ledger::recover(&restarted.db().unwrap().lock().unwrap()).unwrap();
        assert_eq!(
            ledger::get(&restarted.db().unwrap().lock().unwrap(), &key)
                .unwrap()
                .unwrap()
                .state,
            ledger::State::DispatchStarted
        );
        recover_watches(restarted.clone());
        std::fs::write(release, "").unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let result = loop {
            let row = ledger::get(&restarted.db().unwrap().lock().unwrap(), &key)
                .unwrap()
                .unwrap();
            if row.state == ledger::State::Settled {
                break serde_json::from_slice::<Value>(&row.frame.unwrap().body).unwrap();
            }
            assert!(
                std::time::Instant::now() < until,
                "recovered watch did not settle"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_ne!(result["isError"], true, "{result}");
        assert!(
            result["content"].to_string().contains("restart-released"),
            "{result}"
        );
        assert!(
            result["structuredContent"]["waited"]["elapsed_ms"]
                .as_u64()
                .unwrap()
                >= 750,
            "{result}"
        );
        assert_eq!(result["structuredContent"]["waited"]["limit_ms"], 3000);
        let unknown = ledger::Key {
            carrier: "broca".into(),
            call_key: "expired-watch".into(),
        };
        held["_aft_restart_watch"]["started_ms"] = json!(ledger::now_ms() - 6000);
        {
            let db = restarted.db().unwrap();
            let conn = db.lock().unwrap();
            ledger::admit(
                &conn,
                &unknown,
                "other",
                Some(&scope),
                ledger::State::DispatchStarted,
            )
            .unwrap();
            ledger::hold_watch(&conn, &unknown, &serde_json::to_vec(&held).unwrap()).unwrap();
        }
        recover_watches(restarted.clone());
        let row = ledger::get(&restarted.db().unwrap().lock().unwrap(), &unknown)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, ledger::State::Settled);
        assert_eq!(row.frame.unwrap().body, ledger::unknown_held_result().body);
    }

    #[test]
    fn runner_restart_refuses_calls_that_have_not_reached_their_wait() {
        let route = super::super::route_key(1, 1);
        let calls = super::super::drain::HeldBashCalls::default();
        let _claim = calls.insert(route, 7);
        assert!(matches!(
            calls.claim_for_yield(route, 7, true),
            Err(NotYieldable::NotWaiting)
        ));
        let signal = crate::commands::bash_watch::YieldSignal::default();
        assert!(!watch_can_hold(&signal));
        assert!(!watch_is_held(&signal));
    }

    /// The role crate's shared `call-yield.json` vectors: every request the
    /// contract lists is accepted or refused naming the same field.
    #[test]
    fn requests_follow_the_shared_vectors() {
        let vectors: Value =
            serde_json::from_str(cortexkit_role_tool_provider::vectors::CALL_YIELD_JSON).unwrap();
        let requests = vectors["requests"].as_array().unwrap();
        assert!(!requests.is_empty());
        for case in requests {
            let parsed = parse_request(&case["request"]);
            if case["valid"] == true {
                let parsed = parsed.unwrap_or_else(|error| panic!("{case}: {error:?}"));
                assert_eq!(parsed.call_key, case["request"]["arguments"]["call_key"]);
                assert_eq!(parsed.reason, case["request"]["arguments"]["reason"]);
            } else {
                let error = parsed.unwrap_err();
                assert_eq!(error.code, "invalid_request", "{case}");
                assert_eq!(error.detail.unwrap()["field"], case["field"], "{case}");
            }
        }
        // A body that is not a request at all is refused naming `arguments`.
        let error = parse_request(&json!({"name": "tool.yield"})).unwrap_err();
        assert_eq!(error.detail.unwrap()["field"], "arguments");
    }

    #[test]
    fn answers_have_the_contract_shapes() {
        assert_eq!(YieldAnswer::Accepted.body(), json!({"answer": "accepted"}));
        assert_eq!(
            YieldAnswer::UnknownCall.body(),
            json!({"answer": "unknown_call"})
        );
        assert_eq!(
            YieldAnswer::NotYieldable(NotYieldable::Tool).body(),
            json!({"answer": "not_yieldable", "reason": "tool_not_yieldable"})
        );
    }

    #[test]
    fn opting_in_needs_all_three_consumer_capabilities() {
        let caps = |list: &[&str]| list.iter().map(|cap| cap.to_string()).collect::<Vec<_>>();
        assert!(consumer_reads_call_yield(Some(&caps(&[
            "tool-result/v1",
            "held-reply/v1",
            "call-yield/v1"
        ]))));
        for missing in [
            caps(&["held-reply/v1", "call-yield/v1"]),
            caps(&["tool-result/v1", "call-yield/v1"]),
            caps(&["tool-result/v1", "held-reply/v1"]),
        ] {
            assert!(!consumer_reads_call_yield(Some(&missing)), "{missing:?}");
        }
        assert!(!consumer_reads_call_yield(None));
    }

    #[test]
    fn a_watch_is_asked_once_and_only_while_it_waits() {
        let yields = WatchYields::default();
        let route = super::super::route_key(1, 1);
        let signal = yields.register(route, 7);
        assert!(!yields.request(route, 8), "no watch answers corr 8");
        assert!(yields.request(route, 7));
        assert!(signal.requested());
        assert!(!yields.request(route, 7), "a watch is asked only once");
        drop(signal);
        let _other = yields.register(route, 9);
        assert!(!yields.request(route, 7), "an ended watch is not waiting");
    }
}
