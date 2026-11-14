//! Keep a finished command's scratch directory for a while after it exits.
//!
//! Each sandboxed command gets its own `$TMPDIR` (`/private/tmp/aft-w/<id>` on
//! macOS, `/tmp/aft-w/<id>` on Linux). Agents often write a file there in one
//! shell call and read it with AFT's read tools in the next step, so deleting
//! the directory the moment the command exits made that read fail. The
//! directory now stays for [`SCRATCH_RETENTION`] after the command exits.
//!
//! The delete time is stored in the task record (`sandbox_temp_delete_at`), so
//! a restarted daemon that replays the record keeps the same deadline: it
//! deletes scratch whose time has passed and keeps the rest. While a directory
//! is kept, its task leaves the watchdog and is listed here instead. The
//! periodic cleanup lane (once a minute, on its own thread) hands each due
//! entry back to the watchdog, which schedules the deletion on the scratch
//! cleanup worker exactly as it did when deletion happened at exit. A record
//! that is pruned, or a session scope that ends, releases the scratch early.
//! As before, the stale sweep in `sandbox_spawn/task_temp.rs` reclaims, after
//! a day, scratch whose task record is gone or was never replayed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use super::{BgTask, BgTaskRegistry};
use crate::bash_background::persistence::{unix_millis, PersistedTask};

/// How long a finished command's scratch directory stays on disk after the
/// command exits, so the same session can still read files it wrote there.
pub const SCRATCH_RETENTION: Duration = Duration::from_secs(30 * 60);

/// Most due directories one sweep hands back for deletion. The rest wait for
/// the next sweep, a minute later.
const RELEASE_BUDGET: usize = 64;

/// Most directory entries one sweep visits while measuring kept directories.
/// A directory whose walk runs out of budget is reported as a lower bound.
const MEASURE_ENTRY_BUDGET: usize = 4096;

/// One kept scratch directory, waiting for its delete time.
pub(super) struct RetainedScratch {
    delete_at: u64,
    root: PathBuf,
    /// Filled in by a sweep; `None` until one has measured the directory.
    size: Option<ScratchSize>,
}

#[derive(Clone, Copy)]
struct ScratchSize {
    bytes: u64,
    /// False when the walk stopped at the entry budget, so `bytes` is a
    /// lower bound.
    complete: bool,
}

/// Kept scratch directories as the health snapshot reports them.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RetainedScratchHealth {
    /// Finished commands whose scratch directory is still kept on disk.
    pub count: usize,
    /// Total size of the regular files in the measured directories. `None`
    /// when no kept directory has been measured yet.
    pub bytes: Option<u64>,
    /// `measured` when every kept directory was measured in full,
    /// `lower_bound` when some are unmeasured or were cut off by the walk
    /// budget, and `unmeasured` when none has been measured yet. A directory
    /// is measured once, by the cleanup sweep, after its command has exited.
    pub bytes_state: &'static str,
    /// Kept directories no sweep has measured yet.
    pub unmeasured: usize,
}

/// When the scratch of a finished task may be deleted: its exit time plus
/// [`SCRATCH_RETENTION`]. A record without an exit time counts from `now_ms`.
pub(super) fn delete_at_for(metadata: &PersistedTask, now_ms: u64) -> u64 {
    let retention = u64::try_from(SCRATCH_RETENTION.as_millis()).unwrap_or(u64::MAX);
    metadata
        .finished_at
        .unwrap_or(now_ms)
        .saturating_add(retention)
}

impl BgTaskRegistry {
    /// List a kept scratch directory for the retention sweep. Repeated calls
    /// for the same task keep its first measurement.
    pub(super) fn retain_scratch(&self, task_id: &str, root: PathBuf, delete_at: u64) {
        let mut retained = self
            .inner
            .retained_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = retained
            .entry(task_id.to_string())
            .or_insert_with(|| RetainedScratch {
                delete_at,
                root: root.clone(),
                size: None,
            });
        entry.delete_at = delete_at;
        if entry.root != root {
            entry.root = root;
            entry.size = None;
        }
    }

    pub(super) fn forget_retained_scratch(&self, task_id: &str) {
        self.inner
            .retained_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(task_id);
    }

    /// Delete this task's scratch without waiting for its delete time. With
    /// `rearm_watchdog`, the watchdog takes the task back and finishes the
    /// deletion within a pass or two; otherwise the caller drives
    /// `cleanup_terminal_task_temp` itself.
    pub(super) fn release_task_scratch(&self, task: &Arc<BgTask>, rearm_watchdog: bool) {
        let kept = match task.lock_state() {
            Ok(mut state) => {
                state.scratch_released = true;
                state.metadata.sandbox_temp_dir.is_some()
            }
            Err(_) => false,
        };
        self.forget_retained_scratch(&task.task_id);
        if kept && rearm_watchdog {
            if let Ok(mut watchdog) = self.inner.watchdog_tasks.lock() {
                watchdog.insert(task.task_id.clone(), Arc::clone(task));
            }
            let _ = self.inner.wake_tx.try_send(());
        }
    }

    /// Release the kept scratch of every finished task that ran under
    /// `scope`, which has ended. Does no filesystem work here: the watchdog
    /// picks the tasks up and the scratch cleanup worker deletes the folders.
    /// A task that reaches scratch cleanup only after this pass is not listed
    /// yet; `cleanup_terminal_task_temp_at` checks the ended scopes itself
    /// and deletes its scratch instead of keeping it.
    pub(super) fn release_scope_scratch(&self, scope: &subc_protocol::scope::ScopeStamp) {
        let task_ids: Vec<String> = self
            .inner
            .retained_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for task_id in task_ids {
            let Some(task) = self.task(&task_id) else {
                self.forget_retained_scratch(&task_id);
                continue;
            };
            let same_scope = task
                .lock_state()
                .ok()
                .and_then(|state| state.metadata.scope.clone())
                .is_some_and(|stamp| {
                    stamp.owner == scope.owner
                        && stamp.scope_ref == scope.scope_ref
                        && stamp.scope_epoch == scope.scope_epoch
                });
            if same_scope {
                self.release_task_scratch(&task, true);
            }
        }
    }

    /// Hand due scratch back for deletion and measure kept directories.
    /// Runs on the periodic cleanup thread, never on a request path.
    pub(crate) fn sweep_retained_scratch(&self) {
        self.sweep_retained_scratch_at(unix_millis());
    }

    /// [`Self::sweep_retained_scratch`] with the current time passed in, in
    /// Unix milliseconds.
    pub(crate) fn sweep_retained_scratch_at(&self, now_ms: u64) {
        let (due, unmeasured) = {
            let retained = self
                .inner
                .retained_scratch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut examined = 0;
            let due: Vec<String> = retained
                .iter()
                .inspect(|_| examined += 1)
                .filter(|(_, entry)| entry.delete_at <= now_ms)
                .map(|(task_id, _)| task_id.clone())
                .take(RELEASE_BUDGET)
                .collect();
            let unmeasured: Vec<(String, PathBuf)> = retained
                .iter()
                .inspect(|_| examined += 1)
                .filter(|(_, entry)| entry.size.is_none() && entry.delete_at > now_ms)
                .map(|(task_id, entry)| (task_id.clone(), entry.root.clone()))
                .take(RELEASE_BUDGET)
                .collect();
            super::super::watchdog::record_cleanup_work(|work| work.scratch_entries += examined);
            (due, unmeasured)
        };
        for task_id in due {
            match self.task(&task_id) {
                Some(task) => self.release_task_scratch(&task, true),
                // The task left the registry (its project detached). Its record
                // still holds the delete time, so a later replay deletes it.
                None => self.forget_retained_scratch(&task_id),
            }
        }
        // Walk without holding the list lock, so health reads never wait on
        // the filesystem.
        let mut budget = MEASURE_ENTRY_BUDGET;
        for (task_id, root) in unmeasured {
            if budget == 0 {
                break;
            }
            let size = measure(&root, &mut budget);
            if let Some(entry) = self
                .inner
                .retained_scratch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&task_id)
            {
                if entry.root == root {
                    entry.size = Some(size);
                }
            }
        }
    }

    /// Kept scratch for the health snapshot, from memory only. `None` when
    /// the list is locked at this moment, like the other health counts.
    pub(crate) fn try_retained_scratch_health(&self) -> Option<RetainedScratchHealth> {
        let retained = self.inner.retained_scratch.try_lock().ok()?;
        let count = retained.len();
        let sizes: Vec<ScratchSize> = retained.values().filter_map(|entry| entry.size).collect();
        let unmeasured = count - sizes.len();
        let bytes = sizes
            .iter()
            .fold(0u64, |total, size| total.saturating_add(size.bytes));
        let (bytes, bytes_state) = if count == 0 {
            (Some(0), "measured")
        } else if sizes.is_empty() {
            (None, "unmeasured")
        } else if unmeasured == 0 && sizes.iter().all(|size| size.complete) {
            (Some(bytes), "measured")
        } else {
            (Some(bytes), "lower_bound")
        };
        Some(RetainedScratchHealth {
            count,
            bytes,
            bytes_state,
            unmeasured,
        })
    }
}

/// Sum regular-file sizes under `root` without following symbolic links,
/// visiting at most `budget` entries (shared across one sweep).
fn measure(root: &Path, budget: &mut usize) -> ScratchSize {
    let mut size = ScratchSize {
        bytes: 0,
        complete: true,
    };
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            if *budget == 0 {
                size.complete = false;
                return size;
            }
            *budget -= 1;
            let Ok(entry) = entry else { continue };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    size.bytes = size.bytes.saturating_add(metadata.len());
                }
            }
        }
    }
    size
}
