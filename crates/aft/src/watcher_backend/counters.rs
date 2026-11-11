use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WatcherOverflowPrefix {
    pub(crate) prefix: String,
    pub(crate) count: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct WatcherBackendExclusions {
    pub(crate) matcher_generation: u64,
    pub(crate) paths: Vec<PathBuf>,
    /// The next candidates that lost a slot to the exclusion cap, best first.
    /// Carried beside the installed set so the overflow log can say what the
    /// cap left watched.
    pub(crate) candidates_dropped: Vec<PathBuf>,
    pub(crate) queue_depth: Option<usize>,
}

impl Default for WatcherBackendExclusions {
    fn default() -> Self {
        Self {
            matcher_generation: 0,
            paths: Vec::new(),
            candidates_dropped: Vec::new(),
            queue_depth: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WatcherExclusionPlanningSnapshot {
    pub(crate) matcher_generation: u64,
    pub(crate) attempts: u32,
    pub(crate) attempt_limit: u32,
    pub(crate) status: &'static str,
    pub(crate) last_failure: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct WatcherCountersSnapshot {
    /// None on backends that do not plan exclusions. The exhausted retry budget
    /// and last planning failure remain visible without further filesystem events.
    pub(crate) exclusion_planning: Option<WatcherExclusionPlanningSnapshot>,
    /// Successful native stream allocations over this root's process lifetime,
    /// including its auxiliary paths and config-file fallback watch.
    pub(crate) fsevents_stream_creations_total: u64,
    pub(crate) raw_events_total: u64,
    pub(crate) raw_events_since_last_rescan: u64,
    pub(crate) invalidating_events_total: u64,
    pub(crate) invalidating_events_since_last_rescan: u64,
    pub(crate) paths_after_gitignore_total: u64,
    pub(crate) paths_after_gitignore_since_last_rescan: u64,
    pub(crate) paths_dispatched_total: u64,
    pub(crate) paths_dispatched_since_last_rescan: u64,
    /// Shared arrival recovery on every platform. Linux also registers new
    /// watches and reports files found there before acknowledging watcher cookies.
    pub(crate) directory_arrivals_total: u64,
    pub(crate) directory_subtree_walks_total: u64,
    pub(crate) directory_empty_total: u64,
    pub(crate) directory_walk_fallbacks_total: u64,
    pub(crate) overflows_total: u64,
    pub(crate) watch_lost_races_total: u64,
    pub(crate) overflows_during_rescan: u64,
    pub(crate) last_overflow_prefixes: Vec<WatcherOverflowPrefix>,
    pub(crate) rescans_buffer_overflow_total: u64,
    pub(crate) rescans_kernel_dropped_total: u64,
    pub(crate) rescans_user_dropped_total: u64,
    pub(crate) rescans_unknown_total: u64,
    pub(crate) last_rescan_at_ms: Option<u64>,
    pub(crate) last_rescan_cost_ms: Option<u64>,
    pub(crate) last_rescan_rss_delta_bytes: Option<i64>,
}

// After recording a drop's prefix summary, the filter changes RUNNING to RERUN
// using only these atomics. The project tree walk does not access the prefix
// lock or these atomics, so another drop can be recorded while that walk runs.
const WATCHER_RESCAN_IDLE: u8 = 0;
const WATCHER_RESCAN_RUNNING: u8 = 1;
const WATCHER_RESCAN_RERUN: u8 = 2;

#[derive(Debug, Default)]
pub(crate) struct WatcherCounters {
    pub(crate) bash_flush: Arc<crate::watcher_backend::flush::FlushControl>,
    pub(crate) bash_changes: std::sync::OnceLock<Arc<crate::bash_changed_files::Journal>>,
    overflow_ignore_evidence: AtomicU8,
    /// Legacy rescan work coalesces repeated drops, but a newly installed views
    /// snapshot must still learn that delivery was lost during that work.
    pub(crate) checkout_watcher_gap_pending: AtomicBool,
    fsevents_stream_creations_total: AtomicU64,
    raw_events_total: AtomicU64,
    raw_events_since_last_rescan: AtomicU64,
    invalidating_events_total: AtomicU64,
    invalidating_events_since_last_rescan: AtomicU64,
    paths_after_gitignore_total: AtomicU64,
    paths_after_gitignore_since_last_rescan: AtomicU64,
    paths_dispatched_total: AtomicU64,
    paths_dispatched_since_last_rescan: AtomicU64,
    directory_arrivals_total: AtomicU64,
    directory_subtree_walks_total: AtomicU64,
    directory_empty_total: AtomicU64,
    directory_walk_fallbacks_total: AtomicU64,
    directory_arrivals_in_flight: AtomicUsize,
    directory_files_unflushed: AtomicBool,
    overflows_total: AtomicU64,
    watch_lost_races_total: AtomicU64,
    overflows_during_rescan: AtomicU64,
    last_overflow_prefixes: RwLock<Vec<WatcherOverflowPrefix>>,
    observed_exclusion_prefixes: RwLock<Vec<WatcherOverflowPrefix>>,
    backend_exclusions: RwLock<WatcherBackendExclusions>,
    exclusion_planning: RwLock<Option<WatcherExclusionPlanningSnapshot>>,
    rescan_state: AtomicU8,
    rescan_again_reason: AtomicU8,
    rescans_buffer_overflow_total: AtomicU64,
    rescans_kernel_dropped_total: AtomicU64,
    rescans_user_dropped_total: AtomicU64,
    rescans_unknown_total: AtomicU64,
    last_rescan_at_ms: AtomicU64,
    last_rescan_cost_ms: AtomicU64,
    last_rescan_rss_delta_bytes: AtomicI64,
    last_rescan_rss_delta_known: AtomicBool,
    /// Call-graph source files the watcher thread has received but is still
    /// holding in its flush window, before they reach the dispatch channel.
    unflushed_source_paths: Mutex<BTreeSet<PathBuf>>,
    /// How many changed paths of any kind the watcher thread has taken into
    /// its flush window since the window last emptied. Unlike
    /// `unflushed_source_paths` it counts every file, so a reader that needs
    /// every pending change to reach the index store can tell that some are
    /// still held. Reset only once the window has handed its paths on.
    unflushed_path_count: AtomicUsize,
    /// Events the watcher thread is holding because the dispatch channel was
    /// full when it tried to hand them on.
    dispatch_backlog_events: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WatcherRescanInterval {
    pub(crate) raw_events: u64,
}

pub(crate) struct DirectoryArrivalGuard<'a>(&'a WatcherCounters);

impl Drop for DirectoryArrivalGuard<'_> {
    fn drop(&mut self) {
        self.0
            .directory_arrivals_in_flight
            .fetch_sub(1, Ordering::AcqRel);
    }
}

impl WatcherCounters {
    pub(crate) fn note_directory_arrival(&self) {
        self.directory_arrivals_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn directory_arrival_guard(&self) -> DirectoryArrivalGuard<'_> {
        self.directory_arrivals_in_flight
            .fetch_add(1, Ordering::AcqRel);
        DirectoryArrivalGuard(self)
    }

    pub(crate) fn note_directory_walk(&self) {
        self.directory_subtree_walks_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_empty_directory(&self) {
        self.directory_empty_total.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_directory_walk_fallback(&self) {
        self.directory_walk_fallbacks_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_directory_files_queued(&self) {
        self.directory_files_unflushed
            .store(true, Ordering::Release);
    }

    pub(crate) fn clear_directory_files_queued(&self) {
        self.directory_files_unflushed
            .store(false, Ordering::Release);
    }

    pub(crate) fn query_delivery_pending(&self) -> bool {
        self.directory_arrivals_in_flight.load(Ordering::Acquire) > 0
            || self.directory_files_unflushed.load(Ordering::Acquire)
            || self.dispatch_backlog() > 0
    }

    pub(crate) fn note_overflow_ignore_relevance(&self, relevant: bool) {
        self.overflow_ignore_evidence
            .fetch_or(if relevant { 3 } else { 1 }, Ordering::AcqRel);
    }

    pub(crate) fn take_overflow_ignore_relevance(&self) -> bool {
        // No path evidence (including synthetic rescans) is conservative: lost
        // events could include a changed ignore file.
        self.overflow_ignore_evidence.swap(0, Ordering::AcqRel) != 1
    }
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn note_fsevents_stream_creation(&self) {
        self.fsevents_stream_creations_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_raw_event(&self) {
        self.raw_events_total.fetch_add(1, Ordering::Relaxed);
        self.raw_events_since_last_rescan
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_invalidating_event(&self) {
        self.invalidating_events_total
            .fetch_add(1, Ordering::Relaxed);
        self.invalidating_events_since_last_rescan
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_paths_after_gitignore(&self, count: usize) {
        let count = count as u64;
        self.paths_after_gitignore_total
            .fetch_add(count, Ordering::Relaxed);
        self.paths_after_gitignore_since_last_rescan
            .fetch_add(count, Ordering::Relaxed);
    }

    pub(crate) fn note_paths_dispatched(&self, count: usize) {
        let count = count as u64;
        self.paths_dispatched_total
            .fetch_add(count, Ordering::Relaxed);
        self.paths_dispatched_since_last_rescan
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Record changed files the watcher thread holds in its flush window.
    /// Only files the call graph indexes are kept: a call-graph answer is the
    /// only reader, and other files cannot change one.
    pub(crate) fn note_unflushed_paths<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        let mut unflushed = self
            .unflushed_source_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unflushed.extend(
            paths
                .into_iter()
                .inspect(|_| {
                    self.unflushed_path_count.fetch_add(1, Ordering::AcqRel);
                })
                .filter(|path| crate::parser::detect_language(path).is_some())
                .cloned(),
        );
    }

    /// Whether the watcher thread holds changed paths of any kind in its
    /// flush window, not yet handed to the dispatch channel. Read it before
    /// the dispatch channel: the window is emptied only after its paths are
    /// in the channel.
    pub(crate) fn flush_window_holds_paths(&self) -> bool {
        self.unflushed_path_count.load(Ordering::Acquire) > 0
    }

    /// The flush window ended: its files are in the dispatch channel (or were
    /// filtered out), where the watcher drain accounts for them.
    pub(crate) fn clear_unflushed_paths(&self) {
        self.unflushed_source_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.unflushed_path_count.store(0, Ordering::Release);
        self.clear_directory_files_queued();
    }

    pub(crate) fn unflushed_paths(&self) -> BTreeSet<PathBuf> {
        self.unflushed_source_paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record how many events the watcher thread holds for a full dispatch
    /// channel.
    pub(crate) fn set_dispatch_backlog(&self, events: usize) {
        self.dispatch_backlog_events
            .store(events, Ordering::Release);
    }

    /// Events the watcher thread holds because the dispatch channel was full.
    pub(crate) fn dispatch_backlog(&self) -> usize {
        self.dispatch_backlog_events.load(Ordering::Acquire)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn note_watch_lost_race(&self) {
        self.watch_lost_races_total.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_overflow(
        &self,
        reason: crate::watcher_filter::RescanReason,
        prefixes: Vec<WatcherOverflowPrefix>,
    ) -> bool {
        self.overflows_total.fetch_add(1, Ordering::Relaxed);
        if let Some(journal) = self.bash_changes.get() {
            journal.gap();
        }
        *self
            .last_overflow_prefixes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prefixes;

        let reason = match reason {
            crate::watcher_filter::RescanReason::KernelDropped => 1,
            crate::watcher_filter::RescanReason::UserDropped => 2,
            crate::watcher_filter::RescanReason::Unknown => 3,
            crate::watcher_filter::RescanReason::BufferOverflow => 4,
        };
        loop {
            let state = self.rescan_state.load(Ordering::Acquire);
            if state == WATCHER_RESCAN_IDLE {
                return false;
            }
            self.rescan_again_reason.store(reason, Ordering::Release);
            if self
                .rescan_state
                .compare_exchange(
                    state,
                    WATCHER_RESCAN_RERUN,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.overflows_during_rescan.fetch_add(1, Ordering::Relaxed);
                self.checkout_watcher_gap_pending
                    .store(true, Ordering::Release);
                return true;
            }
        }
    }

    pub(crate) fn start_rescan(&self) {
        let _ = self.rescan_state.compare_exchange(
            WATCHER_RESCAN_IDLE,
            WATCHER_RESCAN_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn finish_rescan_walk(&self) -> Option<crate::watcher_filter::RescanReason> {
        loop {
            match self.rescan_state.load(Ordering::Acquire) {
                WATCHER_RESCAN_RERUN => {
                    if self
                        .rescan_state
                        .compare_exchange(
                            WATCHER_RESCAN_RERUN,
                            WATCHER_RESCAN_RUNNING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Some(match self.rescan_again_reason.load(Ordering::Acquire) {
                            1 => crate::watcher_filter::RescanReason::KernelDropped,
                            2 => crate::watcher_filter::RescanReason::UserDropped,
                            4 => crate::watcher_filter::RescanReason::BufferOverflow,
                            _ => crate::watcher_filter::RescanReason::Unknown,
                        });
                    }
                }
                WATCHER_RESCAN_RUNNING => {
                    if self
                        .rescan_state
                        .compare_exchange(
                            WATCHER_RESCAN_RUNNING,
                            WATCHER_RESCAN_IDLE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return None;
                    }
                }
                _ => return None,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn rescan_in_progress(&self) -> bool {
        self.rescan_state.load(Ordering::Acquire) != WATCHER_RESCAN_IDLE
    }

    pub(crate) fn set_observed_exclusion_prefixes(&self, prefixes: Vec<WatcherOverflowPrefix>) {
        *self
            .observed_exclusion_prefixes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prefixes;
    }

    pub(crate) fn observed_exclusion_prefixes(&self) -> Vec<WatcherOverflowPrefix> {
        self.observed_exclusion_prefixes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    // Windows cannot install these paths in ReadDirectoryChangesW, so its
    // backend records an empty list while still publishing the matcher
    // generation that its root handle setup observed.
    pub(crate) fn set_backend_exclusions(
        &self,
        matcher_generation: u64,
        paths: Vec<PathBuf>,
        candidates_dropped: Vec<PathBuf>,
    ) {
        *self
            .backend_exclusions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = WatcherBackendExclusions {
            matcher_generation,
            paths,
            candidates_dropped,
            queue_depth: None,
        };
    }

    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    pub(crate) fn set_exclusion_planning(&self, snapshot: WatcherExclusionPlanningSnapshot) {
        *self
            .exclusion_planning
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(snapshot);
    }

    pub(crate) fn backend_exclusions(&self) -> WatcherBackendExclusions {
        self.backend_exclusions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn begin_rescan(
        &self,
        reason: crate::watcher_filter::RescanReason,
    ) -> WatcherRescanInterval {
        match reason {
            crate::watcher_filter::RescanReason::BufferOverflow => {
                &self.rescans_buffer_overflow_total
            }
            crate::watcher_filter::RescanReason::KernelDropped => {
                &self.rescans_kernel_dropped_total
            }
            crate::watcher_filter::RescanReason::UserDropped => &self.rescans_user_dropped_total,
            crate::watcher_filter::RescanReason::Unknown => &self.rescans_unknown_total,
        }
        .fetch_add(1, Ordering::Relaxed);

        let raw_events = self.raw_events_since_last_rescan.swap(0, Ordering::Relaxed);
        self.invalidating_events_since_last_rescan
            .swap(0, Ordering::Relaxed);
        self.paths_after_gitignore_since_last_rescan
            .swap(0, Ordering::Relaxed);
        self.paths_dispatched_since_last_rescan
            .swap(0, Ordering::Relaxed);
        WatcherRescanInterval { raw_events }
    }

    pub(crate) fn finish_rescan(&self, cost_ms: u64, rss_delta_bytes: Option<i64>) {
        self.last_rescan_cost_ms.store(cost_ms, Ordering::Relaxed);
        if let Some(delta) = rss_delta_bytes {
            self.last_rescan_rss_delta_bytes
                .store(delta, Ordering::Relaxed);
            self.last_rescan_rss_delta_known
                .store(true, Ordering::Relaxed);
        } else {
            self.last_rescan_rss_delta_known
                .store(false, Ordering::Relaxed);
        }
        let at_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        self.last_rescan_at_ms.store(at_ms, Ordering::Release);
    }

    pub(crate) fn snapshot(&self) -> WatcherCountersSnapshot {
        let last_rescan_at_ms = self.last_rescan_at_ms.load(Ordering::Acquire);
        WatcherCountersSnapshot {
            exclusion_planning: self
                .exclusion_planning
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            fsevents_stream_creations_total: self
                .fsevents_stream_creations_total
                .load(Ordering::Relaxed),
            raw_events_total: self.raw_events_total.load(Ordering::Relaxed),
            raw_events_since_last_rescan: self.raw_events_since_last_rescan.load(Ordering::Relaxed),
            invalidating_events_total: self.invalidating_events_total.load(Ordering::Relaxed),
            invalidating_events_since_last_rescan: self
                .invalidating_events_since_last_rescan
                .load(Ordering::Relaxed),
            paths_after_gitignore_total: self.paths_after_gitignore_total.load(Ordering::Relaxed),
            paths_after_gitignore_since_last_rescan: self
                .paths_after_gitignore_since_last_rescan
                .load(Ordering::Relaxed),
            paths_dispatched_total: self.paths_dispatched_total.load(Ordering::Relaxed),
            directory_arrivals_total: self.directory_arrivals_total.load(Ordering::Relaxed),
            directory_subtree_walks_total: self
                .directory_subtree_walks_total
                .load(Ordering::Relaxed),
            directory_empty_total: self.directory_empty_total.load(Ordering::Relaxed),
            directory_walk_fallbacks_total: self
                .directory_walk_fallbacks_total
                .load(Ordering::Relaxed),
            paths_dispatched_since_last_rescan: self
                .paths_dispatched_since_last_rescan
                .load(Ordering::Relaxed),
            overflows_total: self.overflows_total.load(Ordering::Relaxed),
            watch_lost_races_total: self.watch_lost_races_total.load(Ordering::Relaxed),
            overflows_during_rescan: self.overflows_during_rescan.load(Ordering::Relaxed),
            last_overflow_prefixes: self
                .last_overflow_prefixes
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            rescans_buffer_overflow_total: self
                .rescans_buffer_overflow_total
                .load(Ordering::Relaxed),
            rescans_kernel_dropped_total: self.rescans_kernel_dropped_total.load(Ordering::Relaxed),
            rescans_user_dropped_total: self.rescans_user_dropped_total.load(Ordering::Relaxed),
            rescans_unknown_total: self.rescans_unknown_total.load(Ordering::Relaxed),
            last_rescan_at_ms: (last_rescan_at_ms != 0).then_some(last_rescan_at_ms),
            last_rescan_cost_ms: (last_rescan_at_ms != 0)
                .then(|| self.last_rescan_cost_ms.load(Ordering::Relaxed)),
            last_rescan_rss_delta_bytes: (last_rescan_at_ms != 0
                && self.last_rescan_rss_delta_known.load(Ordering::Relaxed))
            .then(|| self.last_rescan_rss_delta_bytes.load(Ordering::Relaxed)),
        }
    }
}

static WATCHER_COUNTERS_BY_ROOT: std::sync::OnceLock<
    Mutex<BTreeMap<PathBuf, Arc<WatcherCounters>>>,
> = std::sync::OnceLock::new();

pub(crate) fn watcher_counters_for_root(root: &Path) -> Arc<WatcherCounters> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let registry = WATCHER_COUNTERS_BY_ROOT.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(counters) = registry.get(&root) {
        return Arc::clone(counters);
    }
    let counters = Arc::new(WatcherCounters::default());
    registry.insert(root, Arc::clone(&counters));
    counters
}
