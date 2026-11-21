pub fn with_test_reader<T>(reader: Option<&str>, run: impl FnOnce() -> T) -> T {
    crate::late_result_bus::with_test_reader(reader, run)
}

pub mod test_config_file {
    pub use crate::config_live::test_config_file::write;
}

pub mod meaning_query {
    pub use crate::data_layer::meaning_query::test_support::{serve, Gate};
}

pub mod reader_floor {
    use crate::cache_location::{CleanupCandidate, CleanupReport};
    use crate::config::Config;
    use crate::config_resolve::{ConfigTier, ConfigWarning};
    use std::path::Path;

    pub fn prepare_cache_root_for_test(config: &mut Config) -> Option<ConfigWarning> {
        crate::reader_floor::prepare_cache_root_for_test(config)
    }

    pub fn clean_old_for_test(
        tiers: &[ConfigTier],
        storage: &Path,
        confirm: impl FnMut(&[CleanupCandidate]) -> bool,
    ) -> CleanupReport {
        crate::reader_floor::clean_old_for_test(tiers, storage, confirm)
    }
}

pub mod identity_clock {
    use std::time::Duration;

    pub fn with_clock_for_test<T>(run: impl FnOnce() -> T) -> T {
        crate::provisional_identity_retry::with_clock_for_test(run)
    }

    pub fn advance_clock_for_test(elapsed: Duration) {
        crate::provisional_identity_retry::advance_clock_for_test(elapsed);
    }
}

pub mod configure {
    use crate::runtime_drain::configure::{self, DatabaseRuntime, Hooks};

    pub fn registered() -> bool {
        configure::registered()
    }

    pub fn command_hooks() -> Option<&'static Hooks> {
        configure::command_hooks()
    }

    pub fn database_runtime() -> Option<DatabaseRuntime> {
        configure::database_runtime()
    }
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
pub mod grammar {
    pub use crate::data_layer::grammar::extraction_probe::TestExtraction;

    pub fn override_extraction_for_test(source: &[u8], outcome: TestExtraction) -> impl Drop {
        crate::data_layer::grammar::override_extraction_for_test(source, outcome)
    }
}

#[cfg(all(
    feature = "wasm-grammars",
    feature = "test-hooks",
    not(all(target_os = "windows", target_arch = "aarch64"))
))]
pub mod test_gates {
    pub use crate::data_layer::test_gates::{fail_next_grammar_retry, hold, Hold, Point};
}

pub mod meaning {
    pub use crate::data_layer::meaning::MeaningState;
    use std::path::Path;

    #[cfg(target_os = "linux")]
    pub fn use_constant_vectors(root: std::path::PathBuf) {
        crate::data_layer::meaning::use_constant_vectors(root);
    }

    pub fn fill_with_constant_vectors_for_test(root: &Path, dimensions: usize) -> impl Drop {
        crate::data_layer::meaning::fill_with_constant_vectors_for_test(root, dimensions)
    }

    pub fn waiting(coverage: &crate::data_layer::vectors::Coverage) -> bool {
        crate::data_layer::meaning::waiting(coverage)
    }
}

pub mod late_result_bus {
    pub fn hooks() -> Option<&'static crate::late_result_bus::Hooks> {
        crate::late_result_bus::hooks()
    }

    pub async fn card_wake(body: &[u8], msg_id: &str) -> Result<(), &'static str> {
        crate::late_result_bus::readers::card_wake(body, msg_id).await
    }

    pub mod status {
        pub fn test_value(session: &str) -> Option<serde_json::Value> {
            crate::late_result_bus::status::test_value(session)
        }
    }

    pub mod display {
        pub use crate::late_result_bus::display::ready::ReadyPublisher;
        use crate::late_result_bus::{self, DisplayPublish, Hooks};

        pub async fn announce(
            hooks: Option<&Hooks>,
            id: String,
            pull_bytes: Vec<u8>,
            publish: DisplayPublish<'_>,
        ) -> Result<(), &'static str> {
            late_result_bus::display::announce(hooks, id, pull_bytes, publish).await
        }

        pub fn body(id: &str) -> Result<Vec<u8>, &'static str> {
            late_result_bus::display::body(id)
        }

        pub async fn publish_one(publisher: &(impl ReadyPublisher + Sync), hooks: Option<&Hooks>) {
            late_result_bus::display::publish_one(publisher, hooks).await
        }
    }

    pub mod views {
        pub use crate::late_result_bus::views::{Publication, RecordingWriter, Writer};

        pub fn declaration() -> &'static cortexkit_bus_naming::ModuleBucket {
            crate::late_result_bus::views::declaration()
        }

        pub fn key(pin: &str) -> String {
            crate::late_result_bus::views::key(pin)
        }
    }
}

pub mod context {
    use crate::context::{self, AppContext};

    pub use context::maintenance_probe_fixture::MaintenanceProbeLock;
    pub use context::pending_capture_fixture::PendingCaptureBoundary;
    pub use context::status_count_fixture::STATUS_COUNT_PROBES;

    pub fn memory_estimate_walks_for_test() -> usize {
        context::memory_estimate_walks_for_test()
    }

    pub fn configure_bind_effects_for_test(
        root: &std::path::Path,
        session_id: &str,
    ) -> (usize, usize) {
        context::configure_bind_effects_for_test(root, session_id)
    }

    pub mod callgraph_test_support {
        pub use crate::context::callgraph_test_support::cold_build_fixture;
    }

    pub mod database_test_support {
        pub use crate::context::database_test_support::*;
    }

    pub mod pending_restore_work {
        pub use crate::context::pending_restore_work::{take_merged, with_legacy_restore};
    }

    pub fn install_pending_capture_hook_for_test(
        hook: impl FnMut(PendingCaptureBoundary) + 'static,
    ) -> impl Drop {
        context::pending_capture_fixture::install_pending_capture_hook_for_test(hook)
    }

    pub fn hold_maintenance_probe_lock_for_test(
        ctx: &AppContext,
        lock: MaintenanceProbeLock,
        while_held: impl FnOnce(),
    ) {
        ctx.hold_maintenance_probe_lock_for_test(lock, while_held);
    }
}

pub mod read_fence {
    pub fn denied_count() -> usize {
        crate::data_layer::read_fence::denied_count()
    }

    pub fn for_request(id: &str) -> bool {
        crate::data_layer::read_fence::for_request(id)
    }
}

pub use crate::data_layer::runtime::publication_fixture::PublicationFixture;
pub use crate::runtime_drain::watcher_fixture::watcher_context_with;

pub mod surfaces {
    use crate::context::AppContext;
    use crate::data_layer::{checkout::CheckoutView, surfaces};
    use crate::protocol::Response;
    use std::path::PathBuf;

    pub use surfaces::fixtures::{
        largest_meaning_fill_health, root_fixture_config, settled, write,
    };
    pub const READY: &str = surfaces::READY;
    pub const NOT_BOUND: &str = surfaces::reason::NOT_BOUND;

    pub fn bind_for_test(ctx: &AppContext) {
        surfaces::bind_for_test(ctx);
    }

    pub fn configure_meaning_without_fill(
        data: &crate::data_layer::runtime::DataCheckout,
        config: &crate::config::SemanticBackendConfig,
    ) {
        data.configure_meaning_without_fill_for_test(Some(config));
    }

    pub fn reserve_build(
        checkout: &crate::data_layer::checkout::Checkout,
    ) -> Option<impl FnOnce() -> Result<(), crate::data_layer::checkout::DataError> + '_> {
        let reserved = checkout.reserve_build()?;
        Some(move || checkout.build_reserved(false, reserved).map(|_| ()))
    }

    pub fn status_push(ctx: &AppContext) -> Option<crate::protocol::StatusPayload> {
        ctx.try_build_status_push_snapshot()
    }

    pub fn serve_text_index_for_test(ctx: &AppContext) {
        surfaces::serve_text_index_for_test(ctx);
    }

    pub fn disclose_parse_gaps(view: &CheckoutView, roots: &[PathBuf], response: &mut Response) {
        surfaces::disclose_parse_gaps(view, roots, response, |_| true);
    }

    #[cfg(target_os = "linux")]
    pub fn shared_memory_estimate(ctx: &AppContext) -> Option<crate::memory::MemoryEstimate> {
        surfaces::memory_estimate(ctx).map(|memory| memory.shared)
    }

    #[cfg(target_os = "linux")]
    pub fn memory_estimate_for_rollup(ctx: &AppContext) -> impl std::fmt::Debug {
        surfaces::memory_estimate_for_rollup(ctx)
    }
}

pub fn project_status(
    ctx: &crate::context::AppContext,
    snapshot: serde_json::Value,
) -> serde_json::Value {
    crate::status_push::project(ctx, snapshot)
}

#[cfg(target_os = "linux")]
pub mod memory_profile {
    use crate::data_layer::{checkout::DataLayer, runtime};
    use std::sync::Arc;
    use std::time::Instant;

    pub fn with_memory_walk_clock_for_test<T>(now: Instant, refresh: impl FnOnce() -> T) -> T {
        crate::context::with_memory_walk_clock_for_test(now, refresh)
    }

    pub fn memory_profile_layer_for_test() -> Arc<DataLayer> {
        runtime::memory_profile_layer_for_test()
    }

    pub fn memory_figure_walks_for_test() -> usize {
        runtime::memory_figure_walks_for_test()
    }

    pub fn with_memory_figure_baseline_for_test<T>(baseline: bool, run: impl FnOnce() -> T) -> T {
        runtime::with_memory_figure_baseline_for_test(baseline, run)
    }
}

pub mod outline {
    use crate::data_layer::{checkout, content, outline};
    use crate::parser::SharedSymbolCache;
    use aft_store::content::{OutlineSymbol, Surface};
    use std::path::Path;

    #[derive(Debug, PartialEq, Eq)]
    pub struct Prewarm(outline::Prewarm);

    impl Prewarm {
        pub fn cancelled() -> Self {
            Self(outline::Prewarm {
                stopped: true,
                ..Default::default()
            })
        }

        pub fn with_loaded(loaded: u64) -> Self {
            Self(outline::Prewarm {
                loaded,
                ..Default::default()
            })
        }

        pub fn cached(&self) -> u64 {
            self.0.cached
        }
        pub fn loaded(&self) -> u64 {
            self.0.loaded
        }
        pub fn left(&self) -> u64 {
            self.0.left
        }
        pub fn stopped(&self) -> bool {
            self.0.stopped
        }
    }

    pub fn prewarm(
        view: &checkout::CheckoutView,
        cache: &SharedSymbolCache,
        generation: u64,
        stop: &dyn Fn() -> bool,
    ) -> Result<Prewarm, checkout::DataError> {
        outline::prewarm(view, cache, generation, stop).map(Prewarm)
    }

    pub fn already_claimed_for_test(
        cache: &SharedSymbolCache,
        generation: u64,
        root: &Path,
    ) -> bool {
        outline::already_claimed_for_test(cache, generation, root)
    }

    pub fn complete_rust_outline(
        symbols: Vec<OutlineSymbol>,
        surface: &Surface,
    ) -> Vec<OutlineSymbol> {
        content::complete_rust_outline(symbols, surface)
    }

    #[cfg(unix)]
    pub fn record_reads() -> u64 {
        outline::record_reads()
    }

    #[cfg(unix)]
    pub mod tables {
        use crate::data_layer::outline::tables;

        pub fn without_sharing<T>(work: impl FnOnce() -> T) -> T {
            tables::without_sharing(work)
        }

        pub fn shared_bytes() -> Option<u64> {
            tables::shared().stats().map(|stats| stats.bytes)
        }
    }
}

pub mod callgraph_roots {
    use crate::data_layer::{callgraph_roots, query, trace_measure};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    pub fn normalize(path: &Path) -> PathBuf {
        callgraph_roots::normalize(path)
    }

    pub fn rust_program_roots(path: &Path) -> BTreeSet<PathBuf> {
        callgraph_roots::rust_program_roots(path)
    }

    pub fn memory_reader(root: &Path) -> query::SliceReader<impl query::SliceSource> {
        trace_measure::memory_reader(root)
    }
}

pub mod trace_budget {
    pub use crate::data_layer::callgraph_store_adapter::trace_budget_fixture::{
        impact_budget_fixture, impact_result_with_budget_for_test, IMPACT_BUDGET_NOTE_FOR_TEST,
    };
}

pub const NO_VECTORS: &str = crate::data_layer::parent::NO_VECTORS;

pub fn bind_foreground(
    root: &std::path::Path,
    storage: &std::path::Path,
    planes: crate::data_layer::checkout::Planes,
) -> Result<crate::data_layer::checkout::Checkout, crate::data_layer::checkout::DataError> {
    crate::data_layer::runtime::bind_foreground(root, storage, planes)
}
mod access;
pub use access::*;

pub mod data_layer {
    pub mod meaning {
        use std::path::Path;

        pub use super::super::meaning::fill_with_constant_vectors_for_test;

        pub fn hold_model_loads_for_test(root: &Path) -> impl Drop {
            crate::data_layer::meaning::hold_model_loads_for_test(root)
        }

        pub fn fail_model_loads_for_test(root: &Path, error: &str) -> impl Drop {
            crate::data_layer::meaning::fail_model_loads_for_test(root, error)
        }

        pub fn fill_without_model_for_test(root: &Path, dimensions: usize) -> impl Drop {
            crate::data_layer::meaning::fill_without_model_for_test(root, dimensions)
        }
    }

    pub mod runtime {
        pub fn hold_builds_for_test(root: &std::path::Path) -> impl Drop {
            crate::data_layer::runtime::hold_builds_for_test(root)
        }
    }

    pub mod surfaces {
        pub fn serve_text_only_for_test(ctx: &crate::context::AppContext) {
            crate::data_layer::surfaces::serve_text_only_for_test(ctx);
        }
    }

    pub mod text_scope {
        pub mod test_hooks {
            pub fn today_only() -> bool {
                crate::data_layer::text_scope::test_hooks::today_only()
            }
        }
    }

    pub mod content {
        pub fn parses_of_for_test(source: &[u8]) -> usize {
            crate::data_layer::content::parses_of_for_test(source)
        }
    }

    pub mod callgraph_store_adapter {
        use crate::callgraph_store::{CallGraphRead, StoreNode};
        use crate::data_layer::callgraph_store_adapter::{
            self, StoreAdapterResult, StoreIncompleteNote,
        };

        pub fn empty_receiver_note(
            store: &impl CallGraphRead,
            target: &StoreNode,
            count: usize,
            include_tests: bool,
        ) -> StoreAdapterResult<Option<StoreIncompleteNote>> {
            callgraph_store_adapter::empty_receiver_note(store, target, count, include_tests)
        }

        pub fn test_filter_note_for_count(count: usize, include_tests: bool) -> Option<String> {
            callgraph_store_adapter::test_filter_note_for_count(count, include_tests)
        }
    }
}

pub mod config_live {
    use std::sync::{Arc, Condvar, Mutex};

    pub fn staged_sources_empty_for_test() -> bool {
        crate::config_live::staged_sources_empty_for_test()
    }

    pub fn enable_config_watches_for_test() {
        crate::config_live::enable_config_watches_for_test();
    }

    pub fn with_config_watch_attach_gate<T>(
        gate: Arc<(Mutex<bool>, Condvar)>,
        run: impl FnOnce() -> T,
    ) -> T {
        crate::config_live::with_config_watch_attach_gate(gate, run)
    }
}

pub mod provisional_identity_retry {
    use std::time::Duration;

    pub fn with_timing_for_test<T>(
        first_delay: Duration,
        give_up_after: Duration,
        run: impl FnOnce() -> T,
    ) -> T {
        crate::provisional_identity_retry::with_timing_for_test(first_delay, give_up_after, run)
    }
}

pub mod runtime_drain {
    pub fn install_watcher_phase_commit_gate_for_test(
        target: std::path::PathBuf,
    ) -> (
        crossbeam_channel::Receiver<()>,
        crossbeam_channel::Sender<()>,
    ) {
        crate::runtime_drain::install_watcher_phase_commit_gate(target)
    }
}

pub mod database_open {
    pub use crate::database_open::recorded_open::{recorded_opens_for_test, RecordedOpen};
}
