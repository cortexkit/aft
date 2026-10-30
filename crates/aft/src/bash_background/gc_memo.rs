//! What the persisted-task sweep (`BgTaskRegistry::maybe_gc_persisted`)
//! learned on earlier passes over a storage root, so later passes need not
//! reopen tasks that cannot have become deletable since.
//!
//! The sweep walks the store a bounded number of directory entries per pass
//! and wraps around when it reaches the end. Most of a large store is history
//! the sweep must keep: finished tasks whose result was never delivered, and
//! tasks written within the modification grace. Without a memory of those
//! verdicts, every lap reopened each task's folders and parsed its metadata
//! twice, only to reach the same answer again.
//!
//! A remembered verdict is trusted only while everything it was derived from
//! is unchanged: the session folder (a task layout appearing or vanishing
//! beside it), the task folder (its `control/` or `io/` folder replaced), and
//! the metadata file itself. Metadata is always written by atomic replacement,
//! so any rewrite gives the file a new identity and a new modification time.
//! A rate-limited validation cursor checks those stamps, including changes
//! made by foreign writers. An ordered deadline index also selects tasks at
//! the end of the modification grace or the abandonment age without walking
//! unrelated history. Conditions that can change without any file in the
//! store changing are evaluated again on validation visits: the age and
//! missing-worktree tests that retire an abandoned completion, whether a
//! running task's recorded process is still alive, and whether a registry in
//! this process still owns a finished task whose scratch folder it is
//! removing. None of those checks opens a file. A running task whose process
//! the sweep found dead, and anything the sweep failed to read, are never
//! remembered at all.
//!
//! The memory lives in the daemon process, keyed by canonical storage root,
//! next to the walk position kept by `gc_cursor`. A restart starts both from
//! scratch, so the first lap after a restart evaluates every task once.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::persistence::TaskLayout;

/// The parts of a file's metadata that every replacement or rewrite changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    modified: SystemTime,
    len: u64,
    /// A replacement is a different file, so this changes even when a
    /// same-length rewrite lands within one tick of the filesystem clock and
    /// keeps every time.
    identity: crate::file_identity::FileIdentity,
    /// Windows also keeps the creation time, which a replacement normally
    /// changes, for a filesystem that reports no file identity.
    #[cfg(windows)]
    created: u64,
}

impl Stamp {
    /// Reads the stamp without following a symlink. A filesystem that cannot
    /// report a modification time yields an error, so nothing is remembered
    /// from evidence that cannot be checked later.
    pub(crate) fn of(path: &Path) -> std::io::Result<Self> {
        #[cfg(test)]
        super::persistence::work_counts::record_stat();
        let (metadata, identity) = crate::file_identity::symlink_metadata(path)?;
        Ok(Self {
            modified: metadata.modified()?,
            len: metadata.len(),
            identity,
            #[cfg(windows)]
            created: std::os::windows::fs::MetadataExt::creation_time(&metadata),
        })
    }
}

/// True while a file is within its modification grace. A future timestamp has
/// an unknown age, so retain it rather than treating it as expired.
pub(crate) fn within_grace_at(modified: SystemTime, grace: Duration, now: SystemTime) -> bool {
    now.duration_since(modified)
        .ok()
        .map(|age| age < grace)
        .unwrap_or(true)
}

/// Whether an undelivered, finished task's result is abandoned: it finished
/// more than seven days ago and both its project root and its working
/// directory are gone. The caller has already established that the task is
/// terminal and its completion undelivered.
pub(crate) fn abandonment_due(
    finished_at: Option<u64>,
    project_root: Option<&Path>,
    workdir: &Path,
    now_ms: u64,
) -> bool {
    const ABANDONMENT_MS: u64 = 7 * 24 * 60 * 60 * 1000;
    finished_at.is_some_and(|finished| now_ms.saturating_sub(finished) > ABANDONMENT_MS)
        && project_root.is_some_and(|root| matches!(root.try_exists(), Ok(false)))
        && matches!(workdir.try_exists(), Ok(false))
}

/// Why the sweep left a task in place, and what could make it deletable
/// without its files changing.
#[derive(Clone, Debug)]
pub(crate) enum Wait {
    /// Its metadata was written within the modification grace.
    Grace,
    /// It finished but its result was never delivered. Unless its metadata
    /// changes (a delivery rewrites it), only abandonment can retire it.
    Abandonment {
        finished_at: Option<u64>,
        project_root: Option<Arc<Path>>,
        workdir: Arc<Path>,
    },
    /// It is still running: a recorded process (the child or its group
    /// leader) was alive. Only that process exiting, or a metadata write,
    /// can change the verdict.
    RecordedProcess { pids: Vec<u32>, started_at: u64 },
    /// It is finished, but a registry in this process still owns it while
    /// its scratch folder is being removed. Only that ownership ending, or a
    /// metadata write, can change the verdict.
    ScratchOwner { session_id: Arc<str> },
}

impl Wait {
    fn deadline(&self, metadata: &Stamp, now: SystemTime) -> Option<SystemTime> {
        match self {
            Self::Grace => metadata
                .modified
                .checked_add(super::registry::PERSISTED_GC_GRACE),
            Self::Abandonment {
                finished_at: Some(finished),
                ..
            } => finished
                .checked_add(7 * 24 * 60 * 60 * 1000 + 1)
                .and_then(|ms| UNIX_EPOCH.checked_add(Duration::from_millis(ms))),
            _ => None,
        }
        .filter(|deadline| *deadline > now)
    }
}

#[derive(Clone, Debug)]
struct Verdict {
    layout: TaskLayout,
    session: Stamp,
    /// The task folder's stamp; `None` for the legacy flat layout, whose
    /// files sit directly in the session folder.
    task_dir: Option<Stamp>,
    metadata: Stamp,
    wait: Wait,
}

impl Verdict {
    fn metadata_path(&self, session_dir: &Path, task_id: &str) -> PathBuf {
        match self.layout {
            TaskLayout::Directory => session_dir
                .join(task_id)
                .join(super::persistence::CONTROL_DIR)
                .join(super::persistence::METADATA_FILE),
            TaskLayout::Flat => session_dir.join(format!("{task_id}.json")),
        }
    }

    /// Whether the task is still certainly not deletable. Any stamp that
    /// differs or cannot be read means "judge it again", never "skip".
    fn still_waiting(
        &self,
        session_dir: &Path,
        session: &Stamp,
        task_id: &str,
        grace: Duration,
        now: SystemTime,
    ) -> bool {
        if self.session != *session {
            return false;
        }
        if let Some(task_dir) = &self.task_dir {
            if Stamp::of(&session_dir.join(task_id)).ok().as_ref() != Some(task_dir) {
                return false;
            }
        }
        if Stamp::of(&self.metadata_path(session_dir, task_id))
            .ok()
            .as_ref()
            != Some(&self.metadata)
        {
            return false;
        }
        match &self.wait {
            Wait::Grace => within_grace_at(self.metadata.modified, grace, now),
            Wait::Abandonment {
                finished_at,
                project_root,
                workdir,
            } => !abandonment_due(
                *finished_at,
                project_root.as_deref(),
                workdir,
                now.duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            ),
            Wait::RecordedProcess { pids, started_at } => pids
                .iter()
                .any(|pid| super::process::is_recorded_process_alive(*pid, *started_at)),
            Wait::ScratchOwner { session_id } => {
                super::registry::task_db_write_order_is_live(session_id, task_id)
            }
        }
    }
}

struct Remembered {
    verdict: Verdict,
    deadline: Option<SystemTime>,
    /// The deadline is a retry installed by selection, not a writer wakeup.
    provisional_retry: bool,
    /// The lap in which the sweep last reached this task. Entries not reached
    /// during a whole lap belong to deleted tasks and are dropped at its end.
    seen_lap: u64,
}

/// One storage root's sweep progress, reported on the health surface.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PersistedGcHealth {
    /// Path of the canonical `bash-tasks` folder (inside a harness storage
    /// root) whose sweep these figures describe.
    pub root: String,
    pub passes: u64,
    /// Full laps over the store completed since the daemon started.
    pub laps_completed: u64,
    /// Set when the last pass used up its entry budget before reaching the end
    /// of the store; the next pass resumes where it stopped. On a store larger
    /// than one pass's budget this is set after every pass of a lap except
    /// the last.
    pub last_pass_out_of_budget: bool,
    pub passes_out_of_budget: u64,
    /// Task candidates reached through discovery or the deadline index, how
    /// many were opened and judged, and how many remembered verdicts held.
    pub last_pass_tasks: usize,
    pub last_pass_evaluated: usize,
    pub last_pass_unchanged: usize,
    /// Session folders that could not be opened during the last pass.
    pub last_pass_unreadable_sessions: usize,
    /// Directory-entry reads that failed during the last pass.
    pub last_pass_directory_read_errors: usize,
    /// Tasks currently remembered as not deletable.
    pub remembered_tasks: usize,
    /// Why the last pass could not walk the store, if it could not. Such a
    /// pass deleted nothing because it could not look, not because nothing
    /// was due.
    pub last_error: Option<String>,
}

#[derive(Default)]
struct RootState {
    /// Generation of the current store walk, including walks with read errors.
    lap: u64,
    sessions: HashMap<PathBuf, HashMap<String, Remembered>>,
    deadlines: BTreeSet<(SystemTime, PathBuf, String)>,
    validation_after: Option<Instant>,
    validation_lap_started: Option<Instant>,
    write_generation: u64,
    /// Sessions whose task listings were read during the current store walk.
    read_sessions: HashSet<PathBuf>,
    /// Sessions whose task listings were incomplete during the current store walk.
    unreadable_sessions: HashSet<PathBuf>,
    /// Whether any root-directory entry could not be read during this store walk.
    root_directory_unreadable: bool,
    /// Project roots and working directories are shared by many tasks; keep
    /// one copy of each.
    paths: HashSet<Arc<Path>>,
    health: PersistedGcHealth,
    /// Process-wide order of this root's last pass, to name the most recent
    /// failure across roots.
    last_pass_seq: u64,
}

impl RootState {
    fn intern(&mut self, path: &Path) -> Arc<Path> {
        if let Some(existing) = self.paths.get(path) {
            return Arc::clone(existing);
        }
        let interned: Arc<Path> = Arc::from(path);
        self.paths.insert(Arc::clone(&interned));
        interned
    }

    fn remembered_tasks(&self) -> usize {
        self.sessions.values().map(HashMap::len).sum()
    }
}

fn roots() -> &'static Mutex<HashMap<PathBuf, RootState>> {
    static ROOTS: std::sync::OnceLock<Mutex<HashMap<PathBuf, RootState>>> =
        std::sync::OnceLock::new();
    ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A daemon metadata replacement may change eligibility before an old
/// deadline. Wake only an already indexed task; discovery handles new tasks
/// and foreign writers. Calls bracket replacement, even on failure: a rename
/// can succeed before a directory-sync error, and a concurrent sweep may read
/// the old file while replacement is in flight.
pub(crate) fn metadata_written(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let (session, id) = if path
        .file_name()
        .is_some_and(|name| name == super::persistence::METADATA_FILE)
    {
        let Some(task_dir) = parent.parent() else {
            return;
        };
        let Some(session) = task_dir.parent() else {
            return;
        };
        let Some(id) = task_dir.file_name().and_then(|id| id.to_str()) else {
            return;
        };
        (session, id)
    } else {
        let Some(id) = path.file_stem().and_then(|id| id.to_str()) else {
            return;
        };
        (parent, id)
    };
    let Some(root) = session.parent() else { return };
    let mut roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The store key is canonical, but remembered sessions retain the caller's
    // spelling (for example /var versus /private/var on macOS). Match that
    // session on an alias miss without adding filesystem I/O to every write.
    let key = if roots.contains_key(root) {
        root.to_path_buf()
    } else {
        let Some(key) = roots
            .iter()
            .find_map(|(key, state)| state.sessions.contains_key(session).then(|| key.clone()))
        else {
            return;
        };
        key
    };
    let state = roots.get_mut(&key).expect("known GC store");
    state.write_generation = state.write_generation.wrapping_add(1);
    let Some(entry) = state
        .sessions
        .get_mut(session)
        .and_then(|tasks| tasks.get_mut(id))
    else {
        return;
    };
    if let Some(deadline) = entry.deadline {
        state
            .deadlines
            .remove(&(deadline, session.to_path_buf(), id.to_string()));
    }
    entry.deadline = Some(UNIX_EPOCH);
    entry.provisional_retry = false;
    state
        .deadlines
        .insert((UNIX_EPOCH, session.to_path_buf(), id.to_string()));
}

fn with_root<R>(root: &Path, work: impl FnOnce(&mut RootState) -> R) -> R {
    let mut roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = roots
        .entry(root.to_path_buf())
        .or_insert_with(|| RootState {
            health: PersistedGcHealth {
                root: root.display().to_string(),
                ..PersistedGcHealth::default()
            },
            ..RootState::default()
        });
    work(state)
}

/// The sweep's memory for one canonical `bash-tasks` root.
pub(crate) struct GcMemo {
    root: PathBuf,
}

/// The file stamps a remembered keep-verdict depends on, captured before the
/// sweep read the task metadata that the verdict is based on.
pub(crate) struct Evidence {
    pub layout: TaskLayout,
    pub session: Stamp,
    pub task_dir: Option<Stamp>,
    pub metadata: Stamp,
    pub write_generation: u64,
}

/// Input for [`Wait::Abandonment`], borrowed from the task's metadata.
pub(crate) struct AbandonmentInput<'a> {
    pub finished_at: Option<u64>,
    pub project_root: Option<&'a Path>,
    pub workdir: &'a Path,
}

impl GcMemo {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn write_generation(&self, session_dir: &Path) -> u64 {
        with_root(&self.root, |state| {
            state.sessions.entry(session_dir.to_path_buf()).or_default();
            state.write_generation
        })
    }

    /// Scheduled requests may arrive from many roots or replays. Validation
    /// advances at most once a minute, and a small store starts no more than
    /// one full lap per ten minutes. Deadline work is independent of this gate.
    pub(crate) fn validation_due(&self, now: Instant) -> bool {
        with_root(&self.root, |state| {
            if state.validation_after.is_some_and(|after| now < after) {
                return false;
            }
            state.validation_lap_started.get_or_insert(now);
            state.validation_after = Some(now + Duration::from_secs(60));
            true
        })
    }

    /// Select directly from the ordered deadline prefix, not by filtering the
    /// task map. The iterator budget includes a possible first future entry.
    /// Keep a one-minute retry until evaluation replaces or forgets the
    /// verdict, so skipping a session being restored does not consume its
    /// deadline without evaluating it.
    pub(crate) fn due_batches(&self, now: SystemTime) -> Vec<super::gc_cursor::Batch> {
        with_root(&self.root, |state| {
            let due: Vec<_> = state
                .deadlines
                .iter()
                .inspect(|_| {
                    #[cfg(test)]
                    super::persistence::work_counts::record_gc_deadline();
                })
                .take(super::gc_cursor::GC_ENTRY_BUDGET)
                .take_while(|(deadline, _, _)| *deadline <= now)
                .cloned()
                .collect();
            let mut batches: Vec<super::gc_cursor::Batch> = Vec::new();
            for key in due {
                state.deadlines.remove(&key);
                let (_, session, id) = key;
                if let Some(entry) = state
                    .sessions
                    .get_mut(&session)
                    .and_then(|tasks| tasks.get_mut(&id))
                {
                    let retry = now.checked_add(Duration::from_secs(60));
                    entry.deadline = retry;
                    entry.provisional_retry = true;
                    if let Some(retry) = retry {
                        state.deadlines.insert((retry, session.clone(), id.clone()));
                    }
                }
                if let Some(batch) = batches.iter_mut().find(|batch| batch.session == session) {
                    batch.ids.push(id);
                } else {
                    batches.push(super::gc_cursor::Batch {
                        session,
                        ids: vec![id],
                        invalid: Vec::new(),
                        names: Vec::new(),
                    });
                }
            }
            batches
        })
    }

    /// True when the task was judged not deletable earlier and nothing it was
    /// judged on has changed. Opens nothing: at most two `stat` calls, plus
    /// two for the worktree checks of an undelivered result old enough to be
    /// abandoned, or a probe of a running task's recorded process. A verdict
    /// that no longer holds is forgotten.
    pub(crate) fn still_waiting(
        &self,
        session_dir: &Path,
        session: Option<&Stamp>,
        task_id: &str,
        grace: Duration,
        now: SystemTime,
    ) -> bool {
        let Some(session) = session else {
            return false;
        };
        let verdict = with_root(&self.root, |state| {
            let lap = state.lap;
            let entry = state.sessions.get_mut(session_dir)?.get_mut(task_id)?;
            entry.seen_lap = lap;
            Some(entry.verdict.clone())
        });
        let Some(verdict) = verdict else {
            return false;
        };
        // The stats run without the lock held: other roots' sweeps share it.
        let waiting = verdict.still_waiting(session_dir, session, task_id, grace, now);
        if !waiting {
            self.forget(session_dir, task_id);
        } else {
            // A still-valid verdict goes back to its natural deadline, or to
            // validation for conditions without a future deadline. Do not turn
            // an unchanged task into minute polling or erase a newer writer's
            // immediate invalidation while replacing the provisional retry.
            let next = verdict.wait.deadline(&verdict.metadata, now);
            with_root(&self.root, |state| {
                if let Some(entry) = state
                    .sessions
                    .get_mut(session_dir)
                    .and_then(|tasks| tasks.get_mut(task_id))
                {
                    if entry.provisional_retry {
                        entry.provisional_retry = false;
                        if let Some(deadline) = entry.deadline.take() {
                            state.deadlines.remove(&(
                                deadline,
                                session_dir.to_path_buf(),
                                task_id.to_string(),
                            ));
                        }
                        entry.deadline = next;
                        if let Some(next) = next {
                            state.deadlines.insert((
                                next,
                                session_dir.to_path_buf(),
                                task_id.to_string(),
                            ));
                        }
                    }
                }
            });
        }
        waiting
    }

    pub(crate) fn remember_grace(&self, session_dir: &Path, task_id: &str, evidence: Evidence) {
        self.remember(session_dir, task_id, evidence, |_| Wait::Grace);
    }

    pub(crate) fn remember_undelivered(
        &self,
        session_dir: &Path,
        task_id: &str,
        evidence: Evidence,
        input: AbandonmentInput<'_>,
    ) {
        self.remember(session_dir, task_id, evidence, |state| Wait::Abandonment {
            finished_at: input.finished_at,
            project_root: input.project_root.map(|root| state.intern(root)),
            workdir: state.intern(input.workdir),
        });
    }

    /// Remembers a running task whose recorded process was alive.
    pub(crate) fn remember_recorded_process(
        &self,
        session_dir: &Path,
        task_id: &str,
        evidence: Evidence,
        pids: Vec<u32>,
        started_at: u64,
    ) {
        self.remember(session_dir, task_id, evidence, |_| Wait::RecordedProcess {
            pids,
            started_at,
        });
    }

    /// Remembers a finished task kept while a registry owns its scratch removal.
    pub(crate) fn remember_scratch_owner(
        &self,
        session_dir: &Path,
        task_id: &str,
        evidence: Evidence,
        session_id: &str,
    ) {
        self.remember(session_dir, task_id, evidence, |_| Wait::ScratchOwner {
            session_id: Arc::from(session_id),
        });
    }

    fn remember(
        &self,
        session_dir: &Path,
        task_id: &str,
        evidence: Evidence,
        wait: impl FnOnce(&mut RootState) -> Wait,
    ) {
        with_root(&self.root, |state| {
            let wait = wait(state);
            let lap = state.lap;
            let deadline = wait.deadline(&evidence.metadata, SystemTime::now());
            // A writer can replace metadata between evidence capture and
            // remembering it. Recheck that verdict promptly instead of
            // replacing the writer's invalidation with an old deadline.
            let deadline = if evidence.write_generation != state.write_generation {
                Some(UNIX_EPOCH)
            } else {
                deadline
            };
            if let Some(old) = state
                .sessions
                .get(session_dir)
                .and_then(|tasks| tasks.get(task_id))
                .and_then(|entry| entry.deadline)
            {
                state
                    .deadlines
                    .remove(&(old, session_dir.to_path_buf(), task_id.to_string()));
            }
            if let Some(deadline) = deadline {
                state
                    .deadlines
                    .insert((deadline, session_dir.to_path_buf(), task_id.to_string()));
            }
            state
                .sessions
                .entry(session_dir.to_path_buf())
                .or_default()
                .insert(
                    task_id.to_string(),
                    Remembered {
                        deadline,
                        provisional_retry: false,
                        verdict: Verdict {
                            layout: evidence.layout,
                            session: evidence.session,
                            task_dir: evidence.task_dir,
                            metadata: evidence.metadata,
                            wait,
                        },
                        seen_lap: lap,
                    },
                );
        });
    }

    /// Marks a session whose task listing was successfully read in this walk.
    pub(crate) fn session_read(&self, session_dir: &Path) {
        with_root(&self.root, |state| {
            if state.sessions.contains_key(session_dir) {
                state.read_sessions.insert(session_dir.to_path_buf());
            }
        });
    }

    /// Keeps memo entries for sessions whose task listing was incomplete.
    pub(crate) fn session_unreadable(&self, session_dir: &Path) {
        with_root(&self.root, |state| {
            if state.sessions.contains_key(session_dir) {
                state.unreadable_sessions.insert(session_dir.to_path_buf());
            }
        });
    }

    /// Keeps unseen session entries when the root listing itself was incomplete.
    pub(crate) fn root_directory_unreadable(&self) {
        with_root(&self.root, |state| state.root_directory_unreadable = true);
    }

    pub(crate) fn forget(&self, session_dir: &Path, task_id: &str) {
        with_root(&self.root, |state| {
            if let Some(tasks) = state.sessions.get_mut(session_dir) {
                if let Some(entry) = tasks.remove(task_id) {
                    if let Some(deadline) = entry.deadline {
                        state.deadlines.remove(&(
                            deadline,
                            session_dir.to_path_buf(),
                            task_id.to_string(),
                        ));
                    }
                }
                if tasks.is_empty() {
                    state.sessions.remove(session_dir);
                }
            }
        });
    }

    /// Memoized task entries absent from successfully read sessions are dropped;
    /// entries for sessions that could not be inspected are preserved. A root
    /// that no longer exists is forgotten entirely.
    pub(crate) fn finish_pass(&self, pass: PassReport) {
        static PASS_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if pass.root_missing {
            roots()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.root);
            return;
        }
        let seq = PASS_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        with_root(&self.root, |state| {
            state.last_pass_seq = seq;
            if pass.walk_finished {
                if let Some(started) = state.validation_lap_started.take() {
                    let after = started + Duration::from_secs(10 * 60);
                    state.validation_after =
                        Some(state.validation_after.map_or(after, |next| next.max(after)));
                }
                let lap = state.lap;
                let read_sessions = std::mem::take(&mut state.read_sessions);
                let unreadable_sessions = std::mem::take(&mut state.unreadable_sessions);
                let root_directory_unreadable =
                    std::mem::replace(&mut state.root_directory_unreadable, false);
                let deadlines = &mut state.deadlines;
                state.sessions.retain(|session_dir, tasks| {
                    if unreadable_sessions.contains(session_dir) {
                        // A partial listing cannot prove which remembered tasks
                        // disappeared, so preserve the session's verdicts.
                    } else if read_sessions.contains(session_dir) {
                        tasks.retain(|id, entry| {
                            if entry.seen_lap >= lap {
                                return true;
                            }
                            if let Some(deadline) = entry.deadline {
                                deadlines.remove(&(deadline, session_dir.clone(), id.clone()));
                            }
                            false
                        });
                    } else if !root_directory_unreadable {
                        for (id, entry) in tasks.iter() {
                            if let Some(deadline) = entry.deadline {
                                deadlines.remove(&(deadline, session_dir.clone(), id.clone()));
                            }
                        }
                        return false;
                    }
                    !tasks.is_empty()
                });
                state.paths.retain(|path| Arc::strong_count(path) > 1);
                state.lap = lap.saturating_add(1);
                if pass.error.is_none() {
                    state.health.laps_completed = state.health.laps_completed.saturating_add(1);
                }
            }
            let remembered = state.remembered_tasks();
            let health = &mut state.health;
            health.passes = health.passes.saturating_add(1);
            health.last_pass_out_of_budget =
                !pass.validation_skipped && !pass.walk_finished && pass.error.is_none();
            if health.last_pass_out_of_budget {
                health.passes_out_of_budget = health.passes_out_of_budget.saturating_add(1);
            }
            health.last_pass_tasks = pass.tasks;
            health.last_pass_evaluated = pass.evaluated;
            health.last_pass_unchanged = pass.unchanged;
            health.last_pass_unreadable_sessions = pass.unreadable_sessions;
            health.last_pass_directory_read_errors = pass.directory_read_errors;
            health.remembered_tasks = remembered;
            health.last_error = pass.error;
        });
    }
}

/// What one sweep pass did, for [`GcMemo::finish_pass`].
#[derive(Debug, Default)]
pub(crate) struct PassReport {
    pub walk_finished: bool,
    pub validation_skipped: bool,
    /// The `bash-tasks` folder does not exist, so there is nothing to sweep
    /// or remember.
    pub root_missing: bool,
    pub error: Option<String>,
    pub tasks: usize,
    pub evaluated: usize,
    pub unchanged: usize,
    pub unreadable_sessions: usize,
    pub directory_read_errors: usize,
}

/// A storage root whose last sweep pass could not walk it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PersistedGcFailure {
    pub root: String,
    pub error: String,
}

/// Sweep progress across every storage root this process sweeps, as fixed
/// size totals so the health payload does not grow with the number of roots.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PersistedGcSummary {
    pub roots: usize,
    /// Roots whose last pass used up its entry budget before the end of the
    /// store. Each resumes where it stopped on its next pass (once a minute),
    /// so this is a backlog being worked through, not a stall.
    pub roots_out_of_budget: usize,
    pub passes_out_of_budget_total: u64,
    pub laps_completed_total: u64,
    /// Tasks known to be kept, which later laps skip without opening.
    pub remembered_tasks_total: usize,
    /// Roots whose last pass could not walk the store at all, and the most
    /// recent such failure. Those passes deleted nothing because they could
    /// not look, not because nothing was due.
    pub roots_failing: usize,
    pub last_failure: Option<PersistedGcFailure>,
}

/// Sweep progress totals for the health surface.
pub fn persisted_gc_health() -> PersistedGcSummary {
    let roots = roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut summary = PersistedGcSummary {
        roots: roots.len(),
        ..PersistedGcSummary::default()
    };
    let mut last_failure_seq = None;
    for state in roots.values() {
        let health = &state.health;
        summary.roots_out_of_budget += usize::from(health.last_pass_out_of_budget);
        summary.passes_out_of_budget_total = summary
            .passes_out_of_budget_total
            .saturating_add(health.passes_out_of_budget);
        summary.laps_completed_total = summary
            .laps_completed_total
            .saturating_add(health.laps_completed);
        summary.remembered_tasks_total += health.remembered_tasks;
        if let Some(error) = &health.last_error {
            summary.roots_failing += 1;
            if last_failure_seq.is_none_or(|seq| state.last_pass_seq > seq) {
                last_failure_seq = Some(state.last_pass_seq);
                summary.last_failure = Some(PersistedGcFailure {
                    root: health.root.clone(),
                    error: error.clone(),
                });
            }
        }
    }
    summary
}

/// Sweep progress for one canonical `bash-tasks` root.
#[cfg(test)]
pub(crate) fn persisted_gc_health_for(root: &Path) -> Option<PersistedGcHealth> {
    roots()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .map(|state| state.health.clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn gc_grace_keeps_future_mtime() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(within_grace_at(
            now + Duration::from_secs(60),
            super::super::registry::PERSISTED_GC_GRACE,
            now
        ));
    }

    #[test]
    fn expired_abandonment_age_with_live_worktree_returns_to_validation() {
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("bash-tasks");
        let session = root.join("session");
        let task = session.join("bash-0000000000000001");
        let metadata = task
            .join("control")
            .join(super::super::persistence::METADATA_FILE);
        std::fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        std::fs::write(&metadata, b"{}").unwrap();
        let memo = GcMemo::new(root);
        let session_stamp = Stamp::of(&session).unwrap();
        let finished = crate::bash_background::persistence::unix_millis();
        memo.remember_undelivered(
            &session,
            "bash-0000000000000001",
            Evidence {
                layout: TaskLayout::Directory,
                session: session_stamp,
                task_dir: Some(Stamp::of(&task).unwrap()),
                metadata: Stamp::of(&metadata).unwrap(),
                write_generation: memo.write_generation(&session),
            },
            AbandonmentInput {
                finished_at: Some(finished),
                project_root: Some(storage.path()),
                workdir: storage.path(),
            },
        );
        let due = UNIX_EPOCH + Duration::from_millis(finished + 7 * 24 * 60 * 60 * 1000 + 1);
        assert_eq!(memo.due_batches(due).len(), 1);
        assert!(memo.still_waiting(
            &session,
            Some(&session_stamp),
            "bash-0000000000000001",
            super::super::registry::PERSISTED_GC_GRACE,
            due
        ));
        assert!(memo.due_batches(due + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn metadata_write_during_evaluation_keeps_its_invalidation() {
        let storage = tempfile::tempdir().unwrap();
        let root = storage.path().join("bash-tasks");
        let session = root.join("..").join("bash-tasks").join("session");
        let task_dir = session.join("bash-0000000000000001");
        let metadata = task_dir
            .join("control")
            .join(super::super::persistence::METADATA_FILE);
        std::fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        std::fs::write(&metadata, b"{}").unwrap();
        let memo = GcMemo::new(root.canonicalize().unwrap());
        let evidence = Evidence {
            layout: TaskLayout::Directory,
            session: Stamp::of(&session).unwrap(),
            task_dir: Some(Stamp::of(&task_dir).unwrap()),
            metadata: Stamp::of(&metadata).unwrap(),
            write_generation: memo.write_generation(&session),
        };
        metadata_written(&metadata);
        memo.remember_grace(&session, "bash-0000000000000001", evidence);
        let now = SystemTime::now();
        let batches = memo.due_batches(now);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].ids, ["bash-0000000000000001"]);
        assert!(memo.still_waiting(
            &session,
            Some(&Stamp::of(&session).unwrap()),
            "bash-0000000000000001",
            super::super::registry::PERSISTED_GC_GRACE,
            now + Duration::from_secs(1)
        ));
        assert!(
            memo.due_batches(now + Duration::from_secs(60)).is_empty(),
            "an unchanged grace verdict became minute polling"
        );
    }

    #[test]
    fn grace_keeps_its_exact_boundary() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let grace = super::super::registry::PERSISTED_GC_GRACE;
        assert!(within_grace_at(
            now - grace + Duration::from_micros(1),
            grace,
            now
        ));
        assert!(!within_grace_at(now - grace, grace, now));
    }

    use super::*;

    /// Task metadata is written by atomic replacement. A rewrite that lands
    /// within one clock tick and keeps the length leaves the size and every
    /// time unchanged, so only the file identity shows that the remembered
    /// verdict was derived from a different file.
    #[test]
    fn same_length_replacement_with_identical_times_changes_the_stamp() {
        let directory = tempfile::tempdir().unwrap();
        let metadata = directory.path().join("task.json");
        std::fs::write(&metadata, br#"{"status":"running"}"#).unwrap();
        let before = Stamp::of(&metadata).unwrap();
        assert_eq!(Stamp::of(&metadata).unwrap(), before);
        crate::file_identity::replace_keeping_times(&metadata, br#"{"status":"stopped"}"#);
        assert_ne!(Stamp::of(&metadata).unwrap(), before);
    }
    #[test]
    fn completed_validation_laps_are_rate_limited() {
        let storage = tempfile::tempdir().unwrap();
        let memo = GcMemo::new(storage.path().to_path_buf());
        let now = Instant::now();
        assert!(memo.validation_due(now));
        assert!(!memo.validation_due(now + Duration::from_secs(59)));
        assert!(memo.validation_due(now + Duration::from_secs(60)));
        memo.finish_pass(PassReport {
            walk_finished: true,
            ..PassReport::default()
        });
        assert!(!memo.validation_due(now + Duration::from_secs(599)));
        assert!(memo.validation_due(now + Duration::from_secs(600)));
    }
}
