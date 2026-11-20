//! Shell result bodies and optional daemon publication. Without registered
//! daemon hooks no result is captured for display or retained as reply JSON.
use serde_json::{json, Value};
use std::{path::Path, sync::OnceLock};
use subc_protocol::Principal;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub runner: Principal,
    pub call_key: String,
    pub schema_pin: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TaskDisplay {
    pub identity: Identity,
    pub capture_key: String,
    pub reply_produced: bool,
    pub deferred: bool,
    pub held_ms: Option<u64>,
    pub attempted: bool,
}

/// Build only scalar facts from execution metadata. Unknown fate never implies
/// an exit code; a kill AFT performed is distinguished from an ordinary exit.
pub(crate) fn bash_bodies(
    metadata: &crate::bash_background::persistence::PersistedTask,
    output_bytes: Option<u64>,
    streams: Option<(String, String)>,
) -> (Value, Value) {
    use crate::protocol::BgTaskStatus;
    let remote = metadata
        .remote
        .as_ref()
        .filter(|_| !metadata.local_fallback_started);
    let execution = if remote.is_some() { "remote" } else { "local" };
    let terminal = metadata.is_terminal();
    let mut record = json!({"outcome":if metadata.status == BgTaskStatus::Completed && metadata.exit_code == Some(0) {"success"} else if terminal {"error"} else {"success"},"phase":if terminal {"foreground_finished"} else {"background_started"},"execution":execution});
    let mut detail =
        json!({"schema":"aft/bash-detail/1","revision":1,"phase":"foreground_finished"});
    if let Some(remote) = remote {
        if let Some(platform) = remote
            .accepted_platform
            .as_ref()
            .or(remote.requested_platform.as_ref())
        {
            record["target"] = serde_json::to_value(platform).unwrap_or(Value::Null);
            detail["target"] = record["target"].clone();
        }
        detail["execution"] = json!("remote");
    }
    if let Some(bytes) = output_bytes {
        record["outputBytes"] = json!(bytes);
    }
    if !terminal {
        record["taskId"] = json!(metadata.task_id);
    }
    if metadata.status == BgTaskStatus::FateUnknown
        || (metadata.status == BgTaskStatus::Killed
            && (!metadata.group_kill_sent
                || metadata
                    .live_descendants
                    .as_ref()
                    .is_some_and(|processes| !processes.is_empty())))
    {
        record["reason"] = json!("fate_unknown");
    } else if terminal {
        let reason = match metadata.status {
            BgTaskStatus::Killed => "killed",
            BgTaskStatus::TimedOut => "timed_out",
            _ => "exited",
        };
        if reason != "exited"
            || metadata.exit_code.is_some()
            || metadata.status == BgTaskStatus::Completed
        {
            record["endReason"] = json!(reason);
        }
        if reason != "exited" {
            record["reason"] = json!(reason);
        }
        if let Some(exit) = metadata.exit_code {
            record["exitCode"] = json!(exit);
        }
    }
    if let Some((stdout, stderr)) = streams {
        if metadata.mode == crate::bash_background::persistence::BgMode::Pty {
            detail["pty"] = json!(stdout);
        } else {
            detail["stdout"] = json!(stdout);
            detail["stderr"] = json!(stderr);
        }
    } else {
        detail["outputUnavailable"] = json!("not_retained");
    }
    (record, detail)
}

pub(crate) type RecoveredFrame = (
    crate::db::call_ledger::RecordedFrame,
    String,
    Option<Vec<u8>>,
);

pub(crate) struct Hooks {
    pub enabled: fn() -> bool,
    pub current_task: fn() -> Option<TaskDisplay>,
    pub bash_reply: fn(&mut TaskDisplay, (Value, Value), bool),
    pub bash_terminal: fn(&TaskDisplay, (Value, Value), &str, Option<u64>),
    pub recovered_bash_frame: fn(
        super::registry::BgTaskSnapshot,
        &crate::db::call_ledger::Key,
        &Path,
        Option<crate::output_store::OutputScope>,
    ) -> Option<RecoveredFrame>,
}
static HOOKS: OnceLock<Hooks> = OnceLock::new();

pub(crate) fn register(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}
#[cfg(test)]
pub(crate) fn registered() -> bool {
    HOOKS.get().is_some()
}

pub(crate) fn enabled() -> bool {
    HOOKS.get().is_some_and(|hooks| (hooks.enabled)())
}
pub(crate) fn current_task() -> Option<TaskDisplay> {
    HOOKS.get().and_then(|hooks| (hooks.current_task)())
}
pub(crate) fn bash_reply(display: &mut TaskDisplay, body: (Value, Value), deferred: bool) {
    if let Some(hooks) = HOOKS.get() {
        (hooks.bash_reply)(display, body, deferred);
    }
}
pub(crate) fn bash_terminal(
    display: &TaskDisplay,
    body: (Value, Value),
    task_id: &str,
    duration_ms: Option<u64>,
) {
    if let Some(hooks) = HOOKS.get() {
        (hooks.bash_terminal)(display, body, task_id, duration_ms);
    }
}
pub(crate) fn recovered_bash_frame(
    snapshot: super::registry::BgTaskSnapshot,
    key: &crate::db::call_ledger::Key,
    root: &Path,
    scope: Option<crate::output_store::OutputScope>,
) -> Option<RecoveredFrame> {
    HOOKS
        .get()
        .and_then(|hooks| (hooks.recovered_bash_frame)(snapshot, key, root, scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_display_reads_legacy_json_byte_identically() {
        const LEGACY: &[u8] = br#"{"identity":{"runner":{"kind":"reserved","module_id":"broca"},"call_key":"call","schema_pin":"tp1:bash:fixture"},"capture_key":"capture","reply_produced":true,"deferred":true,"held_ms":120,"attempted":false}"#;
        let display: TaskDisplay = serde_json::from_slice(LEGACY).unwrap();
        assert_eq!(serde_json::to_vec(&display).unwrap(), LEGACY);
    }
}
