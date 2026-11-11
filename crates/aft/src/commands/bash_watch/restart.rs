//! Watch restart state belongs to the waiting tool, not its delivery route.
use serde_json::Value;
use std::sync::{Arc, OnceLock};

/// A watch owns its wait independently of the route after a restart yield.
/// The mutex also orders a racing completion against writing the held wait to the ledger.
#[derive(Default)]
pub(crate) struct WatchRestart {
    pub(crate) wait: Option<WatchWait>,
    pub(crate) held: Option<(crate::db::call_ledger::Key, bool)>,
    pub(crate) reserved: bool,
    pub(crate) finished: bool,
    pub(crate) unknown: bool,
    pub(crate) response: Option<crate::protocol::Response>,
    pub(crate) resume: Option<(crate::commands::blocking_wait::ReplyBudget, u64)>,
}

#[derive(Clone)]
pub(crate) struct WatchWait {
    pub(crate) ctx: Arc<crate::context::AppContext>,
    pub(crate) request: crate::protocol::RawRequest,
    pub(crate) budget: crate::commands::blocking_wait::ReplyBudget,
    pub(crate) limit_ms: u64,
    pub(crate) principal: crate::sandbox_spawn::AuthenticatedPrincipal,
}

impl WatchWait {
    pub(crate) fn new(
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

    pub(crate) fn checkpoint(&self) -> Value {
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

pub(crate) fn watch_can_hold(signal: &crate::commands::bash_watch::YieldSignal) -> bool {
    let mut state = signal.restart.lock().unwrap();
    if state.wait.is_none() || state.finished || state.reserved {
        return false;
    }
    state.reserved = true;
    true
}

pub(crate) fn watch_is_held(signal: &crate::commands::bash_watch::YieldSignal) -> bool {
    signal.restart.lock().unwrap().reserved
}

pub(crate) fn watch_finished(
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

pub(crate) fn copy_watch_response(
    response: &crate::protocol::Response,
) -> crate::protocol::Response {
    crate::protocol::Response {
        id: response.id.clone(),
        success: response.success,
        data: response.data.clone(),
    }
}

pub(crate) fn watch_unknown(signal: &crate::commands::bash_watch::YieldSignal) {
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
pub(crate) fn register(settle: SettleWatch) {
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
