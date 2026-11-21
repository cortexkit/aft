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

        pub fn key(pin: &str) -> String {
            crate::late_result_bus::views::key(pin)
        }
    }
}

pub mod context {
    use crate::context::{self, AppContext};

    pub use context::maintenance_probe_fixture::MaintenanceProbeLock;
    pub use context::pending_capture_fixture::PendingCaptureBoundary;

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

    pub fn serve_text_index_for_test(ctx: &AppContext) {
        surfaces::serve_text_index_for_test(ctx);
    }

    pub fn disclose_parse_gaps(view: &CheckoutView, roots: &[PathBuf], response: &mut Response) {
        surfaces::disclose_parse_gaps(view, roots, response);
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
