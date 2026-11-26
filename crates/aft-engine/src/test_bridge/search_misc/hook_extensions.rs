pub mod backup {
    pub fn new_for_test() -> crate::backup::BackupStore {
        crate::backup::BackupStore::new_for_unit_test()
    }

    pub use crate::backup::{A03_FINGERPRINT_HASHES, A03_LIVE_FINGERPRINT_READS};
    use std::path::Path;

    pub fn reset_capture_read_count(path: &Path) {
        crate::backup::reset_capture_read_count(path);
    }

    pub fn capture_read_count(path: &Path) -> usize {
        crate::backup::capture_read_count(path)
    }
}

pub mod grep_executor {
    use std::time::Duration;

    pub fn with_walk_limit<R>(limit: usize, run: impl FnOnce() -> R) -> R {
        crate::grep_executor::with_walk_limit(limit, run)
    }

    pub fn with_fallback_walk_budget_for_test<R>(budget: Duration, run: impl FnOnce() -> R) -> R {
        crate::grep_executor::with_fallback_walk_budget_for_test(budget, run)
    }

    pub fn with_fallback_walk_entry_delay_for_test<R>(
        delay: Duration,
        run: impl FnOnce() -> R,
    ) -> R {
        crate::grep_executor::with_fallback_walk_entry_delay_for_test(delay, run)
    }

    pub fn with_fallback_walk_sort_metadata_delay_for_test<R>(
        delay: Duration,
        run: impl FnOnce() -> R,
    ) -> R {
        crate::grep_executor::with_fallback_walk_sort_metadata_delay_for_test(delay, run)
    }

    pub fn note_fallback_walk_for_test() {
        crate::grep_executor::note_fallback_walk_for_test();
    }
}

pub mod large_read {
    use crate::large_read::Site;

    pub fn with_line_scan_failure<T>(site: Site, work: impl FnOnce() -> T) -> T {
        crate::large_read::with_line_scan_failure(site, work)
    }

    pub fn with_step_tap<T>(tap: impl FnMut(u64) + 'static, work: impl FnOnce() -> T) -> T {
        crate::large_read::with_step_tap(tap, work)
    }

    pub fn with_test_steps<T>(step: usize, large: u64, work: impl FnOnce() -> T) -> T {
        crate::large_read::with_test_steps(step, large, work)
    }

    pub fn widest_health_for_test() -> serde_json::Value {
        crate::large_read::widest_health_for_test()
    }
}

pub mod memory_census {
    use crate::memory::{AllocatorObservation, MemoryRollupSnapshot, RootMemoryRollup};
    use std::collections::BTreeMap;

    pub fn rollup_snapshot(
        roots_status: &'static str,
        roots: BTreeMap<String, RootMemoryRollup>,
        allocator: AllocatorObservation,
    ) -> MemoryRollupSnapshot {
        crate::memory_census::rollup_snapshot(roots_status, roots, allocator)
    }
}

pub mod root_cache {
    use std::path::Path;

    pub fn prepare_cache_root_for_test(
        config: &mut crate::config::Config,
    ) -> Option<crate::config_resolve::ConfigWarning> {
        crate::reader_floor::prepare_cache_root_for_test(config)
    }

    pub fn acquire_cache_cleanup_lease(storage: &Path) -> std::io::Result<std::fs::File> {
        aft_base::cache_location::acquire_cache_cleanup_lease(storage)
    }
}

pub mod standing_roots {
    pub fn reconciliation_work_for_test() -> [i64; 6] {
        crate::standing_roots::reconciliation_work_for_test()
    }
}

pub mod semantic_index {
    use std::path::Path;

    pub fn record_query_embed_observation_for_test(root: &Path, elapsed_ms: u64, timed_out: bool) {
        crate::semantic_index::record_query_embed_observation_for_test(root, elapsed_ms, timed_out);
    }
}

pub mod watcher_filter {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn force_exclusion_failure_for_test(root: &std::path::Path) -> impl Drop {
        crate::watcher_filter::force_exclusion_failure_for_test(root)
    }
}

pub mod root_identity {
    pub mod key {
        use std::path::{Path, PathBuf};
        use std::sync::{atomic::AtomicUsize, mpsc::Receiver, Arc};

        pub fn artifact_path_identity_key_for_test(project_root: &Path) -> String {
            crate::root_identity::key::artifact_path_identity_key_for_test(project_root)
        }

        pub fn git_root_commit_probe_override_lock_for_test() -> impl Drop {
            crate::root_identity::key::git_root_commit_probe_override_lock_for_test()
        }

        pub fn force_git_root_commit_probe_transient_for_paths_for_test(
            roots: Vec<PathBuf>,
            detail: impl Into<String>,
        ) -> impl Drop {
            crate::root_identity::key::force_git_root_commit_probe_transient_for_paths_for_test(
                roots, detail,
            )
        }

        pub fn force_git_root_commit_probe_held_transient_for_paths_for_test(
            roots: Vec<PathBuf>,
            attempts: Arc<AtomicUsize>,
            release: Receiver<()>,
        ) -> impl Drop {
            crate::root_identity::key::force_git_root_commit_probe_held_transient_for_paths_for_test(
                roots, attempts, release,
            )
        }
    }
}

pub mod lsp {
    pub mod document {
        pub use crate::lsp::document::CONTENT_HASH_READS;
    }

    pub mod registry {
        pub use crate::lsp::registry::{BIOME_CONFIG_DIRECTORY_PROBES, BIOME_ROOT_WALKS};
    }

    pub mod manager {
        use crate::lsp::manager::LspManager;

        pub mod regression_tests {
            #[cfg(unix)]
            pub use crate::lsp::manager::idle_fixture::{
                drain_flycheck_end_for_maintenance_test, finish_flycheck_for_maintenance_test,
                overflow_flycheck_inbox_for_maintenance_test, FLYCHECK_PUBLISH_SETTLE_FOR_TEST,
            };
            pub use crate::lsp::manager::idle_fixture::{
                enqueue_maintenance_backlog_for_test, publish_after_document_sync_for_test,
                IdleLspFixture,
            };
        }

        pub fn settled_lsp<R>(
            manager: &mut LspManager,
            operation: impl Fn(&mut LspManager) -> R,
        ) -> R {
            crate::lsp::manager::settled_lsp(manager, operation)
        }
    }
}

pub mod file_read_exact {
    use crate::file_read_exact::ReadHooks;
    use std::sync::Arc;

    pub fn with_mid_read(mid_read: Arc<dyn Fn() + Send + Sync>) -> ReadHooks {
        ReadHooks::with_mid_read(mid_read)
    }
}

pub trait IdleLspFixtureTestExt {
    fn ctx(&self) -> &std::sync::Arc<crate::context::AppContext>;
    fn config(&self) -> &crate::config::Config;
    fn busy(&self) -> &std::path::PathBuf;
    fn idle(&self) -> &std::path::PathBuf;
}

impl IdleLspFixtureTestExt for crate::lsp::manager::idle_fixture::IdleLspFixture {
    fn ctx(&self) -> &std::sync::Arc<crate::context::AppContext> {
        &self.ctx
    }
    fn config(&self) -> &crate::config::Config {
        &self.config
    }
    fn busy(&self) -> &std::path::PathBuf {
        &self.busy
    }
    fn idle(&self) -> &std::path::PathBuf {
        &self.idle
    }
}

pub mod synapse_embed {
    pub mod route_tests {
        pub use crate::synapse_embed::route_fixture::{Counts, Daemon};
    }
}

pub trait DaemonTestExt {
    fn path(&self) -> &std::path::PathBuf;
    fn counts(
        &self,
    ) -> &std::sync::Arc<(
        std::sync::Mutex<crate::synapse_embed::route_fixture::Counts>,
        std::sync::Condvar,
    )>;
}

impl DaemonTestExt for crate::synapse_embed::route_fixture::Daemon {
    fn path(&self) -> &std::path::PathBuf {
        &self.path
    }
    fn counts(
        &self,
    ) -> &std::sync::Arc<(
        std::sync::Mutex<crate::synapse_embed::route_fixture::Counts>,
        std::sync::Condvar,
    )> {
        &self.counts
    }
}

pub trait CountsTestExt {
    fn opens(&self) -> usize;
    fn closes(&self) -> usize;
    fn active(&self) -> usize;
    fn queries(&self) -> usize;
}

impl CountsTestExt for crate::synapse_embed::route_fixture::Counts {
    fn opens(&self) -> usize {
        self.opens
    }
    fn closes(&self) -> usize {
        self.closes
    }
    fn active(&self) -> usize {
        self.active
    }
    fn queries(&self) -> usize {
        self.queries
    }
}
