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
    run_subc_mode_with_registration(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        None,
    )
}

#[doc(hidden)]
pub fn run_subc_mode_with_registration(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    registration: Option<AdoptedRegistration>,
) -> Result<(), SubcError> {
    crate::commands::configure::register_runtime_hooks();
    perf_sweep::initialize();
    run_subc_mode_with_probe(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        None,
        registration,
    )
}

pub(super) fn run_subc_mode_with_probe(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    lifecycle_probe: Option<SubcTestLifecycleProbe>,
    registration: Option<AdoptedRegistration>,
) -> Result<(), SubcError> {
    // This entry point accepts only tools AFT advertises in its HELLO manifest.
    // A separate test entry point can also dispatch synthetic native commands.
    run_subc_mode_inner(
        connection_file_path,
        ctx,
        executor,
        dispatch,
        user_config_path,
        false,
        MAX_FRAME_BODY_LEN as usize,
        lifecycle_probe,
        registration,
    )
}

// A current-thread runtime polls its root future on the thread calling
// block_on. Windows gives the process main thread only a 1 MiB stack; the
// module loop and its callees need headroom even with their futures boxed.
pub(super) const MODULE_RUNTIME_STACK_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn with_module_runtime_stack<T: Send>(run: impl FnOnce() -> T + Send) -> io::Result<T> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("aft-subc-runtime".to_string())
            .stack_size(MODULE_RUNTIME_STACK_BYTES)
            .spawn_scoped(scope, run)?;
        match worker.join() {
            Ok(result) => Ok(result),
            // Keep the original panic payload and unwind the caller, rather
            // than turning a module panic into a clean stop or a new panic.
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

pub(super) fn run_subc_mode_inner(
    connection_file_path: &Path,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    dispatch: DispatchFn,
    user_config_path: Option<PathBuf>,
    allow_native_passthrough: bool,
    tool_response_body_limit: usize,
    lifecycle_probe: Option<SubcTestLifecycleProbe>,
    registration: Option<AdoptedRegistration>,
) -> Result<(), SubcError> {
    with_module_runtime_stack(move || {
        run_subc_mode_on_runtime_thread(
            connection_file_path,
            ctx,
            executor,
            dispatch,
            user_config_path,
            allow_native_passthrough,
            tool_response_body_limit,
            lifecycle_probe,
            registration,
        )
    })
    .map_err(SubcError::RuntimeThread)?
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
        false,
        standing_actor_override,
        watchdog_test,
    )
    .await
}
