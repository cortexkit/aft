//! Test access to the locks read by the maintenance probes.

/// The locks behind the subc maintenance probes (see
/// [`super::AppContext::watcher_drain_has_work`] and its siblings), one per field a
/// probe reads. Tests hold each in turn to prove no probe waits for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceProbeLock {
    WatcherReceiver,
    WatcherDrainSlice,
    Tier2RefreshScheduler,
    LspManager,
    ConfigureMaintenanceJobs,
    ParkedConfigureTail,
    SemanticIndexReceiver,
    SemanticRefreshEvents,
    SemanticRefreshWorker,
}

impl MaintenanceProbeLock {
    pub const ALL: [Self; 9] = [
        Self::WatcherReceiver,
        Self::WatcherDrainSlice,
        Self::Tier2RefreshScheduler,
        Self::LspManager,
        Self::ConfigureMaintenanceJobs,
        Self::ParkedConfigureTail,
        Self::SemanticIndexReceiver,
        Self::SemanticRefreshEvents,
        Self::SemanticRefreshWorker,
    ];
}
