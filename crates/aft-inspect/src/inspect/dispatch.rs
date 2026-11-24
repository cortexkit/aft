use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Instant;

use crossbeam_channel::{unbounded, Receiver, Sender};

use super::job::{InspectCategory, InspectJob, InspectResult};

pub type InspectWorker = Arc<dyn Fn(InspectJob) -> InspectResult + Send + Sync + 'static>;

/// Submits one root's inspect jobs to the process-wide pool and collects their
/// results on that root's channel.
///
/// Jobs go straight from the submitting thread onto the pool's FIFO queue.
/// There is no per-root forwarding thread: a root that is idle, or kept
/// registered after idle eviction, costs no thread for inspect.
#[derive(Clone)]
pub struct InspectDispatcher {
    worker: InspectWorker,
    result_tx: Sender<InspectResult>,
    pub result_rx: Receiver<InspectResult>,
    pub pool: Arc<rayon::ThreadPool>,
}

impl InspectDispatcher {
    pub fn new(worker: InspectWorker) -> Self {
        let (result_tx, result_rx) = unbounded::<InspectResult>();
        Self {
            worker,
            result_tx,
            result_rx,
            pool: inspect_pool(),
        }
    }

    /// Queue `job` on the shared pool. Its result arrives on `result_rx`.
    pub fn submit(&self, job: InspectJob) {
        run_on_pool(
            &self.pool,
            job,
            self.result_tx.clone(),
            Arc::clone(&self.worker),
        );
    }
}

/// Number of live workers in the process-wide inspect pool. The pool's start
/// and exit handlers maintain this counter for platform-independent diagnostics.
static INSPECT_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The inspect pool is process-wide because roots are independently evictable
/// while their scans still share the daemon process. Inspect mixes file IO and
/// parsing; `min(8, available_parallelism)` is the measured point where more
/// workers stop improving the supported repositories while leaving cores for
/// interactive lanes.
///
/// `ColdBuildLimiter` remains separate: it admits concurrent heavy operations
/// (including Tier-2 scans), while this pool bounds the threads those admitted
/// operations can use. The pool is intentionally process-lifetime; dropping a
/// root's `InspectManager` only releases that root's handle to this pool.
static INSPECT_POOL: LazyLock<Arc<rayon::ThreadPool>> = LazyLock::new(|| {
    Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(default_pool_size())
            .thread_name(|index| format!("aft-rayon-inspect-{index}"))
            // Rayon defaults workers to ~2MB stacks (vs the main thread's 8MB).
            // The duplicates scanner walks the AST recursively, and deep trees
            // (minified bundles, generated code, long chains) previously
            // overflowed a 2MB worker stack and SIGABRT'd the whole bridge.
            // Match the main thread's 8MB so the bounded recursion in
            // collect_fragments (MAX_FRAGMENT_DEPTH) has comfortable headroom.
            .stack_size(8 * 1024 * 1024)
            .start_handler(|_| {
                INSPECT_THREAD_COUNT.fetch_add(1, Ordering::SeqCst);
            })
            .exit_handler(|_| {
                INSPECT_THREAD_COUNT.fetch_sub(1, Ordering::SeqCst);
            })
            .build()
            .expect("inspect worker pool must build"),
    )
});

fn inspect_pool() -> Arc<rayon::ThreadPool> {
    Arc::clone(&INSPECT_POOL)
}

#[doc(hidden)]
pub fn inspect_pool_size_for_test() -> usize {
    inspect_pool().current_num_threads()
}

#[doc(hidden)]
pub fn inspect_pool_thread_count_for_test() -> usize {
    INSPECT_THREAD_COUNT.load(Ordering::SeqCst)
}

pub fn default_worker() -> InspectWorker {
    Arc::new(dispatch_category)
}

fn run_on_pool(
    pool: &rayon::ThreadPool,
    job: InspectJob,
    tx: Sender<InspectResult>,
    worker: InspectWorker,
) {
    pool.spawn_fifo(move || {
        let started = Instant::now();
        let panic_job = job.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(job)))
            .unwrap_or_else(|panic| {
                InspectResult::failed(
                    &panic_job,
                    format!(
                        "inspect worker panicked before completion: {}",
                        panic_message(panic.as_ref())
                    ),
                    started.elapsed(),
                )
            });
        let scanner_ready = result.outcome.is_ok()
            && matches!(
                result.category,
                InspectCategory::Metrics | InspectCategory::Todos
            );
        let category = result.category;
        let _ = tx.send(result);
        if scanner_ready {
            signal_scanner_ready_for_test(category);
        }
    });
}

/// Test hook that signals successful scanner completion to subprocess fixtures.
/// This keeps cold scanner work outside timed language-server assertions.
fn signal_scanner_ready_for_test(category: InspectCategory) {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    let Some(directory) = DIRECTORY
        .get_or_init(|| std::env::var_os("AFT_TEST_INSPECT_SCANNER_READY_DIR").map(PathBuf::from))
    else {
        return;
    };
    let signal = directory.join(category.as_str());
    if let Err(error) = std::fs::write(&signal, b"complete\n") {
        log::warn!(
            "failed to signal completed inspect scanner at {}: {error}",
            signal.display()
        );
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload")
}

fn dispatch_category(job: InspectJob) -> InspectResult {
    use crate::inspect::scanners;

    match job.category {
        InspectCategory::Todos => scanners::todos::run_todos_scan(&job),
        InspectCategory::Metrics => scanners::metrics::run_metrics_scan(&job),
        InspectCategory::DeadCode => scanners::dead_code::run_dead_code_scan(&job),
        InspectCategory::UnusedExports => scanners::unused_exports::run_unused_exports_scan(&job),
        InspectCategory::Duplicates => scanners::duplicates::run_duplicates_scan(&job),
        InspectCategory::Cycles => scanners::cycles::run_cycles_scan(&job),
        InspectCategory::Complexity => scanners::complexity::run_complexity_scan(&job),
        InspectCategory::Diagnostics => {
            // Diagnostics are backed by the AppContext LSP manager and run via
            // the serial LSP/status lane in `handle_inspect` — never through
            // this rayon worker pool. Reaching this arm means a caller routed
            // Diagnostics into the worker path incorrectly; surface that as a
            // routing bug instead of a misleading "pending" status.
            let started = Instant::now();
            InspectResult::failed(
                &job,
                "diagnostics must run on the main thread (run_diagnostics_category), \
                 not the rayon inspect worker pool",
                started.elapsed(),
            )
        }
        other => {
            let started = Instant::now();
            InspectResult::failed(
                &job,
                format!("inspect category '{other}' is not active in v0.33"),
                started.elapsed(),
            )
        }
    }
}

fn default_pool_size() -> usize {
    // Dev-only override for reproducing thread-regime-dependent behaviour
    // (glibc allocates arenas per contending thread, so pool width changes
    // fragmentation). Not a tuning surface and deliberately undocumented.
    resolve_pool_size(std::env::var("AFT_INSPECT_POOL_THREADS").ok().as_deref())
}

/// Split from the env read so the parsing and clamping are testable without
/// mutating process-global state: `INSPECT_POOL` is a `LazyLock`, and any
/// concurrent test that builds an `InspectManager` would otherwise capture
/// whatever width the env happened to hold.
fn resolve_pool_size(override_value: Option<&str>) -> usize {
    if let Some(threads) = override_value.and_then(|value| value.parse::<usize>().ok()) {
        return threads.clamp(1, 512);
    }

    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .min(8)
}

#[cfg(test)]
mod tests {
    use super::resolve_pool_size;

    #[test]
    fn pool_thread_override_wins_and_clamps() {
        assert_eq!(resolve_pool_size(Some("17")), 17);
        assert_eq!(resolve_pool_size(Some("0")), 1);
        assert_eq!(resolve_pool_size(Some("100000")), 512);
    }

    #[test]
    fn pool_thread_override_ignores_absent_and_unparseable_values() {
        let derived = resolve_pool_size(None);

        assert_eq!(resolve_pool_size(Some("wide")), derived);
        assert_eq!(resolve_pool_size(Some("-4")), derived);
        assert_eq!(resolve_pool_size(Some("")), derived);
    }
}
