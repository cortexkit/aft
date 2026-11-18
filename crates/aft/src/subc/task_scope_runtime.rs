use super::*;

/// Entry point for `aft --subc <connection-file>`. Synchronous on the outside;
/// owns an isolated current-thread tokio runtime on a dedicated, explicitly
/// sized stack for the async transport. The caller joins that thread.
/// Returns `Err` (fail-loud) on any connect/auth/protocol failure — we never
/// fall back to a standalone indexer: it could publish an independent index
/// while the daemon still serves the same checkout.
pub fn run_subc_mode(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
) -> Result<(), SubcError> {
    perf_sweep::initialize();
    run_subc_mode_with_probe(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        None,
    )
}

// Construct the large protocol future in a separate stack frame, then keep its
// state on the heap while polling. Native Windows main threads have a smaller
// default stack than Rust worker threads, and must still handle ordinary calls.
#[inline(never)]
pub(super) fn heap_module_future<F: std::future::Future>(
    build: impl FnOnce() -> F,
) -> std::pin::Pin<Box<F>> {
    Box::pin(build())
}

/// Test entry point for the module frame loop.
#[cfg(test)]
pub(super) async fn run_module_loop<R, W>(
    read: R,
    write: W,
    connection_file_path: &Path,
    shared_app: Arc<App>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    allow_native_passthrough: bool,
    tool_response_body_limit: usize,
    lifecycle_probe: Option<SubcTestLifecycleProbe>,
    storage_dir: &Path,
    connection_end_lsp: QuiescedLsp,
    runtime_metrics: Option<Arc<DispatchPathMetrics>>,
    standing_actor_override: Option<Arc<standing::StandingActor>>,
    watchdog_test: Option<stall_watchdog::FrameLoopTestHooks>,
) -> Result<ModuleLoopExit, SubcError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run_module_loop_with_task_scopes(
        read,
        write,
        connection_file_path,
        shared_app,
        executor,
        dispatch,
        user_config_path,
        allow_native_passthrough,
        tool_response_body_limit,
        lifecycle_probe,
        storage_dir,
        None,
        connection_end_lsp,
        runtime_metrics,
        standing_actor_override,
        watchdog_test,
    )
    .await
}
