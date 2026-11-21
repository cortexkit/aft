//! Extensions used by upper tests without changing the engine's inherent API.

use crate::config::Config;
use crate::context::{App, AppContext};
use crate::data_layer::checkout::{Checkout, CheckoutView, DataError};
use crate::data_layer::runtime::DataCheckout;
use crate::provisional_identity_retry::RetrySlot;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

// Keep the test contract and its forwarding signature together. The calls in
// the engine still resolve to their inherent methods.
macro_rules! test_methods {
    ($extension:ident for $engine:ty { $( $(#[$attr:meta])* fn $name:ident $(<$parameter:ident>)? (&self $(, $arg:ident: $ty:ty)*) $(-> $result:ty)?; )* }
        $(associated { $(fn $static_name:ident($($static_arg:ident: $static_ty:ty),*) -> $static_result:ty;)* })?
    ) => {
        pub trait $extension {
            $( $(#[$attr])* fn $name $(<$parameter>)? (&self $(, $arg: $ty)*) $(-> $result)?; )*
            $($(fn $static_name($($static_arg: $static_ty),*) -> $static_result;)*)?
        }
        impl $extension for $engine {
            $( $(#[$attr])* fn $name $(<$parameter>)? (&self $(, $arg: $ty)*) $(-> $result)? {
                <$engine>::$name(self $(, $arg)*)
            } )*
            $($(fn $static_name($($static_arg: $static_ty),*) -> $static_result {
                <$engine>::$static_name($($static_arg),*)
            })*)?
        }
    };
}

test_methods! { AppTestExt for App {
    fn with_database_slot_locked_for_test(&self, run: impl FnOnce());
    fn isolate_test_config(&self, config: Config) -> Config;
}}

test_methods! { AppContextTestExt for AppContext {
    fn after_next_policy_memory_miss_for_test(&self, hook: impl FnOnce() + Send + 'static);
    fn after_next_policy_read_for_test(&self, hook: impl FnOnce(&AppContext) + Send + 'static);
    fn run_after_policy_memory_miss_hook_for_test(&self);
    fn run_after_policy_read_hook_for_test(&self);
    fn policy_confinement_reads_for_test(&self) -> usize;
    fn mark_database_runtime_initializing_for_test(&self);
    fn note_database_open_attempt_for_test(&self);
    fn database_open_slot(&self) -> (&parking_lot::Mutex<crate::database_open::DatabaseOpenSlot>, &parking_lot::Condvar);
    fn record_worktree_bridge_probe_spawn_for_test(&self);
    fn replace_checkpoint_store_for_test(&self, store: crate::checkpoint::CheckpointStore);
    fn path_restriction_root_memo_is_empty_for_test(&self) -> bool;
    fn worktree_bridge_probe_spawns_for_test(&self) -> u64;
    fn force_worktree_bridge_reprobe_for_test(&self, enabled: bool);
    fn watcher_runtime_thread_id_for_test(&self) -> Option<std::thread::ThreadId>;
    fn hold_data_checkout_slot_for_test(&self) -> impl Drop + '_;
    fn data_note_paths(&self, paths: &[PathBuf]);
    fn try_build_status_push_snapshot(&self) -> Option<crate::protocol::StatusPayload>;
    fn callgraph_executor_for_test(&self) -> Option<crate::executor::RootMaintenanceExecutor>;
    fn note_launch_restore_attempt(&self);
    fn launch_restore_attempts(&self) -> usize;
    fn pending_reconciliation_snapshot_for_test(&self) -> (BTreeSet<PathBuf>, BTreeSet<PathBuf>, bool);
    fn filter_registry_loaded(&self) -> bool;
    fn background_callgraph_refresh_allowed(&self) -> bool;
    fn artifact_cache_key_derivation_count_for_test(&self) -> u64;
    fn set_unbound_build_abandon_grace_for_test(&self, grace: Duration);
    fn lsp_post_edit_worker_running_for_test(&self) -> bool;
} associated {
    fn ignore_walks_on_current_thread_for_test() -> u64;
}}

test_methods! { ConfigLiveStateTestExt for crate::config_live::ConfigLiveState {
    fn wait_for_watch_attachment(&self);
    fn project_fallback_file_for_test(&self) -> Option<PathBuf>;
    fn project_watcher_sees_config_for_test(&self) -> bool;
}}

test_methods! { RetrySlotTestExt for RetrySlot {
    fn is_armed(&self) -> bool;
    fn attempts_for_test(&self) -> Option<u32>;
}}

test_methods! { DataCheckoutTestExt for DataCheckout {
    fn wait_for_memory_profile_idle_for_test(&self);
    fn note_paths(&self, paths: &[PathBuf]);
    fn request_refresh(&self);
    fn meaning_status(&self) -> crate::data_layer::meaning::MeaningState;
    fn root(&self) -> &Path;
    fn note_outcome_for_test<T>(&self, stage: &'static str, result: &Result<T, DataError>);
    fn configure_meaning(&self, config: Option<&crate::config::SemanticBackendConfig>);
    fn filling(&self) -> bool;
    #[cfg(all(feature = "wasm-grammars", feature = "test-hooks", not(all(target_os = "windows", target_arch = "aarch64"))))]
    fn grammar_driver_running_for_test(&self) -> bool;
} associated {
    fn open(root: &Path, storage: &Path, executor: Option<crate::executor::RootMaintenanceExecutor>) -> Result<Arc<DataCheckout>, DataError>;
    fn open_planes(root: &Path, storage: &Path, executor: Option<crate::executor::RootMaintenanceExecutor>, planes: crate::data_layer::checkout::Planes) -> Result<Arc<DataCheckout>, DataError>;
}}

test_methods! { ChildCheckoutsTestExt for crate::data_layer::parent::ChildCheckouts {
    fn insert_for_test(&self, parent: &Path, child: &Path, checkout: Arc<DataCheckout>);
    fn bound_for_test(&self) -> Vec<Arc<DataCheckout>>;
}}

test_methods! { CheckoutTestExt for Checkout {
    fn has_unpublished_luau_changes(&self) -> bool;
    #[cfg(feature = "wasm-grammars")]
    fn retry_grammar_gaps(&self) -> Result<bool, DataError>;
}}

test_methods! { SnapshotStoreTestExt for crate::page_collect::snapshot::SnapshotStore {
    fn live_count(&self) -> usize;
} associated {
    fn open(storage_dir: &Path) -> crate::page_collect::snapshot::SnapshotStore;
}}

pub use crate::data_layer::host::IndexHost;
pub use crate::executor::{ExecutorApp, ExecutorContext};

test_methods! { VectorPlaneTestExt for crate::data_layer::vectors::VectorPlane {
    fn cached_snapshot(&self, view: &CheckoutView) -> Option<crate::data_layer::vectors::MeaningSnapshot>;
}}

test_methods! { LaunchGrantStateTestExt for crate::context::launch_grants::LaunchGrantState {
    fn note_probe(&self, path: &Path);
    #[cfg(unix)]
    fn probe_count(&self, path: &Path) -> usize;
}}

pub trait UnrestoredGrantTestExt {
    fn path(&self) -> &PathBuf;
    fn retryable(&self) -> bool;
}

impl UnrestoredGrantTestExt for crate::context::launch_grants::UnrestoredGrant {
    fn path(&self) -> &PathBuf {
        &self.path
    }
    fn retryable(&self) -> bool {
        self.retryable
    }
}

pub trait StatusEmitterTestExt {
    fn memory_census_builds(&self) -> &std::sync::atomic::AtomicUsize;
}

impl StatusEmitterTestExt for crate::context::StatusEmitter {
    fn memory_census_builds(&self) -> &std::sync::atomic::AtomicUsize {
        &self.memory_census_builds
    }
}

pub trait DataStoreReportTestExt {
    fn health(&self) -> crate::data_layer::surfaces::DataStoreHealth;
    fn files(&self) -> Option<usize>;
    fn callgraph(&self) -> &crate::data_layer::surfaces::PlaneReport;
    fn pending_edits(&self) -> usize;
}

impl DataStoreReportTestExt for crate::data_layer::surfaces::DataStoreReport {
    fn health(&self) -> crate::data_layer::surfaces::DataStoreHealth {
        self.health()
    }
    fn files(&self) -> Option<usize> {
        self.files
    }
    fn callgraph(&self) -> &crate::data_layer::surfaces::PlaneReport {
        &self.callgraph
    }
    fn pending_edits(&self) -> usize {
        self.pending_edits
    }
}

pub trait CheckoutViewTestExt {
    fn parse_gaps_identity(&self) -> aft_store::ids::Digest128;
    fn outline_symbols(&self, path: &[u8]) -> Result<Option<StoredOutline>, DataError>;
}

impl CheckoutViewTestExt for CheckoutView {
    fn parse_gaps_identity(&self) -> aft_store::ids::Digest128 {
        self.parse_gaps_identity()
    }

    fn outline_symbols(&self, path: &[u8]) -> Result<Option<StoredOutline>, DataError> {
        self.outline_symbols(path)
            .map(|found| found.map(StoredOutline))
    }
}

pub struct StoredOutline(crate::data_layer::outline::StoredOutline);
impl StoredOutline {
    pub fn symbols(&self) -> &[aft_store::content::OutlineSymbol] {
        &self.0.symbols
    }
}

pub struct PendingDatabaseOpen<'a>(&'a crate::database_open::DatabaseOpenRequest);
impl PendingDatabaseOpen<'_> {
    pub fn epoch(&self) -> u64 {
        self.0.epoch
    }
    pub fn dispatched_at(&self) -> Option<std::time::Instant> {
        self.0.dispatched_at
    }
    pub fn storage_root(&self) -> &PathBuf {
        &self.0.storage_root
    }
}

pub trait DatabaseOpenSlotTestExt {
    fn epoch(&self) -> u64;
    fn committed_at(&self) -> std::time::Instant;
    fn running(&self) -> bool;
    fn pending(&self) -> Option<PendingDatabaseOpen<'_>>;
}

impl DatabaseOpenSlotTestExt for crate::database_open::DatabaseOpenSlot {
    fn epoch(&self) -> u64 {
        self.epoch
    }
    fn committed_at(&self) -> std::time::Instant {
        self.committed_at
    }
    fn running(&self) -> bool {
        self.running
    }
    fn pending(&self) -> Option<PendingDatabaseOpen<'_>> {
        self.pending.as_ref().map(PendingDatabaseOpen)
    }
}
