use std::path::PathBuf;

use crate::watcher_backend::counters::WatcherCounters;
use crate::watcher_filter::RescanReason;

pub use crate::watcher_backend::counters::{
    WatcherBackendExclusions, WatcherExclusionPlanningSnapshot, WatcherOverflowPrefix,
    WatcherRescanInterval,
};

pub trait WatcherBackendExclusionsTestExt {
    fn matcher_generation(&self) -> u64;
    fn paths(&self) -> &Vec<PathBuf>;
}

impl WatcherBackendExclusionsTestExt for WatcherBackendExclusions {
    fn matcher_generation(&self) -> u64 {
        self.matcher_generation
    }
    fn paths(&self) -> &Vec<PathBuf> {
        &self.paths
    }
}

pub trait WatcherRescanIntervalTestExt {
    fn raw_events(&self) -> u64;
}

impl WatcherRescanIntervalTestExt for WatcherRescanInterval {
    fn raw_events(&self) -> u64 {
        self.raw_events
    }
}

pub trait WatcherCountersTestExt {
    fn note_overflow_ignore_relevance(&self, relevant: bool);
    fn set_exclusion_planning(&self, snapshot: WatcherExclusionPlanningSnapshot);
    fn note_fsevents_stream_creation(&self);
    fn note_raw_event(&self);
    fn note_invalidating_event(&self);
    fn note_paths_after_gitignore(&self, count: usize);
    fn note_paths_dispatched(&self, count: usize);
    fn note_overflow(&self, reason: RescanReason, prefixes: Vec<WatcherOverflowPrefix>) -> bool;
    fn begin_rescan(&self, reason: RescanReason) -> WatcherRescanInterval;
    fn finish_rescan(&self, cost_ms: u64, rss_delta_bytes: Option<i64>);
    fn backend_exclusions(&self) -> WatcherBackendExclusions;
}

impl WatcherCountersTestExt for WatcherCounters {
    fn note_overflow_ignore_relevance(&self, relevant: bool) {
        self.note_overflow_ignore_relevance(relevant);
    }
    fn set_exclusion_planning(&self, snapshot: WatcherExclusionPlanningSnapshot) {
        self.set_exclusion_planning(snapshot);
    }
    fn note_fsevents_stream_creation(&self) {
        self.note_fsevents_stream_creation();
    }
    fn note_raw_event(&self) {
        self.note_raw_event();
    }
    fn note_invalidating_event(&self) {
        self.note_invalidating_event();
    }
    fn note_paths_after_gitignore(&self, count: usize) {
        self.note_paths_after_gitignore(count);
    }
    fn note_paths_dispatched(&self, count: usize) {
        self.note_paths_dispatched(count);
    }
    fn note_overflow(&self, reason: RescanReason, prefixes: Vec<WatcherOverflowPrefix>) -> bool {
        self.note_overflow(reason, prefixes)
    }
    fn begin_rescan(&self, reason: RescanReason) -> WatcherRescanInterval {
        self.begin_rescan(reason)
    }
    fn finish_rescan(&self, cost_ms: u64, rss_delta_bytes: Option<i64>) {
        self.finish_rescan(cost_ms, rss_delta_bytes);
    }
    fn backend_exclusions(&self) -> WatcherBackendExclusions {
        self.backend_exclusions()
    }
}

pub trait WatcherCountersSnapshotTestExt {
    fn rescans_unknown_total(&self) -> u64;
    fn exclusion_planning(&self) -> &Option<WatcherExclusionPlanningSnapshot>;
}

impl WatcherCountersSnapshotTestExt for crate::watcher_backend::counters::WatcherCountersSnapshot {
    fn rescans_unknown_total(&self) -> u64 {
        self.rescans_unknown_total
    }

    fn exclusion_planning(&self) -> &Option<WatcherExclusionPlanningSnapshot> {
        &self.exclusion_planning
    }
}

pub trait WatcherExclusionPlanningSnapshotTestExt {
    fn attempts(&self) -> u32;
}

impl WatcherExclusionPlanningSnapshotTestExt for WatcherExclusionPlanningSnapshot {
    fn attempts(&self) -> u32 {
        self.attempts
    }
}
