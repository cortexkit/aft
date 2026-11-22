//! Watch restart state belongs to the waiting tool, not its delivery route.
use serde_json::Value;
use std::sync::{Arc, OnceLock};

use crate::commands::bash_watch::YieldSignal;
use crate::db::call_ledger as ledger;
use crate::protocol::Response;

/// A watch owns its wait independently of the route after a restart yield.
/// The mutex also orders a racing completion against writing the held wait to the ledger.
#[derive(Default)]
pub(super) struct WatchRestart {
    wait: Option<WatchWait>,
    held: Option<(ledger::Key, bool)>,
    reserved: bool,
    finished: bool,
    unknown: bool,
    response: Option<Response>,
    resume: Option<(crate::commands::blocking_wait::ReplyBudget, u64)>,
}

/// The original request, principal and timing of a watch that can survive a restart.
#[derive(Clone)]
pub struct WatchWait {
    ctx: Arc<crate::context::AppContext>,
    request: crate::protocol::RawRequest,
    budget: crate::commands::blocking_wait::ReplyBudget,
    limit_ms: u64,
    principal: crate::sandbox_spawn::AuthenticatedPrincipal,
}

impl WatchWait {
    /// Capture the current principal along with the request's wait budget.
    pub fn new(
        ctx: Arc<crate::context::AppContext>,
        request: crate::protocol::RawRequest,
        budget: crate::commands::blocking_wait::ReplyBudget,
        limit_ms: u64,
    ) -> Self {
        Self {
            ctx,
            request,
            budget,
            limit_ms,
            principal: crate::sandbox_spawn::current_authenticated_principal(),
        }
    }

    /// Copy the context handle used by the original watch.
    pub fn ctx(&self) -> Arc<crate::context::AppContext> {
        self.ctx.clone()
    }

    /// Copy the request as it was when the watch started.
    pub fn request(&self) -> crate::protocol::RawRequest {
        self.request.clone()
    }

    /// Encode the original wait and its deadline for restart recovery.
    pub fn checkpoint(&self) -> Value {
        let now = std::time::Instant::now();
        let wall = crate::db::call_ledger::now_ms();
        serde_json::json!({
            "id": self.request.id, "params": self.request.params,
            "session": self.request.session_id,
            "started_ms": wall - now.saturating_duration_since(self.budget.received_at).as_millis() as i64,
            "deadline_ms": wall + self.budget.deadline.saturating_duration_since(now).as_millis() as i64,
            "limit_ms": self.limit_ms,
            "principal": self.principal,
            "worker_limit_ms": crate::commands::bash_orchestrate::worker_wait_max_ms(&self.ctx),
            "primary_cap_ms": self.ctx.config().bash.watch_sync_max_ms,
        })
    }
}

pub(crate) fn watch_resume_timing(
    signal: &crate::commands::bash_watch::YieldSignal,
) -> Option<(crate::commands::blocking_wait::ReplyBudget, u64)> {
    signal.restart.lock().unwrap().resume
}

pub(crate) fn watch_waiting(signal: &crate::commands::bash_watch::YieldSignal, wait: WatchWait) {
    signal.restart.lock().unwrap().wait = Some(wait);
}

/// Reserve a waiting watch for handoff, before any ledger lock is taken.
pub fn reserve_watch_hold(signal: &YieldSignal) -> bool {
    let mut state = signal.restart.lock().unwrap();
    if state.wait.is_none() || state.finished || state.reserved {
        return false;
    }
    state.reserved = true;
    true
}

/// Release the reservation after a failed persistence attempt; takes only the restart lock.
pub fn clear_watch_hold(signal: &YieldSignal) {
    signal.restart.lock().unwrap().reserved = false;
}

/// Restore the reservation and original timing together; takes only the restart lock.
pub fn recover_watch_hold(
    signal: &YieldSignal,
    key: ledger::Key,
    structured: bool,
    wait: WatchWait,
) {
    let mut state = signal.restart.lock().unwrap();
    state.reserved = true;
    state.held = Some((key, structured));
    state.resume = Some((wait.budget, wait.limit_ms));
    state.wait = Some(wait);
}

/// Persist a reserved watch, or settle it if the response won the race.
/// Takes the restart lock first, then the call-ledger lock. The settlement
/// callback runs under the restart lock, with the call-ledger lock released.
pub fn persist_watch_hold(
    signal: &YieldSignal,
    key: ledger::Key,
    structured: bool,
    retained: &[u8],
    settle: impl FnOnce(&WatchWait, &ledger::Key, bool, Response),
) -> Result<(), String> {
    let mut state = signal.restart.lock().unwrap();
    let wait = state.wait.as_ref().ok_or("watch is no longer waiting")?;
    let db = wait.ctx.db().ok_or("Call ledger is unavailable")?;
    let mut body: Value = serde_json::from_slice(retained).map_err(|e| e.to_string())?;
    body["_aft_restart_watch"] = wait.checkpoint();
    body["_aft_watch_incarnation"] = serde_json::json!(ledger::provider_incarnation());
    body["_aft_include_structured"] = serde_json::json!(structured);
    {
        let conn = db.lock().map_err(|e| e.to_string())?;
        ledger::hold_watch(&conn, &key, &body.to_string().into_bytes())
            .map_err(|e| e.to_string())?;
    }
    if state.unknown {
        let conn = db.lock().map_err(|e| e.to_string())?;
        ledger::settle(
            &conn,
            &key,
            &ledger::unknown_held_result(),
            "outcome_unknown",
            None,
            ledger::now_ms(),
        )
        .map_err(|e| e.to_string())?;
    } else if let Some(response) = state.response.take() {
        let wait = state.wait.take().unwrap();
        settle(&wait, &key, structured, response);
    } else {
        state.held = Some((key, structured));
    }
    Ok(())
}

/// Report whether the restart path reserved the watch; takes only the restart lock.
pub fn watch_is_held(signal: &crate::commands::bash_watch::YieldSignal) -> bool {
    signal.restart.lock().unwrap().reserved
}

/// Publish the terminal reply, settling it immediately if the hold is already durable.
/// Takes the restart lock before any call-ledger lock.
pub fn watch_finished(
    signal: &crate::commands::bash_watch::YieldSignal,
    response: &crate::protocol::Response,
) -> bool {
    let mut state = signal.restart.lock().unwrap();
    state.finished = true;
    if !state.reserved {
        return false;
    }
    state.unknown |= state.resume.is_some() && !response.success;
    if let Some((key, structured)) = state.held.take() {
        if let Some(wait) = state.wait.take() {
            if state.unknown {
                if let Some(db) = wait.ctx.db() {
                    if let Ok(conn) = db.lock() {
                        let _ = crate::db::call_ledger::settle(
                            &conn,
                            &key,
                            &crate::db::call_ledger::unknown_held_result(),
                            "outcome_unknown",
                            None,
                            crate::db::call_ledger::now_ms(),
                        );
                    }
                }
            } else {
                settle_watch(&wait, &key, structured, copy_watch_response(response));
            }
        }
    } else {
        state.response = Some(copy_watch_response(response));
    }
    true
}

/// Copy a terminal watch reply, including any undelivered notices.
pub fn copy_watch_response(response: &crate::protocol::Response) -> crate::protocol::Response {
    crate::protocol::Response {
        id: response.id.clone(),
        success: response.success,
        data: response.data.clone(),
        notices: response.notices.clone(),
    }
}

/// Mark recovery as unknown; takes the restart lock before any call-ledger lock.
pub fn watch_unknown(signal: &crate::commands::bash_watch::YieldSignal) {
    let mut state = signal.restart.lock().unwrap();
    state.finished = true;
    state.unknown = true;
    if state.held.is_some() {
        if let (Some((key, _)), Some(wait)) = (state.held.take(), state.wait.take()) {
            if let Some(db) = wait.ctx.db() {
                if let Ok(conn) = db.lock() {
                    let _ = crate::db::call_ledger::settle(
                        &conn,
                        &key,
                        &crate::db::call_ledger::unknown_held_result(),
                        "outcome_unknown",
                        None,
                        crate::db::call_ledger::now_ms(),
                    );
                }
            }
        }
    }
}

type SettleWatch = fn(&WatchWait, &crate::db::call_ledger::Key, bool, crate::protocol::Response);
static SETTLE_WATCH: OnceLock<SettleWatch> = OnceLock::new();
/// Register the callback that settles completed restart-held watches.
pub fn register(settle: SettleWatch) {
    let _ = SETTLE_WATCH.set(settle);
}
#[cfg(test)]
pub(crate) fn registered() -> bool {
    SETTLE_WATCH.get().is_some()
}
fn settle_watch(
    wait: &WatchWait,
    key: &crate::db::call_ledger::Key,
    structured: bool,
    response: crate::protocol::Response,
) {
    if let Some(settle) = SETTLE_WATCH.get() {
        settle(wait, key, structured, response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_completion_between_reservation_and_persistence_is_settled_once() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        ));
        let db = Arc::new(std::sync::Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(db.clone());
        let key = ledger::Key {
            carrier: "test".into(),
            call_key: "watch".into(),
        };
        ledger::admit(
            &db.lock().unwrap(),
            &key,
            "digest",
            None,
            ledger::State::DispatchStarted,
        )
        .unwrap();

        let signal = YieldSignal::default();
        let request = serde_json::from_value(serde_json::json!({
            "id": "watch", "command": "bash_watch", "task_id": "test-task"
        }))
        .unwrap();
        watch_waiting(
            &signal,
            WatchWait::new(
                ctx,
                request,
                crate::blocking_wait::ReplyBudget::new(std::time::Instant::now(), None),
                3000,
            ),
        );
        assert!(reserve_watch_hold(&signal));
        assert!(watch_finished(
            &signal,
            &Response::success(
                "watch",
                serde_json::json!({
                    "status": "completed", "output": "finished while the hold was reserved"
                })
            )
        ));

        let mut settlement = None;
        persist_watch_hold(
            &signal,
            key.clone(),
            true,
            br#"{"held":{"yielded":true}}"#,
            |wait, settled_key, structured, response| {
                assert_eq!(settled_key, &key);
                assert!(structured);
                assert_eq!(wait.request().id, "watch");
                assert!(
                    db.try_lock().is_ok(),
                    "settlement must not inherit the ledger lock"
                );
                settlement = Some(response);
            },
        )
        .unwrap();
        let response = settlement.expect("the racing terminal reply must reach settlement");
        assert_eq!(
            response.data["output"],
            "finished while the hold was reserved"
        );
        assert!(persist_watch_hold(&signal, key, true, b"{}", |_, _, _, _| {
            panic!("the terminal reply was consumed by the first settlement");
        })
        .is_err());
    }
}
