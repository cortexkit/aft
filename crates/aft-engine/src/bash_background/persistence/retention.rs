//! Supply live shell and task-layout facts to database retention.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::db::bash_tasks::{self, RetentionHooks, TerminalPruneCandidate, TerminalRowsPrune};
use crate::db::compression_events::{self, RetentionGuard, RetentionSweepOutcome, RetentionTick};
use crate::db::TrackedConnection;

use super::{resolve_task_layout, session_tasks_dir, uninitialized_layout_is_recent};
use crate::bash_background::BgTaskRegistry;

const LAYOUT_CREATION_GRACE: Duration = Duration::from_secs(5 * 60);

pub(crate) const HOOKS: RetentionHooks = RetentionHooks {
    write_order_is_live: crate::bash_background::registry::task_db_write_order_is_live,
    process_alive: crate::bash_background::process::is_recorded_process_alive,
    layout_is_gone: task_layout_is_gone,
};

fn guard(registries: Option<&[BgTaskRegistry]>) -> RetentionGuard {
    RetentionGuard {
        hooks: HOOKS,
        has_active_watch: registries.map(|registries| {
            let registries = registries.to_vec();
            Arc::new(move |task_id: &str| {
                registries
                    .iter()
                    .any(|registry| registry.active_watch_count(task_id) > 0)
            }) as Arc<dyn Fn(&str) -> bool + Send + Sync>
        }),
    }
}

/// Remove acknowledged terminal rows only after the retention age has passed
/// and the task layout is absent. The filesystem check uses the same lookup and
/// initialization grace as persisted-task garbage collection, so a task being
/// created concurrently is not mistaken for a missing task.
pub fn prune_terminal_rows(
    conn: &rusqlite::Connection,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<TerminalRowsPrune> {
    bash_tasks::prune_terminal_rows(conn, now_ms, limit, HOOKS)
}

pub fn prune_retention_tick(
    conn: &mut rusqlite::Connection,
    now_ms: i64,
) -> rusqlite::Result<RetentionTick> {
    compression_events::prune_retention_tick(conn, now_ms, HOOKS)
}

/// Drain a retention backlog through independently committed, bounded transactions.
/// Contention between batches stops the sweep so interactive work always wins the mutex.
pub fn prune_retention_sweep(
    db: &Arc<Mutex<TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[BgTaskRegistry]>,
) -> Result<RetentionSweepOutcome, String> {
    compression_events::prune_retention_sweep(db, now_ms, &guard(registries))
}

/// Schedule bounded retention away from the daemon and standalone request loops.
/// A process can have only one sweep in flight and attempts at most once a
/// minute, or a few seconds after a sweep skipped for lack of the mutex.
pub fn maybe_spawn_retention(
    db: Option<Arc<Mutex<TrackedConnection>>>,
    registries: Option<Vec<BgTaskRegistry>>,
) {
    compression_events::maybe_spawn_retention(db, guard(registries.as_deref()));
}

#[cfg(test)]
pub(crate) fn prune_retention_sweep_observed(
    db: &Arc<Mutex<TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[BgTaskRegistry]>,
    observe: impl FnMut(compression_events::RetentionCountPhase, usize),
) -> Result<RetentionSweepOutcome, String> {
    compression_events::prune_retention_sweep_observed(db, now_ms, &guard(registries), observe)
}

#[cfg(test)]
pub(crate) fn prune_retention_once_observed(
    db: &Arc<Mutex<TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[BgTaskRegistry]>,
    observe: impl FnOnce(),
) -> Result<Option<compression_events::RetentionPass>, String> {
    compression_events::prune_retention_once_observed(db, now_ms, &guard(registries), observe)
}

fn task_layout_is_gone(storage_root: Option<&Path>, candidate: &TerminalPruneCandidate) -> bool {
    let session_dir = candidate_session_dir(candidate).or_else(|| {
        storage_root.map(|storage_root| session_tasks_dir(storage_root, &candidate.session_id))
    });
    let Some(session_dir) = session_dir else {
        return false;
    };
    let task_id = &candidate.task_id;
    let directory_layout = session_dir.join(task_id);
    let flat_layout = session_dir.join(format!("{task_id}.json"));
    match resolve_task_layout(&session_dir, task_id) {
        Ok(_) => false,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if !directory_layout.exists() && !flat_layout.exists() {
                return true;
            }
            !uninitialized_layout_is_recent(&session_dir, task_id, LAYOUT_CREATION_GRACE)
                .unwrap_or(true)
        }
        // Releases before the 64-bit random suffix used eight hexadecimal
        // digits. Current layout validation intentionally rejects those IDs,
        // so retain any surviving legacy artifact and prune only full absence.
        Err(error) if error.kind() == ErrorKind::InvalidInput && is_legacy_task_id(task_id) => {
            !directory_layout.exists()
                && !flat_layout.exists()
                && !candidate
                    .stdout_path
                    .iter()
                    .chain(&candidate.stderr_path)
                    .any(|path| Path::new(path).exists())
        }
        Err(_) => false,
    }
}

fn candidate_session_dir(candidate: &TerminalPruneCandidate) -> Option<PathBuf> {
    candidate
        .stdout_path
        .iter()
        .chain(&candidate.stderr_path)
        .find_map(|path| {
            let path = Path::new(path);
            let parent = path.parent()?;
            let file_name = path.file_name()?.to_str()?;
            if file_name.starts_with(&format!("{}.", candidate.task_id)) {
                return Some(parent.to_path_buf());
            }
            let task_dir = parent.parent()?;
            (task_dir.file_name()?.to_str()? == candidate.task_id)
                .then(|| task_dir.parent().map(Path::to_path_buf))
                .flatten()
        })
}

fn is_legacy_task_id(task_id: &str) -> bool {
    task_id.strip_prefix("bash-").is_some_and(|suffix| {
        suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}
