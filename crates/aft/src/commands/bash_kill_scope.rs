use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use subc_protocol::Principal;

use crate::bash_background::registry::BgTaskRegistry;
use crate::protocol::Response;

pub(crate) const BASH_KILL_SCOPE_OPERATION: &str = "bash.kill_scope";

/// Stop a cancelled worker's bash tasks without revoking its scope.
///
/// Only the tasks held by `registries` are stopped. The module's frame loop
/// uses [`handle_bash_kill_scope_with_saved_tasks`] so tasks saved by an
/// earlier module instance are stopped as well.
#[cfg(all(test, unix))]
pub(crate) fn handle_bash_kill_scope(
    params: Value,
    principal: Option<&Principal>,
    registries: &[BgTaskRegistry],
) -> Response {
    handle_bash_kill_scope_with_saved_tasks(params, principal, registries, None)
}

/// Stop a cancelled worker's bash tasks without revoking its scope, including
/// its unfinished tasks saved under `saved_storage_root` that no live
/// registry holds.
///
/// After a module restart, a project's saved tasks are replayed only when
/// that project binds again, but their processes keep running meanwhile.
/// They are recovered from their saved records and stopped now, not left to
/// a later replay: a project may never bind again, and until it does nothing
/// would enforce even the task's own time limit. Each such task is marked
/// `from_saved_record` in the reply. If a saved task of this scope cannot be
/// recovered, the reply is an error naming it, never a completed stop.
pub(crate) fn handle_bash_kill_scope_with_saved_tasks(
    params: Value,
    principal: Option<&Principal>,
    registries: &[BgTaskRegistry],
    saved_storage_root: Option<&Path>,
) -> Response {
    const ID: &str = "management-bash-kill-scope";
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Params {
        scope: subc_protocol::scope::ScopeStamp,
    }
    let params = match serde_json::from_value::<Params>(params) {
        Ok(params) => params,
        Err(error) => return Response::error(ID, "invalid_request", error.to_string()),
    };
    if principal != Some(&params.scope.owner) {
        return Response::error(
            ID,
            "kill_scope_not_owner",
            "bash.kill_scope requires the scope's owner",
        );
    }
    let (recovered, recovery_errors) = match saved_storage_root {
        Some(root) => BgTaskRegistry::recover_saved_scope_tasks(root, &params.scope, registries),
        None => (Vec::new(), Vec::new()),
    };
    #[cfg(all(test, unix))]
    if let Some(root) = saved_storage_root {
        saved_recovery_gate::pass(root, saved_recovery_gate::Point::AfterRecovery);
    }
    let all_registries = registries
        .iter()
        .map(|registry| (registry, false))
        .chain(recovered.iter().map(|registry| (registry, true)))
        .collect::<Vec<_>>();
    let mut killed = Vec::new();
    let mut already_finished = 0;
    // Finish all local kills before requesting remote cancellation. All remote
    // workers receive intent before any wait, and share one reply deadline.
    let mut remote_tasks = Vec::new();
    let mut deadline = Instant::now();
    let mut kill_error = None;
    // One set of tasks for both passes, each remote member already carrying
    // the pause: a task cannot slip from remote to local between the passes
    // and be skipped by both.
    let mut members = Vec::with_capacity(all_registries.len());
    for (registry, _) in &all_registries {
        match registry.begin_scope_pause(&params.scope) {
            Ok(set) => members.push(set),
            Err(error) => {
                kill_error.get_or_insert(error);
                members.push(Default::default());
            }
        }
    }
    'passes: for remote in [false, true] {
        if kill_error.is_some() {
            break;
        }
        if remote {
            #[cfg(all(test, unix))]
            if let Some(root) = saved_storage_root {
                saved_recovery_gate::pass(root, saved_recovery_gate::Point::BetweenPasses);
            }
            deadline = Instant::now() + Duration::from_secs(10);
        }
        for ((registry, from_saved_record), members) in all_registries.iter().zip(&members) {
            match registry.pause_scope(&params.scope, remote, members) {
                Ok(report) => {
                    for mut task in report.killed {
                        task.from_saved_record = *from_saved_record;
                        if task.remote.is_some() {
                            remote_tasks.push(((*registry).clone(), killed.len()));
                        }
                        killed.push(task);
                    }
                    already_finished += report.already_finished;
                }
                Err(error) => {
                    kill_error = Some(error);
                    break 'passes;
                }
            }
        }
    }
    // Remote tasks these registries restored themselves have no worker yet.
    // (A task a project's replay restored first keeps that project's worker,
    // which reads the same cancellation state.) Start them only now that
    // their pause is recorded: each worker then cancels a job the runner
    // already holds, or withholds a resubmission or local fallback, and
    // records that outcome, which the loop below waits for.
    for registry in &recovered {
        registry.start_held_remote_workers();
    }
    if kill_error.is_none() {
        for (registry, index) in remote_tasks {
            let task = &mut killed[index];
            task.remote =
                Some(registry.wait_owner_pause_cancel(&task.task_id, &task.session_id, deadline));
        }
    }
    // Never report the pause done while a task it covers could still run.
    if kill_error.is_none() {
        let running = all_registries
            .iter()
            .zip(&members)
            .flat_map(|((registry, _), members)| registry.unstopped_pause_members(members))
            .collect::<Vec<_>>();
        if !running.is_empty() {
            kill_error = Some(format!(
                "tasks still running after the pause: {}",
                running.join(", ")
            ));
        }
    }
    #[cfg(all(test, unix))]
    if let Some(root) = saved_storage_root {
        saved_recovery_gate::pass(root, saved_recovery_gate::Point::BeforeDetach);
    }
    // The recovered tasks are stopped and their records saved; a later
    // replay of their project restores them as finished.
    for registry in &recovered {
        registry.detach();
    }
    let mut speech_databases = all_registries
        .iter()
        .filter_map(|(registry, _)| registry.speech_database_path())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(root) = saved_storage_root {
        let path = root.join("aft.db");
        if path.is_file() {
            speech_databases.insert(path);
        }
    }
    let queued = speech_databases.into_iter().try_fold(0_u64, |total, path| {
        crate::db::gh_speech::Store::new(path)
            .pending_legacy_for_scope(&params.scope)
            .ok()
            .map(|count| total.saturating_add(count))
    });
    let warning = match queued {
        Some(count) if count > 0 => Some(super::bash_kill::queued_writes_warning(count)),
        None => Some("bot_writes_already_queued: writes already queued for this scope may still post; the pending count could not be read.".to_string()),
        _ => None,
    };
    if let Some(error) = kill_error {
        return Response::error(
            ID,
            "kill_scope_failed",
            warning
                .as_ref()
                .map_or(error.clone(), |warning| format!("{error}\n{warning}")),
        );
    }
    if !recovery_errors.is_empty() {
        let error = recovery_errors.join("; ");
        return Response::error(
            ID,
            "kill_scope_failed",
            warning
                .as_ref()
                .map_or(error.clone(), |warning| format!("{error}\n{warning}")),
        );
    }
    killed.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    let mut data = json!({"killed":killed, "already_finished":already_finished, "not_found":0});
    if let Some(warning) = warning {
        data["bot_write_revocation_warning"] = json!(warning);
    }
    if queued.is_some_and(|count| count > 0) {
        data["bot_writes_already_queued"] = json!(queued.unwrap());
    }
    Response::success(ID, data)
}

#[cfg(all(test, unix))]
#[path = "bash_kill_scope_tests.rs"]
mod tests;

/// Lets a test hold `bash.kill_scope` at a `Point` of its handling of a
/// storage root's saved tasks, so another restore of the same tasks (a
/// project binding) can land in exactly that window.
#[cfg(all(test, unix))]
pub(crate) mod saved_recovery_gate {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Mutex, OnceLock};

    type Gate = (tokio::sync::oneshot::Sender<()>, mpsc::Receiver<()>);

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub(crate) enum Point {
        /// After the saved tasks were recovered, before they are paused.
        AfterRecovery,
        /// After the pass that stops local tasks, before the remote pass.
        BetweenPasses,
        /// After the pause settled, before the recovery registries detach.
        BeforeDetach,
    }

    fn gates() -> &'static Mutex<HashMap<(PathBuf, Point), Gate>> {
        static GATES: OnceLock<Mutex<HashMap<(PathBuf, Point), Gate>>> = OnceLock::new();
        GATES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn key(root: &Path, point: Point) -> (PathBuf, Point) {
        (
            std::fs::canonicalize(root).unwrap_or_else(|_| root.into()),
            point,
        )
    }

    /// Hold the next `bash.kill_scope` for saved tasks under `root` once at
    /// `point`. The receiver fires when it is held; sending on (or dropping)
    /// the sender lets it go on.
    pub(crate) fn arm(
        root: &Path,
        point: Point,
    ) -> (tokio::sync::oneshot::Receiver<()>, mpsc::Sender<()>) {
        let (reached_tx, reached) = tokio::sync::oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        assert!(gates()
            .lock()
            .unwrap()
            .insert(key(root, point), (reached_tx, release_rx))
            .is_none());
        (reached, release)
    }

    pub(super) fn pass(root: &Path, point: Point) {
        let armed = gates().lock().unwrap().remove(&key(root, point));
        if let Some((reached, release)) = armed {
            let _ = reached.send(());
            let _ = release.recv();
        }
    }
}
