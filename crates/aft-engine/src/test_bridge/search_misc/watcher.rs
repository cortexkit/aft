use std::path::{Path, PathBuf};

use crate::watcher_backend::counters::WatcherCounters;
use crate::watcher_filter::{self, SharedGitignore};

pub const WATCHER_EXCLUSION_LIMIT: usize = watcher_filter::WATCHER_EXCLUSION_LIMIT;

pub fn note_unflushed_paths<'a>(
    counters: &WatcherCounters,
    paths: impl IntoIterator<Item = &'a PathBuf>,
) {
    counters.note_unflushed_paths(paths);
}

pub fn clear_unflushed_paths(counters: &WatcherCounters) {
    counters.clear_unflushed_paths();
}

#[derive(Default)]
pub struct WatcherExclusionPlan(watcher_filter::WatcherExclusionPlan);

#[derive(Debug)]
pub struct ExclusionPlanUnavailable(watcher_filter::ExclusionPlanUnavailable);

type Outcome = Result<WatcherExclusionPlan, ExclusionPlanUnavailable>;

pub fn derive_watcher_exclusion_plan(
    root: &Path,
    matcher: &SharedGitignore,
    max_paths: Option<usize>,
) -> Outcome {
    watcher_filter::derive_watcher_exclusion_plan(root, matcher, max_paths)
        .map(WatcherExclusionPlan)
        .map_err(ExclusionPlanUnavailable)
}

pub fn watcher_exclusion_paths(plan: &WatcherExclusionPlan) -> Vec<PathBuf> {
    watcher_filter::watcher_exclusion_paths(&plan.0.selected)
}

pub struct WatcherExclusionPaths(watcher_filter::WatcherExclusionPaths);

impl WatcherExclusionPaths {
    pub fn new(root: &Path, plan: &WatcherExclusionPlan) -> Self {
        Self(watcher_filter::WatcherExclusionPaths::new(
            root,
            &plan.0.selected,
        ))
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.0.paths()
    }
}

#[derive(Default)]
pub struct WatcherExclusionRetry(watcher_filter::WatcherExclusionRetry);

impl WatcherExclusionRetry {
    pub fn attempt(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &WatcherCounters,
        derive: impl FnOnce() -> Outcome,
    ) -> Option<Outcome> {
        self.0
            .attempt(root, generation, counters, || {
                derive().map(|plan| plan.0).map_err(|err| err.0)
            })
            .map(|outcome| {
                outcome
                    .map(WatcherExclusionPlan)
                    .map_err(ExclusionPlanUnavailable)
            })
    }

    pub fn replacement(
        &mut self,
        root: &Path,
        generation: u64,
        counters: &WatcherCounters,
        installed: &mut WatcherExclusionPaths,
        matcher: &SharedGitignore,
        derive: impl FnOnce() -> Outcome,
    ) -> Option<()> {
        self.0
            .replacement(
                root,
                generation,
                counters,
                &mut installed.0,
                matcher,
                || derive().map(|plan| plan.0).map_err(|err| err.0),
            )
            .map(|update| match update {
                watcher_filter::WatcherExclusionUpdate::Planned(plan) => drop(plan),
                watcher_filter::WatcherExclusionUpdate::Reduced => (),
            })
    }
}
