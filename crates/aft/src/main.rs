mod cli;
use aft::bash_background::BgTaskRegistry;
use aft::config::Config;
use aft::context::{App, AppContext, SemanticIndexStatus};
use aft::log_ctx;
use aft::lsp::child_registry::LspChildRegistry;
use aft::protocol::{EchoParams, PushFrame, RawRequest, Response};
use aft::response_finalize::{DispatchOutcome, PendingResponses};
use aft::runtime_registry::RuntimeRegistry;
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

/// Parse `--subc <connection-file>` / `--subc=<path>` from argv. Returns `None`
/// when absent (standalone mode). The presence of the flag is the subc-mode
/// gate; the value is the daemon's published connection-file path.
fn parse_subc_arg(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            return args.next().map(std::path::PathBuf::from);
        }
        if let Some(raw) = arg.to_str().and_then(|a| a.strip_prefix("--subc=")) {
            if !raw.is_empty() {
                return Some(std::path::PathBuf::from(raw));
            }
        }
    }
    None
}

fn main() {
    // Confinement must be installed before global argument scanning, logging,
    // PATH discovery, or application threads. Target arguments such as
    // `--version` belong to the command after `--`, not to AFT itself.
    if std::env::args().nth(1).as_deref() == Some("sandbox-launch") {
        let args = std::env::args_os().skip(2).collect::<Vec<_>>();
        match cli::sandbox_launch::run(args) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(error.exit_code());
            }
        }
    }

    // Handle --version before starting the normal application.
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("aft {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if std::env::args().nth(1).as_deref() == Some("migrate-storage") {
        let args = std::env::args_os().skip(2).collect::<Vec<_>>();
        match aft::migrate_storage::parse_cli_args(args) {
            Ok(args) => {
                let status = aft::migrate_storage::run_with_options(
                    args,
                    aft::migrate_storage::Options::default(),
                );
                std::process::exit(i32::from(status.code()));
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(2);
            }
        }
    }

    // Daemon launches can miss user shell PATH entries. Initialize before any
    // AFT threads or executors start so all subprocesses inherit one PATH.
    aft::effective_path::initialize_process_path();

    aft::logging::init();

    if std::env::args().nth(1).as_deref() == Some("warmup") {
        let args = std::env::args_os().skip(2).collect::<Vec<_>>();
        match cli::warmup::run(args) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(error.exit_code());
            }
        }
    }

    // subc daemon attach: `aft --subc <connection-file>` swaps the
    // standalone NDJSON-over-stdin transport for the subc loopback-TCP client.
    // Presence of --subc => subc mode; absence => standalone (the dormancy gate).
    // Fail-loud on connect/auth failure — never silently downgrade to standalone
    // (split-brain index state). tokio runs ONLY inside run_subc_mode.
    if let Some(connection_file) = parse_subc_arg(std::env::args_os().skip(1)) {
        aft::slog_info!("subc mode, pid {}", std::process::id());
        // A single AppContext serves the attached routes (N=1); subc tool calls
        // are routed through the per-actor executor once the first route binds.
        let app = App::default_shared();
        let ctx = Arc::new(AppContext::from_app(Arc::clone(&app), Config::default()));
        let executor = Arc::new(aft::executor::Executor::new());
        install_subc_signal_handler(
            Arc::clone(&executor),
            Arc::clone(&ctx),
            app.lsp_child_registry(),
        );
        // Resolve AFT's own CortexKit config home once at the boundary; it is
        // threaded into the per-bind tier composition (W5). Under the gateway
        // there is no plugin to relay on-disk config, so this is how a gateway
        // user's aft.jsonc reaches AFT.
        let user_config_path = aft::subc_config::cortexkit_user_config_path();
        match aft::subc::run_subc_mode(&connection_file, ctx, executor, dispatch, user_config_path)
        {
            Ok(()) => return,
            Err(error) => {
                aft::slog_error!("subc attach failed: {error}");
                std::process::exit(1);
            }
        }
    }

    aft::slog_info!("started, pid {}", std::process::id());

    let app = App::default_shared();
    let ctx = AppContext::from_app(Arc::clone(&app), Config::default());
    let registry = RuntimeRegistry::standalone(app, ctx);
    // P3-02 slice 2: signal handling aggregates all per-actor
    // background registries; drain/dispatch multi-root routing remains later.
    {
        let bg_registries = signal_bg_registries(&registry);
        let lsp_children = registry.app().lsp_child_registry();
        install_signal_handler(bg_registries, lsp_children);
    }

    // Install bash output-compression closures per actor so each background
    // registry captures its own filter registry and compress flag.
    // Future P5 actor creation at attach must call this for each new actor.
    for runtime in registry.iter() {
        install_bash_compressor(runtime);
    }

    // P3-02: stdout/progress is a process service — N>1 must keep one
    // shared sink so response and push frames do not interleave/corrupt.
    let stdout_writer = registry.current().stdout_writer();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let shutdown_from_push = Arc::clone(&shutdown_requested);
    registry
        .current()
        .set_progress_sender(Some(Arc::new(Box::new(move |frame: PushFrame| {
            let Ok(mut writer) = stdout_writer.lock() else {
                aft::slog_error!("stdout push frame lock poisoned; shutting down bridge");
                shutdown_from_push.store(true, Ordering::SeqCst);
                return;
            };
            write_push_frame_or_request_shutdown(&mut *writer, &frame, &shutdown_from_push);
        }))));

    // Stdin is read by a dedicated thread that forwards lines through a
    // channel. The main thread does recv_timeout so it wakes periodically
    // even when no agent traffic is arriving — that periodic wake runs
    // the drain_* functions so background-build channel events (e.g.
    // SemanticIndexEvent::Ready) get processed and their status_changed
    // push frames emitted. Without the wake, the sidebar can stay stuck
    // on "loading" indefinitely until the next request happens to arrive.
    const DRAIN_INTERVAL: Duration = Duration::from_millis(250);
    const PENDING_POLL_INTERVAL: Duration = Duration::from_millis(100);
    let mut pending = PendingResponses::default();
    // Opportunistic macOS allocator relief: rate-limit stamp for the slack
    // check that runs on the periodic drain wake (threshold + spacing live in
    // memory.rs so subc and standalone share one policy).
    #[cfg(target_os = "macos")]
    let mut last_slack_relief: Option<std::time::Instant> = None;
    let (line_tx, line_rx) = mpsc::channel::<io::Result<String>>();
    let mut graceful_stdin_shutdown = false;
    thread::spawn(move || {
        let stdin = io::stdin();
        let reader = stdin.lock();
        for line_result in reader.lines() {
            if line_tx.send(line_result).is_err() {
                break;
            }
        }
    });

    loop {
        if shutdown_requested.load(Ordering::SeqCst) {
            break;
        }

        let recv_timeout = if pending.is_empty() {
            DRAIN_INTERVAL
        } else {
            PENDING_POLL_INTERVAL
        };
        let line_result = match line_rx.recv_timeout(recv_timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic drain so push frames flow even without requests.
                // Cheap on the idle path: each drain just checks try_recv
                // on a channel and bails if empty.
                if let Err(e) = drain_runtime_events_and_write_pending(&registry, &mut pending) {
                    aft::slog_error!("stdout write error: {}", e);
                    break;
                }
                #[cfg(target_os = "macos")]
                {
                    let now = std::time::Instant::now();
                    if aft::memory::spawn_allocator_slack_relief_if_due(last_slack_relief, now) {
                        last_slack_relief = Some(now);
                    }
                }
                if shutdown_requested.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                graceful_stdin_shutdown = true;
                break;
            }
        };

        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                aft::slog_error!("stdin read error: {}", e);
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut shutdown_after_response = false;
        let response = match serde_json::from_str::<RawRequest>(trimmed) {
            Ok(req) => {
                // Drain search index FIRST so watcher events apply to the latest index.
                // If reversed, watcher updates applied to the old index would be lost
                // when the background-built index replaces it.
                aft::logging::note_drain_slice();
                if let Err(e) = drain_runtime_events_and_write_pending(&registry, &mut pending) {
                    aft::slog_error!("stdout write error: {}", e);
                    break;
                }
                let request_id = req.id.clone();
                let session_id = req.session().to_string();
                let command = req.command.clone();
                let attach_command = if req.command == "tool_call" {
                    req.params
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("tool_call")
                        .to_string()
                } else {
                    req.command.clone()
                };
                let session_id_for_log = req.session_id.clone();
                // P3-02/P3-03 seam: request-root identity is absent in
                // standalone; the selected single runtime is the root today.
                // P3-03 adds an explicit root selector here instead of path inference.
                let runtime = registry.current();
                let dispatch_result = catch_unwind(AssertUnwindSafe(|| {
                    log_ctx::with_session(session_id_for_log, || dispatch_outcome(req, runtime))
                }));
                match dispatch_result {
                    Ok(DispatchOutcome::Immediate(mut response)) => {
                        aft::response_finalize::finalize_response(
                            &mut response,
                            runtime,
                            &session_id,
                            &attach_command,
                        );
                        Some(response)
                    }
                    Ok(DispatchOutcome::Deferred(pending_response)) => {
                        pending.register(pending_response);
                        None
                    }
                    Err(payload) => {
                        shutdown_after_response = true;
                        Some(dispatch_panic_response(
                            request_id,
                            &command,
                            payload.as_ref(),
                        ))
                    }
                }
            }
            Err(e) => {
                aft::slog_error!("parse error: {} — input: {}", e, trimmed);
                Some(Response::error(
                    "_parse_error",
                    "parse_error",
                    format!("failed to parse request: {}", e),
                ))
            }
        };

        if let Some(response) = response {
            if let Err(e) = write_response(registry.current(), &response) {
                aft::slog_error!("stdout write error: {}", e);
                break;
            }
        }
        drain_configure_warning_events_for_registry(&registry);
        if shutdown_after_response || shutdown_requested.load(Ordering::SeqCst) {
            break;
        }
    }

    pending.drain_on_shutdown();
    if graceful_stdin_shutdown {
        // Only the natural stdin-EOF path flushes owner-side index deltas and
        // queued callgraph refreshes. Signal, stdout-error, and panic teardown
        // skip disk work so abrupt exits stay fast and avoid lock contention.
        flush_indexes_on_graceful_shutdown(&registry);
    }
    for runtime in registry.iter() {
        runtime.lsp().shutdown_all();
        runtime.bash_background().detach();
    }
    aft::artifact_owner::shutdown_heartbeat_thread();
    aft::slog_info!("stdin closed, shutting down");
}

fn drain_runtime_events(registry: &RuntimeRegistry) {
    for runtime in registry.iter() {
        aft::runtime_drain::drain_deferred_configure_maintenance(runtime);
        aft::runtime_drain::drain_configure_warning_events(runtime);
        aft::runtime_drain::drain_search_index_events(runtime);
        aft::runtime_drain::drain_callgraph_store_events(runtime);
        aft::runtime_drain::drain_semantic_index_events(runtime);
        aft::runtime_drain::drain_semantic_refresh_events(runtime);
        aft::runtime_drain::drain_inspect_events(runtime);
        aft::runtime_drain::drain_watcher_events(runtime);
        aft::runtime_drain::drain_lsp_events(runtime);
    }
    aft::logging::perf_tick(None);
}

#[cfg(test)]
fn callgraph_refresh_worker_test_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn flush_indexes_on_graceful_shutdown(registry: &RuntimeRegistry) {
    for runtime in registry.iter() {
        let _ = runtime.flush_search_index_on_graceful_shutdown();
    }
    // Tests run in parallel, but callgraph refreshes use one process-wide
    // worker. Without this lock, one test's graceful shutdown can reject
    // another test's pending refresh.
    #[cfg(test)]
    let _callgraph_refresh_worker_guard = callgraph_refresh_worker_test_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = aft::callgraph_store::flush_callgraph_store_refreshes_on_graceful_shutdown();
}

fn drain_runtime_events_and_write_pending(
    registry: &RuntimeRegistry,
    pending: &mut PendingResponses,
) -> io::Result<usize> {
    let ctx = registry.current();
    let mut written = write_ready_pending(ctx, pending)?;
    drain_runtime_events(registry);
    written += write_ready_pending(ctx, pending)?;
    Ok(written)
}

fn write_ready_pending(ctx: &AppContext, pending: &mut PendingResponses) -> io::Result<usize> {
    finalize_ready_pending(ctx, pending, |response| write_response(ctx, response))
}

#[cfg(test)]
fn drain_runtime_events_and_write_pending_to_writer(
    registry: &RuntimeRegistry,
    pending: &mut PendingResponses,
    writer: &mut impl Write,
) -> io::Result<usize> {
    let ctx = registry.current();
    let mut written = write_ready_pending_to_writer(ctx, pending, writer)?;
    drain_runtime_events(registry);
    written += write_ready_pending_to_writer(ctx, pending, writer)?;
    Ok(written)
}

#[cfg(test)]
fn write_ready_pending_to_writer(
    ctx: &AppContext,
    pending: &mut PendingResponses,
    writer: &mut impl Write,
) -> io::Result<usize> {
    finalize_ready_pending(ctx, pending, |response| {
        write_response_to_writer(writer, response)
    })
}

fn finalize_ready_pending(
    ctx: &AppContext,
    pending: &mut PendingResponses,
    mut write: impl FnMut(&Response) -> io::Result<()>,
) -> io::Result<usize> {
    let ready = pending.poll_ready(ctx);
    let count = ready.len();
    for mut resolved in ready {
        aft::response_finalize::finalize_response(
            &mut resolved.response,
            ctx,
            &resolved.session_id,
            &resolved.attach_command,
        );
        write(&resolved.response)?;
    }
    Ok(count)
}

fn drain_configure_warning_events_for_registry(registry: &RuntimeRegistry) {
    for runtime in registry.iter() {
        aft::runtime_drain::drain_deferred_configure_maintenance(runtime);
        aft::runtime_drain::drain_configure_warning_events(runtime);
    }
}

fn signal_bg_registries(registry: &RuntimeRegistry) -> Vec<BgTaskRegistry> {
    registry
        .iter()
        .map(|runtime| runtime.bash_background().clone())
        .collect()
}

fn install_bash_compressor(runtime: &AppContext) {
    // Install bash output-compression closure on the BgTaskRegistry. The
    // closure captures the shared filter-registry handle and the shared
    // compress-flag (atomic) so the watchdog thread can compress without
    // touching the rest of AppContext. The flag is updated from `configure`
    // when `experimental.bash.compress` changes; the filter registry is
    // updated when `reset_filter_registry` is called.
    let filter_registry_handle = runtime.shared_filter_registry();
    let compress_flag = runtime.bash_compress_flag();
    runtime.bash_background().set_compressor_with_exit_code(
        move |command: &str, output: String, exit_code: Option<i32>| {
            if !compress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return aft::compress::CompressionResult::new(output);
            }
            let registry_guard = match filter_registry_handle.read() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            aft::compress::compress_with_registry_exit_code(
                command,
                &output,
                exit_code,
                &registry_guard,
            )
        },
    );
}

#[cfg(unix)]
fn install_signal_handler(bg_registries: Vec<BgTaskRegistry>, lsp_children: LspChildRegistry) {
    let signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ]);
    let Ok(mut signals) = signals else {
        if let Err(error) = signals {
            aft::slog_error!("failed to install signal handlers: {error}");
        }
        return;
    };

    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            // Plugin restarts can SIGTERM the bridge while background bash jobs
            // are still running. Detach first so child handles are not killed by
            // Rust drop glue and can be rehydrated from disk.
            for registry in &bg_registries {
                registry.detach();
            }
            aft::artifact_owner::shutdown_heartbeat_thread();
            // Kill LSP children synchronously before exit. Without this, LSP
            // child processes (typescript-language-server, biome lsp-proxy,
            // etc.) get orphaned to PID 1 because process::exit bypasses the
            // graceful shutdown path that LspManager::shutdown_all uses on
            // the natural stdin-closed exit. Graceful shutdown takes up to
            // 5s per server (shutdown request + exit notification + poll),
            // which is too slow for a signal handler — we SIGKILL instead.
            let killed = lsp_children.kill_all();
            if killed > 0 {
                aft::slog_info!("signal {}: killed {} LSP child process(es)", signal, killed);
            }
            std::process::exit(128 + signal);
        }
    });
}

#[cfg(unix)]
fn install_subc_signal_handler(
    executor: Arc<aft::executor::Executor>,
    initial_ctx: Arc<AppContext>,
    lsp_children: LspChildRegistry,
) {
    let signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ]);
    let Ok(mut signals) = signals else {
        if let Err(error) = signals {
            aft::slog_error!("failed to install subc signal handlers: {error}");
        }
        return;
    };

    std::thread::spawn(move || {
        if let Some(signal) = signals.forever().next() {
            // When this process is shutting down during a subc drain, it may
            // receive SIGTERM before the async cleanup code runs. Detach each
            // live actor's background registry now so any running bash
            // subprocesses keep their task metadata and can be recovered after
            // the process restarts.
            let mut contexts = executor.actor_contexts();
            if !contexts.iter().any(|ctx| Arc::ptr_eq(ctx, &initial_ctx)) {
                contexts.push(Arc::clone(&initial_ctx));
            }
            for actor_ctx in &contexts {
                actor_ctx.bash_background().detach();
            }
            aft::artifact_owner::shutdown_heartbeat_thread();
            let killed = lsp_children.kill_all();
            if killed > 0 {
                aft::slog_info!(
                    "subc signal {}: killed {} LSP child process(es)",
                    signal,
                    killed
                );
            }
            std::process::exit(128 + signal);
        }
    });
}

#[cfg(not(unix))]
fn install_subc_signal_handler(
    _executor: Arc<aft::executor::Executor>,
    initial_ctx: Arc<AppContext>,
    lsp_children: LspChildRegistry,
) {
    install_signal_handler(vec![initial_ctx.bash_background().clone()], lsp_children);
}

#[cfg(not(unix))]
static WINDOWS_SIGNAL_REGISTRIES: std::sync::OnceLock<(Vec<BgTaskRegistry>, LspChildRegistry)> =
    std::sync::OnceLock::new();

#[cfg(windows)]
unsafe extern "system" fn windows_console_handler(ctrl_type: u32) -> i32 {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    if matches!(
        ctrl_type,
        CTRL_C_EVENT
            | CTRL_BREAK_EVENT
            | CTRL_CLOSE_EVENT
            | CTRL_LOGOFF_EVENT
            | CTRL_SHUTDOWN_EVENT
    ) {
        if let Some((bg_registries, lsp_children)) = WINDOWS_SIGNAL_REGISTRIES.get() {
            for registry in bg_registries {
                registry.detach();
            }
            aft::artifact_owner::shutdown_heartbeat_thread();
            let killed = lsp_children.kill_all();
            if killed > 0 {
                aft::slog_info!(
                    "windows console event {ctrl_type}: killed {killed} LSP child process(es)"
                );
            }
        }
        1
    } else {
        0
    }
}

#[cfg(windows)]
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(not(unix))]
fn install_signal_handler(bg_registries: Vec<BgTaskRegistry>, lsp_children: LspChildRegistry) {
    #[cfg(windows)]
    {
        let _ = WINDOWS_SIGNAL_REGISTRIES.set((bg_registries, lsp_children));
        // SAFETY: registers a process-global console-control callback. The
        // callback only uses cloneable registries stored in OnceLock.
        let ok = unsafe { SetConsoleCtrlHandler(Some(windows_console_handler), 1) };
        if ok == 0 {
            aft::slog_error!("failed to install Windows console control handler");
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (bg_registries, lsp_children);
    }
}

fn write_push_frame_or_request_shutdown(
    writer: &mut impl Write,
    frame: &PushFrame,
    shutdown_requested: &AtomicBool,
) {
    if let Err(error) = write_push_frame(writer, frame) {
        aft::slog_error!(
            "stdout push frame write error: {}; shutting down bridge",
            error
        );
        shutdown_requested.store(true, Ordering::SeqCst);
    }
}

fn dispatch_panic_response(
    request_id: impl Into<String>,
    command: &str,
    payload: &(dyn std::any::Any + Send),
) -> Response {
    let panic_message = panic_payload_message(payload);
    aft::slog_error!(
        "command '{}' panicked: {}; shutting down bridge",
        command,
        panic_message
    );
    Response::error(
        request_id,
        "internal_error",
        format!("command '{command}' panicked: {panic_message}"),
    )
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    aft::commands::tool_call::register_dispatch(dispatch);
    match req.command.as_str() {
        "ping" => Response::success(&req.id, serde_json::json!({ "command": "pong" })),
        "version" => Response::success(
            &req.id,
            serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
        ),
        "echo" => handle_echo(&req),
        "tool_call" => aft::commands::tool_call::handle(&req, ctx),
        "bash" => aft::commands::bash::handle(&req, ctx),
        "bash_drain_completions" => aft::commands::bash_drain_completions::handle(&req, ctx),
        "bash_ack_completions" => aft::commands::bash_drain_completions::handle_ack(&req, ctx),
        "bash_status" => aft::commands::bash_status::handle(&req, ctx),
        "bash_notify" => aft::commands::bash_notify::handle(&req, ctx),
        "bash_unnotify" => aft::commands::bash_notify::handle_unnotify(&req, ctx),
        "bash_promote" => aft::commands::bash_promote::handle(&req, ctx),
        "bash_wait_detach" => aft::commands::bash_wait_detach::handle(&req, ctx),
        "bash_regex_match" => aft::commands::bash_regex_match::handle(&req),
        "bash_kill" => aft::commands::bash_kill::handle(&req, ctx),
        "bash_write" => aft::commands::bash_write::handle(&req, ctx),
        "db_get_state" => aft::commands::state::handle_db_get_state(&req, ctx),
        "db_set_state" => aft::commands::state::handle_db_set_state(&req, ctx),
        "db_get_host_state" => aft::commands::state::handle_db_get_host_state(&req, ctx),
        "db_set_host_state" => aft::commands::state::handle_db_set_host_state(&req, ctx),
        "outline" => aft::commands::outline::handle_outline(&req, ctx),
        "zoom" => aft::commands::zoom::handle_zoom(&req, ctx),
        "read" => aft::commands::read::handle_read(&req, ctx),
        "undo" | "undo_preview" | "edit_history" | "checkpoint" | "checkpoint_paths"
        | "restore_checkpoint" | "list_checkpoints"
            if ctx.config().backup.enabled == Some(false) =>
        {
            Response::error(
                &req.id,
                "backups_disabled",
                "backups disabled by user config",
            )
        }
        "undo" => aft::commands::undo::handle_undo(&req, ctx),
        "undo_preview" => aft::commands::undo::handle_undo_preview(&req, ctx),
        "edit_history" => aft::commands::edit_history::handle_edit_history(&req, ctx),
        "checkpoint" => aft::commands::checkpoint::handle_checkpoint(&req, ctx),
        "checkpoint_paths" => aft::commands::checkpoint::handle_checkpoint_paths(&req, ctx),
        "restore_checkpoint" => {
            aft::commands::restore_checkpoint::handle_restore_checkpoint(&req, ctx)
        }
        "list_checkpoints" => aft::commands::list_checkpoints::handle_list_checkpoints(&req, ctx),
        "apply_patch" => aft::commands::apply_patch::handle_apply_patch(&req, ctx),
        "write" => aft::commands::write::handle_write(&req, ctx),
        "delete_file" => aft::commands::delete_file::handle_delete_file(&req, ctx),
        "move_file" => aft::commands::move_file::handle_move_file(&req, ctx),
        "edit_symbol" => aft::commands::edit_symbol::handle_edit_symbol(&req, ctx),
        "edit_match" => aft::commands::edit_match::handle_edit_match(&req, ctx),
        "batch" => aft::commands::batch::handle_batch(&req, ctx),
        "add_import" => aft::commands::add_import::handle_add_import(&req, ctx),
        "remove_import" => aft::commands::remove_import::handle_remove_import(&req, ctx),
        "organize_imports" => aft::commands::organize_imports::handle_organize_imports(&req, ctx),
        "configure" => aft::commands::configure::handle_configure(&req, ctx),
        "glob" => aft::commands::glob::handle_glob(&req, ctx),
        "grep" => aft::commands::grep::handle_grep(&req, ctx),
        "semantic_search" => {
            if let Some(response) = wait_for_semantic_index_before_search(&req, ctx) {
                response
            } else {
                aft::commands::semantic_search::handle_semantic_search(&req, ctx)
            }
        }
        "status" => aft::commands::status::handle_status(&req, ctx),
        "list_filters" => aft::commands::list_filters::handle_list_filters(&req, ctx),
        "trust_filter_project" => {
            aft::commands::trust_filter_project::handle_trust_filter_project(&req, ctx)
        }
        "untrust_filter_project" => {
            aft::commands::untrust_filter_project::handle_untrust_filter_project(&req, ctx)
        }
        "call_tree" => aft::commands::call_tree::handle_call_tree(&req, ctx),
        "callers" => aft::commands::callers::handle_callers(&req, ctx),
        "trace_to" => aft::commands::trace_to::handle_trace_to(&req, ctx),
        "trace_to_symbol" => aft::commands::trace_to_symbol::handle_trace_to_symbol(&req, ctx),
        "impact" => aft::commands::impact::handle_impact(&req, ctx),
        "trace_data" => aft::commands::trace_data::handle_trace_data(&req, ctx),
        "move_symbol" => aft::commands::move_symbol::handle_move_symbol(&req, ctx),
        "extract_function" => aft::commands::extract_function::handle_extract_function(&req, ctx),
        "inline_symbol" => aft::commands::inline_symbol::handle_inline_symbol(&req, ctx),
        "inspect" => aft::commands::inspect::handle_inspect(&req, ctx),
        "inspect_tier2_run" => aft::commands::inspect::handle_inspect_tier2_run(&req, ctx),
        "git_conflicts" => aft::commands::conflicts::handle_git_conflicts(ctx, &req),
        "ast_search" => aft::commands::ast_search::handle_ast_search(&req, ctx),
        "ast_replace" => aft::commands::ast_replace::handle_ast_replace(&req, ctx),
        "lsp_diagnostics" => aft::commands::lsp_diagnostics::handle_lsp_diagnostics(&req, ctx),
        "lsp_inspect" => aft::commands::lsp_inspect::handle_lsp_inspect(&req, ctx),
        "lsp_hover" => aft::commands::lsp_hover::handle_lsp_hover(&req, ctx),
        "lsp_goto_definition" => {
            aft::commands::lsp_goto_definition::handle_lsp_goto_definition(&req, ctx)
        }
        "lsp_find_references" => {
            aft::commands::lsp_find_references::handle_lsp_find_references(&req, ctx)
        }
        "lsp_prepare_rename" => {
            aft::commands::lsp_prepare_rename::handle_lsp_prepare_rename(&req, ctx)
        }
        "lsp_rename" => aft::commands::lsp_rename::handle_lsp_rename(&req, ctx),
        // NOTE: "snapshot" must remain in the production binary because integration tests in
        // crates/aft/tests/integration/ spawn the compiled binary as a subprocess and send
        // "snapshot" commands through the stdin/stdout protocol. A #[cfg(test)] gate would
        // only affect unit-test compilation and would not exclude this arm from the binary
        // that integration tests execute. See: crates/aft/tests/integration/safety_test.rs
        "snapshot" => handle_snapshot(&req, ctx),
        _ => {
            aft::slog_warn!("unknown command: {}", req.command);
            Response::error(
                &req.id,
                "unknown_command",
                format!("unknown command: {}", req.command),
            )
        }
    }
}

fn dispatch_outcome(req: RawRequest, ctx: &AppContext) -> DispatchOutcome {
    aft::commands::tool_call::register_dispatch(dispatch);
    if req.command == "bash"
        && aft::commands::bash_orchestrate::foreground_orchestrate_enabled(&req)
    {
        let spawn_response = aft::commands::bash::handle(&req, ctx);
        return aft::commands::bash_orchestrate::build_bash_outcome(&req, ctx, spawn_response);
    }
    DispatchOutcome::Immediate(dispatch(req, ctx))
}

fn handle_echo(req: &RawRequest) -> Response {
    match serde_json::from_value::<EchoParams>(req.params.clone()) {
        Ok(params) => Response::success(&req.id, serde_json::json!({ "message": params.message })),
        Err(e) => Response::error(
            &req.id,
            "invalid_request",
            format!("echo: invalid params: {}", e),
        ),
    }
}

/// Test-only command: snapshot a file into the backup store.
///
/// Params: `file` (string, required) — path to snapshot.
/// Returns: `{ backup_id }`.
fn wait_for_semantic_index_before_search(req: &RawRequest, ctx: &AppContext) -> Option<Response> {
    if std::env::var_os("AFT_WAIT_FOR_SEMANTIC_READY").is_none() || !ctx.config().semantic_search {
        return None;
    }

    let timeout_ms = std::env::var("AFT_WAIT_FOR_SEMANTIC_READY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(600_000);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        aft::runtime_drain::drain_search_index_events(ctx);
        aft::runtime_drain::drain_semantic_index_events(ctx);

        match ctx
            .semantic_index_status()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            SemanticIndexStatus::Ready { .. }
            | SemanticIndexStatus::Disabled
            | SemanticIndexStatus::Failed(_) => return None,
            SemanticIndexStatus::Building { stage, .. } => {
                if Instant::now() >= deadline {
                    return Some(Response::error(
                        &req.id,
                        "semantic_index_timeout",
                        format!(
                            "semantic index did not become ready before semantic_search within {timeout_ms}ms (stage: {stage})"
                        ),
                    ));
                }
            }
        }

        thread::sleep(Duration::from_millis(250));
    }
}

fn handle_snapshot(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "snapshot: missing required param 'file'",
            );
        }
    };

    let path = match ctx.validate_path(&req.id, std::path::Path::new(file)) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let path = path.as_path();
    let mut backup = ctx.backup().lock();

    match backup.snapshot(req.session(), path, "manual snapshot") {
        Ok(id) => Response::success(&req.id, serde_json::json!({ "backup_id": id })),
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn write_response(ctx: &AppContext, response: &Response) -> io::Result<()> {
    let stdout_writer = ctx.stdout_writer();
    let mut writer = stdout_writer
        .lock()
        .map_err(|_| io::Error::other("stdout writer lock poisoned"))?;
    write_response_to_writer(&mut *writer, response)
}

fn write_response_to_writer(writer: &mut impl Write, response: &Response) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_push_frame(writer: &mut impl Write, frame: &PushFrame) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, frame)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
fn global_gitignore_env_test_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
mod pending_response_tests {
    use super::{drain_runtime_events_and_write_pending_to_writer, write_ready_pending_to_writer};
    use aft::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use aft::bash_background::registry::BgTaskSnapshot;
    use aft::bash_background::BgTaskStatus;
    use aft::config::Config;
    use aft::context::{App, AppContext};
    use aft::parser::TreeSitterProvider;
    use aft::protocol::{ConfigureWarningsFrame, PushFrame, RawRequest, Response};
    use aft::response_finalize::{
        attach_bg_completions, attach_status_bar, finalize_response, PendingResponse,
        PendingResponses,
    };
    use aft::runtime_registry::RuntimeRegistry;
    use std::cell::Cell;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    struct FinalizeFixture {
        _root: TempDir,
        _storage: TempDir,
        ctx: AppContext,
    }

    fn make_ctx_with_root(root: &Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    fn make_runtime_registry(root: &Path) -> RuntimeRegistry {
        let app = App::default_shared();
        let ctx = AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            },
        );
        RuntimeRegistry::standalone(app, ctx)
    }

    fn install_pending_completion(
        ctx: &AppContext,
        storage: &Path,
        root: &Path,
        session_id: &str,
        task_id: &str,
    ) {
        let paths = task_paths(storage, session_id, task_id).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            session_id.to_string(),
            "echo done".to_string(),
            root.to_path_buf(),
            Some(root.to_path_buf()),
            None,
            true,
            true,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task(&paths.json, &metadata).unwrap();
        std::fs::write(&paths.stdout, "done\n").unwrap();
        ctx.bash_background()
            .replay_session_for_project(storage, session_id, root)
            .unwrap();
    }

    fn fixture_with_completion_and_status(session_id: &str, task_id: &str) -> FinalizeFixture {
        let root = TempDir::new().unwrap();
        let storage = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(root.path());
        install_pending_completion(&ctx, storage.path(), root.path(), session_id, task_id);
        ctx.update_status_bar_tier2(Some(3), Some(4), Some(5), Some(6), false);
        FinalizeFixture {
            _root: root,
            _storage: storage,
            ctx,
        }
    }

    fn line_values(writer: &[u8]) -> Vec<serde_json::Value> {
        let output = std::str::from_utf8(writer).unwrap();
        output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[cfg(unix)]
    const WAIT_DETACH_COMMAND: &str = "sleep 1.5 && echo detached";
    #[cfg(windows)]
    const WAIT_DETACH_COMMAND: &str = "Start-Sleep -Milliseconds 1500; Write-Output detached";

    fn wait_for_snapshot(
        ctx: &AppContext,
        root: &Path,
        storage: &Path,
        session_id: &str,
        task_id: &str,
        predicate: impl Fn(&BgTaskSnapshot) -> bool,
    ) -> BgTaskSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(snapshot) =
                ctx.bash_background()
                    .status(task_id, session_id, Some(root), Some(storage), 1024)
            {
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for snapshot for {task_id}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn finalize_response_matches_inline_finalizers() {
        let session_id = "session-finalize";
        let inline_fixture =
            fixture_with_completion_and_status(session_id, "bash-0000000000000501");
        let mut inline = Response::success("req-finalize", serde_json::json!({"value": true}));
        attach_bg_completions(&mut inline, &inline_fixture.ctx, session_id, "read");
        attach_status_bar(&mut inline, &inline_fixture.ctx, "read");

        let helper_fixture =
            fixture_with_completion_and_status(session_id, "bash-0000000000000501");
        let mut helper = Response::success("req-finalize", serde_json::json!({"value": true}));
        finalize_response(&mut helper, &helper_fixture.ctx, session_id, "read");

        assert_eq!(
            serde_json::to_value(&helper).unwrap(),
            serde_json::to_value(&inline).unwrap()
        );
        assert_eq!(
            helper.data["bg_completions"][0]["task_id"],
            "bash-0000000000000501"
        );
        assert_eq!(helper.data["status_bar"]["dead_code"], 3);
    }

    #[test]
    fn pending_registry_writes_once_after_resolution_and_finalizes() {
        let session_id = "session-registry";
        let fixture = fixture_with_completion_and_status(session_id, "bash-0000000000000502");
        let mut pending = PendingResponses::default();
        let poll_calls = Rc::new(Cell::new(0usize));
        let calls_for_poll = Rc::clone(&poll_calls);
        pending.register(PendingResponse {
            request_id: "pending-registry".to_string(),
            session_id: session_id.to_string(),
            attach_command: "read".to_string(),
            poll: Box::new(move |_| {
                let next = calls_for_poll.get() + 1;
                calls_for_poll.set(next);
                if next <= 2 {
                    None
                } else {
                    Some(Response::success(
                        "pending-registry",
                        serde_json::json!({"ready": true}),
                    ))
                }
            }),
        });

        let mut writer = Vec::new();
        for expected_calls in 1..=2 {
            assert_eq!(
                write_ready_pending_to_writer(&fixture.ctx, &mut pending, &mut writer).unwrap(),
                0
            );
            assert!(writer.is_empty());
            assert_eq!(poll_calls.get(), expected_calls);
        }

        assert_eq!(
            write_ready_pending_to_writer(&fixture.ctx, &mut pending, &mut writer).unwrap(),
            1
        );
        assert!(pending.is_empty());
        assert_eq!(poll_calls.get(), 3);
        let values = line_values(&writer);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["id"], "pending-registry");
        assert_eq!(
            values[0]["bg_completions"][0]["task_id"],
            "bash-0000000000000502"
        );
        assert_eq!(values[0]["status_bar"]["dead_code"], 3);

        assert_eq!(
            write_ready_pending_to_writer(&fixture.ctx, &mut pending, &mut writer).unwrap(),
            0
        );
        assert_eq!(line_values(&writer).len(), 1);
        assert_eq!(poll_calls.get(), 3);
    }

    #[test]
    fn wait_true_detach_signal_finalizes_early_and_keeps_completion_reminder() {
        let root = TempDir::new().unwrap();
        let storage = TempDir::new().unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        );
        let frames = Arc::new(Mutex::new(Vec::<PushFrame>::new()));
        let frames_for_sender = Arc::clone(&frames);
        ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            frames_for_sender.lock().unwrap().push(frame);
        }))));
        let session_id = "session-wait-detach";
        let request = RawRequest {
            id: "wait-detach".to_string(),
            command: "bash".to_string(),
            lsp_hints: None,
            session_id: Some(session_id.to_string()),
            params: serde_json::json!({
                "command": WAIT_DETACH_COMMAND,
                "timeout": 10_000,
                "foreground_orchestrate": true,
                "block_to_completion": true,
                "wait": true,
                "notify_on_completion": false,
                "compressed": true,
            }),
        };
        let spawn_response = aft::commands::bash::handle(&request, &ctx);
        assert!(spawn_response.success);
        assert_eq!(spawn_response.data["status"], serde_json::json!("running"));
        let task_id = spawn_response.data["task_id"].as_str().unwrap().to_string();
        let outcome =
            aft::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, spawn_response);
        let mut pending = PendingResponses::default();
        match outcome {
            aft::response_finalize::DispatchOutcome::Deferred(pending_response) => {
                pending.register(pending_response);
            }
            aft::response_finalize::DispatchOutcome::Immediate(response) => {
                panic!("expected deferred wait-mode bash, got immediate response: {response:?}");
            }
        }

        let mut writer = Vec::new();
        assert_eq!(
            write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
            0
        );
        assert!(writer.is_empty());

        let detach = super::dispatch(
            RawRequest {
                id: "wait-detach-signal".to_string(),
                command: "bash_wait_detach".to_string(),
                lsp_hints: None,
                session_id: Some(session_id.to_string()),
                params: serde_json::json!({}),
            },
            &ctx,
        );
        assert!(detach.success);
        assert_eq!(detach.data["detached"], serde_json::json!(true));

        assert_eq!(
            write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
            1
        );
        let values = line_values(&writer);
        let response = values.last().unwrap();
        let output = response["output"].as_str().unwrap();
        assert_eq!(response["id"], serde_json::json!("wait-detach"));
        assert_eq!(response["status"], serde_json::json!("running"));
        assert!(output.contains(&task_id));
        assert!(output.contains("Detached because a user message arrived."));
        assert!(pending.is_empty());

        let running = wait_for_snapshot(
            &ctx,
            root.path(),
            storage.path(),
            session_id,
            &task_id,
            |snapshot| snapshot.info.status == BgTaskStatus::Running,
        );
        assert_eq!(running.info.status, BgTaskStatus::Running);

        wait_for_snapshot(
            &ctx,
            root.path(),
            storage.path(),
            session_id,
            &task_id,
            |snapshot| snapshot.info.status.is_terminal(),
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let saw_completion = frames.lock().unwrap().iter().any(|frame| {
                matches!(
                    frame,
                    PushFrame::BashCompleted(completion)
                        if completion.task_id == task_id && completion.session_id == session_id
                )
            });
            if saw_completion {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for completion reminder for {task_id}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn tick_drains_runtime_events_and_polls_pending() {
        let root = TempDir::new().unwrap();
        let registry = make_runtime_registry(root.path());
        let drain_frames = Arc::new(AtomicUsize::new(0));
        let frames_for_sender = Arc::clone(&drain_frames);
        registry
            .current()
            .set_progress_sender(Some(Arc::new(Box::new(move |_| {
                frames_for_sender.fetch_add(1, Ordering::SeqCst);
            }))));
        let generation = registry.current().advance_configure_generation();
        registry
            .current()
            .configure_warnings_sender()
            .send((
                generation,
                ConfigureWarningsFrame {
                    frame_type: "configure_warnings",
                    session_id: Some("session-drain".to_string()),
                    project_root: root.path().display().to_string(),
                    warnings: Vec::new(),
                },
            ))
            .unwrap();

        let mut pending = PendingResponses::default();
        let poll_calls = Rc::new(Cell::new(0usize));
        let calls_for_poll = Rc::clone(&poll_calls);
        pending.register(PendingResponse {
            request_id: "pending-drain".to_string(),
            session_id: "session-drain".to_string(),
            attach_command: "read".to_string(),
            poll: Box::new(move |_| {
                calls_for_poll.set(calls_for_poll.get() + 1);
                None
            }),
        });
        let mut writer = Vec::new();

        assert_eq!(
            drain_runtime_events_and_write_pending_to_writer(&registry, &mut pending, &mut writer)
                .unwrap(),
            0
        );
        assert_eq!(drain_frames.load(Ordering::SeqCst), 1);
        assert_eq!(poll_calls.get(), 2);
        assert!(writer.is_empty());
        assert!(!pending.is_empty());
    }

    #[test]
    fn tick_flushes_ready_pending_before_runtime_maintenance() {
        let root = TempDir::new().unwrap();
        let registry = make_runtime_registry(root.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_drain = Arc::clone(&events);
        registry
            .current()
            .set_progress_sender(Some(Arc::new(Box::new(move |_| {
                events_for_drain.lock().unwrap().push("drain");
            }))));
        let generation = registry.current().advance_configure_generation();
        registry
            .current()
            .configure_warnings_sender()
            .send((
                generation,
                ConfigureWarningsFrame {
                    frame_type: "configure_warnings",
                    session_id: Some("session-order".to_string()),
                    project_root: root.path().display().to_string(),
                    warnings: Vec::new(),
                },
            ))
            .unwrap();

        let events_for_pending = Arc::clone(&events);
        let mut pending = PendingResponses::default();
        pending.register(PendingResponse {
            request_id: "pending-order".to_string(),
            session_id: "session-order".to_string(),
            attach_command: "bash".to_string(),
            poll: Box::new(move |_| {
                events_for_pending.lock().unwrap().push("pending");
                Some(Response::success("pending-order", serde_json::json!({})))
            }),
        });
        let mut writer = Vec::new();

        assert_eq!(
            drain_runtime_events_and_write_pending_to_writer(&registry, &mut pending, &mut writer)
                .unwrap(),
            1
        );
        assert_eq!(*events.lock().unwrap(), ["pending", "drain"]);
        assert_eq!(line_values(&writer).len(), 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn long_pending_response_is_polled_each_tick_without_writing_until_ready() {
        let root = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(root.path());
        let mut pending = PendingResponses::default();
        let poll_calls = Rc::new(Cell::new(0usize));
        let calls_for_poll = Rc::clone(&poll_calls);
        pending.register(PendingResponse {
            request_id: "pending-long".to_string(),
            session_id: "session-long".to_string(),
            attach_command: "read".to_string(),
            poll: Box::new(move |_| {
                let next = calls_for_poll.get() + 1;
                calls_for_poll.set(next);
                if next <= 50 {
                    None
                } else {
                    Some(Response::success(
                        "pending-long",
                        serde_json::json!({"ready_after_ticks": next}),
                    ))
                }
            }),
        });
        let mut writer = Vec::new();

        for expected_calls in 1..=50 {
            assert_eq!(
                write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
                0
            );
            assert!(writer.is_empty());
            assert_eq!(poll_calls.get(), expected_calls);
        }

        assert_eq!(
            write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
            1
        );
        let values = line_values(&writer);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["id"], "pending-long");
        assert_eq!(values[0]["ready_after_ticks"], 51);
        assert!(pending.is_empty());
    }

    #[test]
    fn multiple_pending_responses_resolve_independently() {
        let root = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(root.path());
        let second_ready = Rc::new(Cell::new(false));
        let second_ready_for_poll = Rc::clone(&second_ready);
        let mut pending = PendingResponses::default();
        pending.register(PendingResponse {
            request_id: "pending-ready".to_string(),
            session_id: "session-multi".to_string(),
            attach_command: "read".to_string(),
            poll: Box::new(|_| {
                Some(Response::success(
                    "pending-ready",
                    serde_json::json!({"which": "ready"}),
                ))
            }),
        });
        pending.register(PendingResponse {
            request_id: "pending-later".to_string(),
            session_id: "session-multi".to_string(),
            attach_command: "read".to_string(),
            poll: Box::new(move |_| {
                if second_ready_for_poll.get() {
                    Some(Response::success(
                        "pending-later",
                        serde_json::json!({"which": "later"}),
                    ))
                } else {
                    None
                }
            }),
        });
        let mut writer = Vec::new();

        assert_eq!(
            write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
            1
        );
        let values = line_values(&writer);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0]["id"], "pending-ready");
        assert!(!pending.is_empty());

        second_ready.set(true);
        assert_eq!(
            write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
            1
        );
        let values = line_values(&writer);
        assert_eq!(values.len(), 2);
        assert_eq!(values[1]["id"], "pending-later");
        assert!(pending.is_empty());
    }

    #[test]
    fn shutdown_drops_pending_without_polling_or_writing() {
        let root = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(root.path());
        let poll_calls = Rc::new(Cell::new(0usize));
        let calls_for_poll = Rc::clone(&poll_calls);
        let mut pending = PendingResponses::default();
        pending.register(PendingResponse {
            request_id: "pending-shutdown".to_string(),
            session_id: "session-shutdown".to_string(),
            attach_command: "read".to_string(),
            poll: Box::new(move |_| {
                calls_for_poll.set(calls_for_poll.get() + 1);
                Some(Response::success(
                    "pending-shutdown",
                    serde_json::json!({"should_not_write": true}),
                ))
            }),
        });
        let mut writer = Vec::new();

        pending.drain_on_shutdown();
        assert!(pending.is_empty());
        assert_eq!(poll_calls.get(), 0);
        assert_eq!(
            write_ready_pending_to_writer(&ctx, &mut pending, &mut writer).unwrap(),
            0
        );
        assert!(writer.is_empty());
        assert_eq!(poll_calls.get(), 0);
    }
}

#[cfg(test)]
mod signal_handler_tests {
    use super::{signal_bg_registries, App, AppContext, Config, RuntimeRegistry};
    use std::sync::Arc;

    #[test]
    fn signal_bg_registry_collection_includes_standalone_actor() {
        let app = App::default_shared();
        let ctx = AppContext::from_app(Arc::clone(&app), Config::default());
        let registry = RuntimeRegistry::standalone(app, ctx);

        assert_eq!(signal_bg_registries(&registry).len(), 1);
    }
}

#[cfg(test)]
mod graceful_shutdown_search_index_tests {
    use super::{
        flush_indexes_on_graceful_shutdown, global_gitignore_env_test_lock, App, AppContext,
        Config, RuntimeRegistry,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    fn make_runtime_registry(root: &Path, storage: &Path) -> RuntimeRegistry {
        let app = App::default_shared();
        let ctx = AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(std::fs::canonicalize(root).expect("canonical root"));
        RuntimeRegistry::standalone(app, ctx)
    }

    fn install_resident_search_index(
        ctx: &AppContext,
        root: &Path,
        storage: &Path,
    ) -> (PathBuf, PathBuf) {
        let canonical_root = std::fs::canonicalize(root).expect("canonical root");
        let cache_dir = aft::search_index::resolve_cache_dir(&canonical_root, Some(storage));
        let mut index = aft::search_index::SearchIndex::build(&canonical_root);
        let git_head = index.stored_git_head().map(str::to_owned);
        index.write_to_disk(&cache_dir, git_head.as_deref());
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        (canonical_root, cache_dir)
    }

    fn wait_for_next_directory_write_mtime(dir: &Path, baseline: SystemTime) {
        let probe = dir.join("mtime-probe");
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut counter = 0u64;
        loop {
            std::fs::write(&probe, counter.to_string()).expect("write mtime probe");
            let modified = std::fs::metadata(&probe)
                .expect("mtime probe metadata")
                .modified()
                .expect("mtime probe modified time");
            if modified != baseline {
                let _ = std::fs::remove_file(&probe);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a later directory write mtime"
            );
            counter += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stable_global_gitignore_env_guard() -> std::sync::MutexGuard<'static, ()> {
        global_gitignore_env_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_for_search_index_build_to_finish(ctx: &AppContext) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            aft::runtime_drain::drain_search_index_events(ctx);
            if ctx
                .search_index_rx()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the delayed search rebuild to publish"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn graceful_shutdown_flushes_search_delta_to_disk() {
        // Search cache reads validate the global git-ignore fingerprint stamped
        // at write time. Keep HOME/XDG_CONFIG_HOME stable while this test writes
        // and reloads cache.bin, because other bin tests temporarily redirect
        // those environment variables to neutralize developer-global ignores.
        let _global_gitignore_env_guard = stable_global_gitignore_env_guard();
        let root = tempfile::tempdir().expect("root tempdir");
        let storage = tempfile::tempdir().expect("storage tempdir");
        let file = root.path().join("src.txt");
        std::fs::write(&file, "old shutdown token\n").expect("write source");

        let registry = make_runtime_registry(root.path(), storage.path());
        let ctx = registry.current();
        let (canonical_root, cache_dir) =
            install_resident_search_index(ctx, root.path(), storage.path());

        std::fs::write(&file, "new shutdown token\n").expect("edit source");
        {
            let mut search_index = ctx
                .search_index()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            search_index
                .as_mut()
                .expect("resident search index")
                .update_file(&file);
        }

        flush_indexes_on_graceful_shutdown(&registry);

        let mut restored =
            aft::search_index::SearchIndex::read_from_disk(&cache_dir, &canonical_root)
                .expect("load flushed search index");
        restored.ready = true;
        assert_eq!(
            restored
                .grep("new shutdown token", true, &[], &[], &canonical_root, 10)
                .matches
                .len(),
            1,
            "graceful shutdown should persist the edited file into cache.bin"
        );
        assert!(
            restored
                .grep("old shutdown token", true, &[], &[], &canonical_root, 10)
                .matches
                .is_empty(),
            "without the shutdown flush this test goes red because disk still holds the old base"
        );
    }

    #[test]
    fn graceful_shutdown_waits_for_inflight_search_rebuild_before_flushing() {
        let _global_gitignore_env_guard = stable_global_gitignore_env_guard();
        let root = tempfile::tempdir().expect("root tempdir");
        let storage = tempfile::tempdir().expect("storage tempdir");
        let file = root.path().join("src.txt");
        std::fs::write(&file, "old delayed shutdown token\n").expect("write source");

        let registry = make_runtime_registry(root.path(), storage.path());
        let ctx = registry.current();
        let (canonical_root, cache_dir) =
            install_resident_search_index(ctx, root.path(), storage.path());

        std::fs::write(&file, "new delayed shutdown token\n").expect("edit source");
        let (publish_reached_rx, shutdown_waiting_rx, publish_release_tx) =
            aft::runtime_drain::install_search_rebuild_publish_gate_for_test();
        aft::runtime_drain::spawn_search_corpus_refresh(ctx, canonical_root.clone(), ctx.config());
        publish_reached_rx
            .recv_timeout(Duration::from_secs(12))
            .expect("search rebuild reaches its publish gate");
        assert!(
            ctx.search_index_rx()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "test should exercise graceful shutdown while a search rebuild is still in flight"
        );

        let release = std::thread::spawn(move || {
            shutdown_waiting_rx
                .recv_timeout(Duration::from_secs(12))
                .expect("graceful shutdown waits for the in-flight rebuild");
            publish_release_tx
                .send(())
                .expect("release search rebuild publish gate");
        });
        flush_indexes_on_graceful_shutdown(&registry);
        release.join().expect("publish gate release thread");

        let mut restored =
            aft::search_index::SearchIndex::read_from_disk(&cache_dir, &canonical_root)
                .expect("load search index after delayed rebuild flush");
        restored.ready = true;
        let new_matches = restored
            .grep(
                "new delayed shutdown token",
                true,
                &[],
                &[],
                &canonical_root,
                10,
            )
            .matches
            .len();
        let old_matches = restored
            .grep(
                "old delayed shutdown token",
                true,
                &[],
                &[],
                &canonical_root,
                10,
            )
            .matches
            .len();
        wait_for_search_index_build_to_finish(ctx);

        assert_eq!(
            new_matches, 1,
            "graceful shutdown should wait for the in-flight rebuild to publish before flushing"
        );
        assert_eq!(
            old_matches, 0,
            "shutdown flush should not leave the old cache on disk"
        );
    }

    #[test]
    fn graceful_shutdown_skips_read_only_search_flush() {
        let root = tempfile::tempdir().expect("root tempdir");
        let storage = tempfile::tempdir().expect("storage tempdir");
        let file = root.path().join("src.txt");
        std::fs::write(&file, "old readonly token\n").expect("write source");

        let registry = make_runtime_registry(root.path(), storage.path());
        let ctx = registry.current();
        let (_canonical_root, cache_dir) =
            install_resident_search_index(ctx, root.path(), storage.path());

        std::fs::write(&file, "new readonly token\n").expect("edit source");
        {
            let mut search_index = ctx
                .search_index()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            search_index
                .as_mut()
                .expect("resident search index")
                .update_file(&file);
        }
        let cache_path = cache_dir.join("cache.bin");
        let before = std::fs::metadata(&cache_path)
            .expect("cache metadata before shutdown")
            .modified()
            .expect("cache mtime before shutdown");

        ctx.set_cache_role(true, None);
        wait_for_next_directory_write_mtime(&cache_dir, before);
        flush_indexes_on_graceful_shutdown(&registry);

        let after = std::fs::metadata(&cache_path)
            .expect("cache metadata after shutdown")
            .modified()
            .expect("cache mtime after shutdown");
        assert_eq!(
            before, after,
            "worktree/read-only runtimes must not rewrite shared search artifacts on shutdown"
        );
    }

    #[test]
    fn graceful_shutdown_skips_empty_search_delta() {
        let root = tempfile::tempdir().expect("root tempdir");
        let storage = tempfile::tempdir().expect("storage tempdir");
        std::fs::write(root.path().join("src.txt"), "unchanged token\n").expect("write source");

        let registry = make_runtime_registry(root.path(), storage.path());
        let ctx = registry.current();
        let (_canonical_root, cache_dir) =
            install_resident_search_index(ctx, root.path(), storage.path());
        let cache_path = cache_dir.join("cache.bin");
        let before = std::fs::metadata(&cache_path)
            .expect("cache metadata before shutdown")
            .modified()
            .expect("cache mtime before shutdown");

        wait_for_next_directory_write_mtime(&cache_dir, before);
        flush_indexes_on_graceful_shutdown(&registry);

        let after = std::fs::metadata(&cache_path)
            .expect("cache metadata after shutdown")
            .modified()
            .expect("cache mtime after shutdown");
        assert_eq!(
            before, after,
            "shutdown with no trigram delta should leave cache.bin untouched"
        );
    }
}

#[cfg(test)]
mod watcher_filter_tests {
    use super::{dispatch, dispatch_panic_response, write_push_frame_or_request_shutdown};
    use aft::config::{BackupConfig, Config};
    use aft::context::{
        AppContext, CallgraphStoreAccess, SemanticIndexEvent, SemanticIndexStatus,
        SemanticRefreshEvent, SemanticRefreshRequest, SemanticRefreshWorkerSlot,
    };
    use aft::harness::Harness;
    use aft::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
    use aft::lsp::registry::ServerKind;
    use aft::lsp::roots::ServerKey;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::{ConfigureWarningsFrame, PushFrame, RawRequest, Response};
    use aft::response_finalize::attach_status_bar;
    use aft::runtime_drain::{
        drain_semantic_refresh_events, drain_watcher_events,
        record_semantic_refresh_transient_failure, schedule_semantic_refresh_retry,
        semantic_refresh_circuit_is_open, semantic_refresh_probe_is_scheduled_for_test,
        semantic_refresh_transient_failure_count_for_test, watcher_path_is_callgraph_indexed,
        BREAKER_TRIP_THRESHOLD, MAX_RETRY_ATTEMPTS, WATCHER_BATCH_INLINE_CAP,
    };
    use aft::semantic_index::SemanticIndex;
    use aft::watcher_filter::{
        watcher_event_invalidates, FilteredWatcherPaths, WatcherDispatchEvent, WatcherFilterConfig,
    };
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify::EventKind;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// Wait budget for an async dispatch (semantic-refresh request / status
    /// frame) the worker produces on a freshly-spawned thread. The dispatch
    /// thread does `spawn -> sleep(backoff) -> send`, so under CI load the
    /// spawn-plus-wakeup latency can briefly exceed a tight budget. The wait
    /// returns the instant the value arrives, so this is zero-cost on the happy
    /// path. Negative waits (asserting absence) must stay short and are NOT
    /// this const.
    const RECV_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    fn make_ctx_with_root(root: &std::path::Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    fn install_watcher_rx(ctx: &AppContext) -> crossbeam_channel::Sender<WatcherDispatchEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        *ctx.watcher_rx().lock() = Some(rx);
        tx
    }

    fn watcher_paths_event(path: std::path::PathBuf) -> WatcherDispatchEvent {
        WatcherDispatchEvent::Paths(vec![path])
    }

    fn filter_watcher_raw_paths<I>(ctx: &AppContext, raw_paths: I) -> FilteredWatcherPaths
    where
        I: IntoIterator<Item = std::path::PathBuf>,
    {
        let root = ctx
            .canonical_cache_root_opt()
            .or_else(|| ctx.config().project_root.clone())
            .expect("test context has project root");
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let config = WatcherFilterConfig::new(root, ctx.git_common_dir());
        aft::watcher_filter::filter_watcher_raw_paths_for_test(
            &config,
            &ctx.shared_gitignore(),
            raw_paths,
        )
    }

    // The callgraph store indexes every detected language (not just the trigram
    // SOURCE_EXTENSIONS set), so its watcher refresh must too — otherwise edits
    // to Java/C/C++/C#/Kotlin/Ruby/… serve stale call results until full rebuild.
    #[test]
    fn callgraph_watcher_gate_covers_all_indexed_languages() {
        use std::path::Path;
        for ok in [
            "Foo.java", "x.cpp", "y.c", "Svc.cs", "m.kt", "a.rb", "z.php", "s.scala", "C.sol",
            "app.ts", "main.rs", "h.go", "p.py",
        ] {
            assert!(
                watcher_path_is_callgraph_indexed(Path::new(ok)),
                "{ok} should be callgraph-indexed"
            );
        }
        // Genuinely-undetected extensions (detect_language → None). Note md/json/
        // yaml ARE detected (the store walks them at cold-build), so matching
        // cold-build means they refresh too — refresh coverage == index coverage.
        for skip in [
            "notes.txt",
            "image.png",
            "Cargo.lock",
            "data.csv",
            "config.toml",
        ] {
            assert!(
                !watcher_path_is_callgraph_indexed(Path::new(skip)),
                "{skip} should not be callgraph-indexed"
            );
        }
    }

    fn install_semantic_refresh_channels(
        ctx: &AppContext,
    ) -> (
        crossbeam_channel::Receiver<SemanticRefreshRequest>,
        crossbeam_channel::Sender<SemanticRefreshEvent>,
    ) {
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let worker_slot: SemanticRefreshWorkerSlot =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        ctx.install_semantic_refresh_worker(request_tx, event_rx, worker_slot);
        (request_rx, event_tx)
    }

    fn transient_embedding_error() -> String {
        format!(
            "{}backend unavailable",
            aft::semantic_index::TRANSIENT_EMBEDDING_MARKER
        )
    }

    fn recv_files_request(
        request_rx: &crossbeam_channel::Receiver<SemanticRefreshRequest>,
    ) -> Vec<std::path::PathBuf> {
        match request_rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("semantic refresh request")
        {
            SemanticRefreshRequest::Files { paths } => paths,
            SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
        }
    }

    fn status_frame_rx(ctx: &AppContext) -> std::sync::mpsc::Receiver<PushFrame> {
        let (tx, rx) = std::sync::mpsc::channel();
        ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
            let _ = tx.send(frame);
        }))));
        rx
    }

    fn recv_status_changed(rx: &std::sync::mpsc::Receiver<PushFrame>) -> serde_json::Value {
        match rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("status_changed frame")
        {
            PushFrame::StatusChanged(frame) => frame.snapshot,
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    fn err_diag(file: &std::path::Path) -> StoredDiagnostic {
        StoredDiagnostic {
            file: file.to_path_buf(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "boom".into(),
            code: None,
            source: None,
        }
    }

    /// Shared serialization lock for tests that override the process-global
    /// semantic retry-backoff environment variable. The semantic breaker itself
    /// is per-AppContext; only the test seam remains process-global.
    fn semantic_breaker_test_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_semantic_retry_backoff_ms<R>(ms: u64, f: impl FnOnce() -> R) -> R {
        let _guard = semantic_breaker_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("AFT_SEMANTIC_RETRY_BACKOFF_MS");
        unsafe {
            std::env::set_var("AFT_SEMANTIC_RETRY_BACKOFF_MS", ms.to_string());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("AFT_SEMANTIC_RETRY_BACKOFF_MS", value),
                None => std::env::remove_var("AFT_SEMANTIC_RETRY_BACKOFF_MS"),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Run `f` with global git-ignore discovery neutralized.
    ///
    /// `rebuild_gitignore` loads git's global excludes via the `ignore`
    /// crate, which discovers them from TWO places: `core.excludesfile` in
    /// `$HOME/.gitconfig` (or `$XDG_CONFIG_HOME/git/config`), and the default
    /// `$XDG_CONFIG_HOME/git/ignore` / `$HOME/.config/git/ignore` locations.
    /// A developer machine commonly has one of these, so any "no project
    /// ignore → None" baseline is only deterministic when BOTH discovery
    /// roots point at an empty directory — neutralizing only
    /// `XDG_CONFIG_HOME` still finds a `~/.gitconfig` `core.excludesfile`.
    /// Serialized by the same process-local mutex used by search-cache tests:
    /// otherwise a cache written while HOME points here can be rejected when it
    /// is read after HOME is restored.
    fn with_neutralized_global_gitignore<R>(f: impl FnOnce() -> R) -> R {
        let _guard = super::global_gitignore_env_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: serialized by LOCK above; restored immediately after `f`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("USERPROFILE", tmp.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }

    #[test]
    fn watcher_drain_refreshes_open_callgraph_store() {
        let _callgraph_refresh_worker_guard = super::callgraph_refresh_worker_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            aft::callgraph_store::flush_callgraph_store_refreshes_with_budget(Duration::from_secs(
                30
            )),
            "a prior callgraph refresh worker should fully stop before this test"
        );
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let source = root.join("main.ts");
        std::fs::write(
            &source,
            "export function entry() { oldLeaf(); }\nfunction oldLeaf() {}\nfunction newLeaf() {}\n",
        )
        .unwrap();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(root.join("storage")),
                callgraph_store: true,
                ..Config::default()
            },
        );
        ctx.set_harness(Harness::Opencode);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx.set_cache_role(false, None);
        ctx.rebuild_gitignore();
        let tx = install_watcher_rx(&ctx);
        {
            let store = ctx
                .ensure_callgraph_store()
                .unwrap()
                .expect("store should build on demand");
            let tree = store
                .call_tree(std::path::Path::new("main.ts"), "entry", 1)
                .unwrap();
            assert_eq!(tree.children[0].name, "oldLeaf");
        }

        std::fs::write(
            &source,
            "export function entry() { newLeaf(); }\nfunction oldLeaf() {}\nfunction newLeaf() {}\n",
        )
        .unwrap();
        aft::callgraph_store::set_callgraph_refresh_worker_test_seam(
            root.to_path_buf(),
            Duration::from_millis(350),
            false,
        );
        tx.send(WatcherDispatchEvent::Paths(vec![source])).unwrap();
        let drain_started = Instant::now();
        drain_watcher_events(&ctx);
        assert!(
            drain_started.elapsed() < Duration::from_millis(250),
            "watcher drain waited for the callgraph store write"
        );

        let store = {
            let guard = ctx
                .callgraph_store()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .as_ref()
                .map(std::sync::Arc::clone)
                .expect("store remains open")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let tree = store
                .call_tree(std::path::Path::new("main.ts"), "entry", 1)
                .unwrap();
            if tree.children[0].name == "newLeaf" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "async callgraph refresh did not publish the changed edge"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            aft::callgraph_store::flush_callgraph_store_refreshes_with_budget(Duration::from_secs(
                1
            ))
        );
        aft::callgraph_store::clear_callgraph_refresh_worker_test_seam(root);
    }

    #[test]
    fn create_and_remove_invalidate() {
        assert!(watcher_event_invalidates(&EventKind::Create(
            CreateKind::File
        )));
        assert!(watcher_event_invalidates(&EventKind::Remove(
            RemoveKind::File
        )));
    }

    #[test]
    fn modify_data_invalidates() {
        // The "actual file write" case — must invalidate.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Data(DataChange::Content)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Data(DataChange::Any)
        )));
    }

    #[test]
    fn modify_name_rename_invalidates() {
        // Renames should invalidate the old path's cached state.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Name(RenameMode::To)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Name(RenameMode::From)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Name(RenameMode::Both)
        )));
    }

    #[test]
    fn modify_metadata_writetime_invalidates() {
        // mtime change implies real content edit on every supported platform.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::WriteTime)
        )));
    }

    #[test]
    fn modify_metadata_any_or_other_invalidates() {
        // Catch-all "we can't tell what changed" — better to over-invalidate.
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Any)
        )));
        assert!(watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Other)
        )));
    }

    /// Regression: biome-lint reading every TS file under Linux atime triggers
    /// notify `Modify(Metadata(AccessTime))` events. Treating those as
    /// invalidations re-parses the entire symbol cache, search index, and
    /// semantic index for every read-only lint pass — wasted work.
    #[test]
    fn modify_metadata_access_time_does_not_invalidate() {
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::AccessTime)
        )));
    }

    #[test]
    fn modify_metadata_permissions_ownership_extended_do_not_invalidate() {
        // chmod / chown / xattrs don't change content.
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Permissions)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Ownership)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Extended)
        )));
    }

    #[test]
    fn access_events_do_not_invalidate() {
        // Read syscalls cause an atime feedback loop on Linux when the watcher
        // is watching a directory we read into.
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Open(AccessMode::Read)
        )));
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Read
        )));
        assert!(!watcher_event_invalidates(&EventKind::Access(
            AccessKind::Close(AccessMode::Read)
        )));
    }

    #[test]
    fn other_event_kinds_do_not_invalidate() {
        // `Other`, `Any` — we explicitly opt out of unknown event categories
        // since the existing `Modify(_)` and `Modify(Metadata(Any))` arms
        // already handle the meaningful catch-all cases.
        assert!(!watcher_event_invalidates(&EventKind::Other));
        assert!(!watcher_event_invalidates(&EventKind::Any));
    }

    #[test]
    fn dispatch_panic_response_is_clear_internal_error() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");

        let response = dispatch_panic_response("panic-id", "db_get_state", payload.as_ref());

        assert!(!response.success);
        assert_eq!(response.data["code"], "internal_error");
        assert!(response.data["message"]
            .as_str()
            .unwrap()
            .contains("command 'db_get_state' panicked: boom"));
    }

    #[test]
    fn dispatch_rejects_agent_safety_commands_when_backups_disabled() {
        let temp = TempDir::new().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        config.backup = BackupConfig {
            enabled: Some(false),
            ..BackupConfig::default()
        };
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
        for command in [
            "undo",
            "undo_preview",
            "edit_history",
            "checkpoint",
            "checkpoint_paths",
            "restore_checkpoint",
            "list_checkpoints",
        ] {
            let response = dispatch(
                RawRequest {
                    id: command.to_string(),
                    command: command.to_string(),
                    params: serde_json::json!({}),
                    session_id: None,
                    lsp_hints: None,
                },
                &ctx,
            );
            assert!(!response.success, "{command} unexpectedly succeeded");
            assert_eq!(response.data["code"], "backups_disabled");
        }
    }

    #[test]
    fn push_frame_write_error_requests_shutdown() {
        struct BrokenWriter;

        impl std::io::Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stdout closed",
                ))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let shutdown = std::sync::atomic::AtomicBool::new(false);
        let frame = PushFrame::ConfigureWarnings(ConfigureWarningsFrame::new("/repo", Vec::new()));

        write_push_frame_or_request_shutdown(&mut BrokenWriter, &frame, &shutdown);

        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn status_bar_attach_skips_unchanged_fingerprint() {
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);

        let mut first = Response::success("one", serde_json::json!({}));
        attach_status_bar(&mut first, &ctx, "read");
        assert_eq!(first.data["status_bar"]["dead_code"], 1);
        assert_eq!(first.data["status_bar"]["unused_exports"], 2);
        assert_eq!(first.data["status_bar"]["duplicates"], 3);
        assert_eq!(first.data["status_bar"]["todos"], 4);

        let mut unchanged = Response::success("two", serde_json::json!({}));
        attach_status_bar(&mut unchanged, &ctx, "read");
        assert!(unchanged.data.get("status_bar").is_none());

        ctx.update_status_bar_tier2(Some(5), Some(2), Some(3), Some(4), false);
        let mut changed = Response::success("three", serde_json::json!({}));
        attach_status_bar(&mut changed, &ctx, "read");
        assert_eq!(changed.data["status_bar"]["dead_code"], 5);
    }

    #[test]
    fn configure_warning_drain_drops_stale_generation() {
        let tmp = TempDir::new().unwrap();
        let ctx = make_ctx_with_root(tmp.path());
        let (frame_tx, frame_rx) = std::sync::mpsc::channel();
        ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
            let _ = frame_tx.send(frame);
        }))));

        let warnings_tx = ctx.configure_warnings_sender();
        let current_generation = ctx.advance_configure_generation();
        warnings_tx
            .send((
                current_generation - 1,
                ConfigureWarningsFrame::new("/stale", Vec::new()),
            ))
            .unwrap();
        warnings_tx
            .send((
                current_generation,
                ConfigureWarningsFrame::new("/current", Vec::new()),
            ))
            .unwrap();

        aft::runtime_drain::drain_configure_warning_events(&ctx);

        let frame = frame_rx.try_recv().expect("current warning frame");
        match frame {
            PushFrame::ConfigureWarnings(frame) => {
                assert_eq!(frame.project_root, "/current");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(frame_rx.try_recv().is_err());
    }

    #[test]
    fn gitignore_write_rebuilds_before_filtering_same_batch_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let gitignore = root.join(".gitignore");
        let ignored = root.join("foo.txt");
        let kept = root.join("bar.txt");
        std::fs::write(&ignored, "ignored").unwrap();
        std::fs::write(&kept, "kept").unwrap();

        let ctx = make_ctx_with_root(root);
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        assert!(ctx.gitignore().is_none());

        std::fs::write(&gitignore, "foo.txt\n").unwrap();
        with_neutralized_global_gitignore(|| ctx.rebuild_gitignore());
        let changed =
            filter_watcher_raw_paths(&ctx, vec![gitignore.clone(), ignored.clone(), kept.clone()]);

        let gitignore = std::fs::canonicalize(gitignore).unwrap();
        let ignored = std::fs::canonicalize(ignored).unwrap();
        let kept = std::fs::canonicalize(kept).unwrap();
        assert!(changed.ignore_file_changed);
        assert!(changed.changed.contains(&gitignore));
        assert!(!changed.changed.contains(&ignored));
        assert!(changed.changed.contains(&kept));
    }

    #[test]
    fn infra_ignore_file_does_not_request_corpus_refresh_but_project_aftignore_does() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let infra_gitignore = root.join("node_modules").join("pkg").join(".gitignore");
        std::fs::create_dir_all(infra_gitignore.parent().unwrap()).unwrap();
        std::fs::write(&infra_gitignore, "dist/\n").unwrap();

        let ctx = make_ctx_with_root(root);
        let changed = filter_watcher_raw_paths(&ctx, vec![infra_gitignore]);

        assert!(!changed.ignore_file_changed);
        assert!(changed.changed.is_empty());

        let aftignore = root.join(".aftignore");
        std::fs::write(&aftignore, "ignored/\n").unwrap();
        let changed = filter_watcher_raw_paths(&ctx, vec![aftignore.clone()]);

        let aftignore = std::fs::canonicalize(aftignore).unwrap();
        assert!(changed.ignore_file_changed);
        assert!(changed.changed.contains(&aftignore));
    }

    #[test]
    fn project_git_info_exclude_requests_corpus_refresh_without_indexing_git_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let git_info = root.join(".git").join("info");
        std::fs::create_dir_all(&git_info).unwrap();
        let exclude = git_info.join("exclude");
        std::fs::write(&exclude, "ignored/\n").unwrap();

        let ctx = make_ctx_with_root(root);
        let changed = filter_watcher_raw_paths(&ctx, vec![exclude]);

        assert!(changed.ignore_file_changed);
        assert!(changed.changed.is_empty());
    }

    #[test]
    fn shared_git_info_exclude_requests_corpus_refresh_without_indexing_external_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let common = TempDir::new().unwrap();
        let git_info = common.path().join("info");
        std::fs::create_dir_all(&git_info).unwrap();
        let exclude = git_info.join("exclude");
        std::fs::write(&exclude, "ignored/\n").unwrap();

        let ctx = make_ctx_with_root(root);
        ctx.set_cache_role(false, Some(common.path().to_path_buf()));
        let changed = filter_watcher_raw_paths(&ctx, vec![exclude]);

        assert!(changed.ignore_file_changed);
        assert!(changed.changed.is_empty());
    }

    #[test]
    fn semantic_index_disconnect_without_terminal_event_marks_failed() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = make_ctx_with_root(&root);
        let (tx, rx) = crossbeam_channel::unbounded::<SemanticIndexEvent>();
        *ctx.semantic_index_rx().lock() = Some(rx);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "embedding".into(),
            files: Some(1),
            entries_done: Some(0),
            entries_total: Some(1),
        };
        drop(tx);

        aft::runtime_drain::drain_semantic_index_events(&ctx);

        assert!(ctx.semantic_index_rx().lock().is_none());
        assert!(matches!(
            &*ctx.semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Failed(message)
                if message.contains("disconnected before reporting completion")
        ));
    }

    #[test]
    fn semantic_refresh_disconnect_after_started_cancels_refreshing_paths() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let ctx = make_ctx_with_root(&root);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (_request_rx, event_tx) = install_semantic_refresh_channels(&ctx);
        event_tx
            .send(SemanticRefreshEvent::Started {
                paths: vec![file.clone()],
            })
            .unwrap();
        drop(event_tx);

        drain_semantic_refresh_events(&ctx);

        assert!(ctx.semantic_refresh_event_rx().lock().is_none());
        assert_eq!(
            ctx.semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .refreshing_count(),
            0
        );
    }

    #[test]
    fn watcher_overflow_rescan_reschedules_callgraph_store_instead_of_blocking() {
        // Regression for the configure/bash timeout on OpenCode Desktop large
        // repos (v0.39.1): a watcher overflow (RescanRequired) used to run the
        // callgraph store's full cold_build SYNCHRONOUSLY inside
        // drain_watcher_events — which runs on the single dispatch thread before
        // every request — blocking configure/bash past the 30s transport
        // timeout. The drain must now DROP the resident store and schedule a
        // BACKGROUND rebuild, never cold-build inline.
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("lib.rs"), "fn used() {}\nfn main() { used(); }\n").unwrap();

        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                storage_dir: Some(tmp.path().join("storage")),
                callgraph_store: true,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());
        ctx.rebuild_gitignore();

        // Make a callgraph store resident (synchronous build of the 1-file repo).
        let resident = ctx
            .ensure_callgraph_store()
            .expect("ensure callgraph store");
        assert!(resident.is_some(), "callgraph store should be resident");
        drop(resident);
        assert!(ctx
            .callgraph_store()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some());

        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx
            .send(WatcherDispatchEvent::RescanRequired)
            .unwrap();

        drain_watcher_events(&ctx);

        // The resident store is dropped (rescheduled), NOT refreshed in place...
        assert!(
            ctx.callgraph_store()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "watcher overflow must drop the resident store to reschedule, not refresh inline"
        );
        // ...and the drain itself did NOT spawn a build (no inline cold_build on
        // the dispatch thread); the rebuild happens lazily on the next op.
        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "drain must not start a synchronous/inline callgraph build"
        );
        // The next callgraph op will see the force flag and background-build.
    }

    #[test]
    fn watcher_large_batch_reschedules_indexes_instead_of_inline_refresh() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        std::fs::write(
            root.join("main.ts"),
            "export function entry() { oldLeaf(); }\nfunction oldLeaf() {}\n",
        )
        .unwrap();

        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                storage_dir: Some(tmp.path().join("storage")),
                callgraph_store: true,
                search_index: true,
                semantic_search: true,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());
        ctx.rebuild_gitignore();

        let resident = ctx
            .ensure_callgraph_store()
            .expect("ensure callgraph store")
            .expect("callgraph store should build on demand");
        drop(resident);
        assert!(ctx
            .callgraph_store()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some());

        let mut search_index = aft::search_index::SearchIndex::new();
        search_index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(search_index);

        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.clone(), 3));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (request_rx, _event_tx) = install_semantic_refresh_channels(&ctx);

        let watcher_tx = install_watcher_rx(&ctx);
        let paths = (0..=WATCHER_BATCH_INLINE_CAP)
            .map(|i| root.join(format!("changed-{i}.ts")))
            .collect::<Vec<_>>();
        watcher_tx.send(WatcherDispatchEvent::Paths(paths)).unwrap();

        drain_watcher_events(&ctx);

        assert!(
            ctx.callgraph_store()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none(),
            "oversized watcher batch must drop the resident store instead of refreshing inline"
        );
        assert!(
            ctx.callgraph_store_rx().lock().is_none(),
            "drain must not start a callgraph cold build on the dispatch thread"
        );
        assert!(
            matches!(
                ctx.callgraph_store_for_ops(),
                CallgraphStoreAccess::Building
            ),
            "next callgraph op should start the forced background rebuild and return Building"
        );
        assert!(
            ctx.search_index_rx()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "oversized watcher batch should spawn a background search corpus refresh"
        );
        assert!(
            !ctx.search_index()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .expect("resident search index")
                .ready,
            "resident search index should be marked not-ready while the corpus refresh runs"
        );
        match request_rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("semantic corpus refresh request")
        {
            SemanticRefreshRequest::Corpus => {}
            SemanticRefreshRequest::Files { paths } => {
                panic!("expected semantic corpus refresh for oversized batch, got {paths:?}")
            }
        }
        assert!(request_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
    }

    #[test]
    fn watcher_error_clears_receiver_and_marks_degraded() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = make_ctx_with_root(&root);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx
            .send(WatcherDispatchEvent::Error(
                "watcher init failed".to_string(),
            ))
            .unwrap();

        drain_watcher_events(&ctx);

        assert!(ctx.watcher_rx().lock().is_none());
        assert!(ctx
            .degraded_reasons()
            .contains(&"watcher_unavailable".to_string()));
        let status = ctx.build_status_snapshot();
        assert!(status["degraded"].as_bool().unwrap_or(false));
        assert_eq!(
            status["degraded_reasons"],
            serde_json::json!(["watcher_unavailable"])
        );
    }

    #[test]
    fn project_root_deleted_control_drops_watcher_rx_and_marks_degraded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let ctx = make_ctx_with_root(&root);
        ctx.set_canonical_cache_root(root);

        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(WatcherDispatchEvent::RootDeleted).unwrap();

        drain_watcher_events(&ctx);

        assert!(ctx.watcher_rx().lock().is_none());
        assert!(ctx
            .degraded_reasons()
            .contains(&"project_root_deleted".to_string()));
    }

    #[test]
    fn watcher_semantic_refresh_includes_vue_extension() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("App.vue");
        std::fs::write(&file, "<script setup>const n = 1;</script>").unwrap();

        let ctx = make_ctx_with_root(&root);
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.clone(), 3));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (request_rx, _event_tx) = install_semantic_refresh_channels(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file.clone())).unwrap();

        drain_watcher_events(&ctx);

        match request_rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("semantic refresh request")
        {
            SemanticRefreshRequest::Files { paths } => assert_eq!(paths, vec![file]),
            SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
        }
    }

    #[test]
    fn watcher_rescan_required_coalesces_and_requests_one_corpus_refresh() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                semantic_search: true,
                ..Config::default()
            },
        );
        ctx.set_canonical_cache_root(root.clone());
        ctx.rebuild_gitignore();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SemanticIndex::new(root, 3));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        let (request_rx, _event_tx) = install_semantic_refresh_channels(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file.clone())).unwrap();
        watcher_tx
            .send(WatcherDispatchEvent::RescanRequired)
            .unwrap();
        watcher_tx
            .send(WatcherDispatchEvent::RescanRequired)
            .unwrap();
        watcher_tx.send(watcher_paths_event(file)).unwrap();

        drain_watcher_events(&ctx);

        match request_rx
            .recv_timeout(RECV_DISPATCH_TIMEOUT)
            .expect("semantic corpus refresh request")
        {
            SemanticRefreshRequest::Corpus => {}
            SemanticRefreshRequest::Files { paths } => {
                panic!("expected corpus refresh, got file refresh for {paths:?}")
            }
        }
        assert!(request_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
    }

    #[test]
    fn transient_file_refresh_failure_requeues_retry_without_completing() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);
            let mut status = SemanticIndexStatus::ready();
            status.add_refreshing_file(file.clone());
            status.start_refreshing_file(file.clone());
            *ctx.semantic_index_status()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = status;

            event_tx
                .send(SemanticRefreshEvent::Failed {
                    paths: vec![file.clone()],
                    error: format!(
                        "{}backend unavailable",
                        aft::semantic_index::TRANSIENT_EMBEDDING_MARKER
                    ),
                })
                .unwrap();

            drain_semantic_refresh_events(&ctx);

            match request_rx
                .recv_timeout(RECV_DISPATCH_TIMEOUT)
                .expect("retry request")
            {
                SemanticRefreshRequest::Files { paths } => assert_eq!(paths, vec![file.clone()]),
                SemanticRefreshRequest::Corpus => panic!("unexpected corpus refresh"),
            }
            assert_eq!(
                ctx.semantic_index_status()
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .refreshing_count(),
                1
            );
        });
    }

    #[test]
    fn semantic_refresh_breaker_coalesces_open_retries_into_single_probe() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let files = (0..(BREAKER_TRIP_THRESHOLD + 2))
            .map(|index| root.join(format!("file{index}.rs")))
            .collect::<Vec<_>>();
        for file in &files {
            std::fs::write(file, "fn main() {}\n").unwrap();
        }

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);

            for file in files.iter().take(BREAKER_TRIP_THRESHOLD) {
                event_tx
                    .send(SemanticRefreshEvent::Failed {
                        paths: vec![file.clone()],
                        error: transient_embedding_error(),
                    })
                    .unwrap();
            }
            drain_semantic_refresh_events(&ctx);

            assert!(semantic_refresh_circuit_is_open(&ctx));
            assert!(semantic_refresh_probe_is_scheduled_for_test(&ctx));

            for file in files.iter().skip(BREAKER_TRIP_THRESHOLD) {
                event_tx
                    .send(SemanticRefreshEvent::Failed {
                        paths: vec![file.clone()],
                        error: transient_embedding_error(),
                    })
                    .unwrap();
            }
            drain_semantic_refresh_events(&ctx);

            assert!(semantic_refresh_probe_is_scheduled_for_test(&ctx));
            let expected_probe_paths = files[(BREAKER_TRIP_THRESHOLD - 1)..].to_vec();
            let pending = ctx.take_pending_semantic_index_paths();
            assert_eq!(pending, expected_probe_paths);
            ctx.add_pending_semantic_index_paths(pending);

            std::thread::sleep(std::time::Duration::from_millis(20));
            assert!(
                ctx.completion_drains_have_work(),
                "a ready breaker probe must schedule completion draining without another event"
            );
            drain_semantic_refresh_events(&ctx);

            let mut batches = Vec::new();
            for _ in 0..BREAKER_TRIP_THRESHOLD {
                batches.push(recv_files_request(&request_rx));
            }
            for file in files.iter().take(BREAKER_TRIP_THRESHOLD - 1) {
                assert!(batches.contains(&vec![file.clone()]));
            }
            assert!(batches.contains(&expected_probe_paths));
            assert!(request_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err());
        });
    }

    #[test]
    fn transient_corpus_refresh_failure_retries_without_another_watcher_event() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);
            event_tx
                .send(SemanticRefreshEvent::CorpusFailed {
                    error: transient_embedding_error(),
                })
                .unwrap();

            drain_semantic_refresh_events(&ctx);

            assert!(semantic_refresh_circuit_is_open(&ctx));
            assert!(semantic_refresh_probe_is_scheduled_for_test(&ctx));
            std::thread::sleep(std::time::Duration::from_millis(20));
            drain_semantic_refresh_events(&ctx);

            assert!(matches!(
                request_rx.recv_timeout(std::time::Duration::from_secs(1)),
                Ok(SemanticRefreshRequest::Corpus)
            ));
        });
    }

    #[test]
    fn semantic_refresh_circuit_breaker_is_per_app_context() {
        let root_a = TempDir::new().unwrap();
        let root_b = TempDir::new().unwrap();
        let ctx_a = make_ctx_with_root(root_a.path());
        let ctx_b = make_ctx_with_root(root_b.path());

        for _ in 0..BREAKER_TRIP_THRESHOLD {
            record_semantic_refresh_transient_failure(&ctx_a);
        }

        assert!(semantic_refresh_circuit_is_open(&ctx_a));
        assert_eq!(
            semantic_refresh_transient_failure_count_for_test(&ctx_a),
            BREAKER_TRIP_THRESHOLD
        );
        assert!(!semantic_refresh_circuit_is_open(&ctx_b));
        assert_eq!(semantic_refresh_transient_failure_count_for_test(&ctx_b), 0);
        assert!(!semantic_refresh_probe_is_scheduled_for_test(&ctx_b));
    }

    #[test]
    fn semantic_refresh_success_resets_breaker_and_next_failure_starts_fresh() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let files = (0..(BREAKER_TRIP_THRESHOLD + 1))
            .map(|index| root.join(format!("reset{index}.rs")))
            .collect::<Vec<_>>();
        for file in &files {
            std::fs::write(file, "fn main() {}\n").unwrap();
        }

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, event_tx) = install_semantic_refresh_channels(&ctx);

            for file in files.iter().take(BREAKER_TRIP_THRESHOLD) {
                event_tx
                    .send(SemanticRefreshEvent::Failed {
                        paths: vec![file.clone()],
                        error: transient_embedding_error(),
                    })
                    .unwrap();
            }
            drain_semantic_refresh_events(&ctx);
            assert!(semantic_refresh_circuit_is_open(&ctx));

            // The pre-breaker failures each schedule a retry on an independent
            // thread with the same backoff, so their arrival order at the channel
            // is nondeterministic. Collect them and assert the set, not the order
            // (matches the sibling breaker tests).
            let mut retry_batches = Vec::new();
            for _ in 0..(BREAKER_TRIP_THRESHOLD - 1) {
                retry_batches.push(recv_files_request(&request_rx));
            }
            for file in files.iter().take(BREAKER_TRIP_THRESHOLD - 1) {
                assert!(
                    retry_batches.contains(&vec![file.clone()]),
                    "missing retry for {file:?}; got {retry_batches:?}"
                );
            }

            event_tx
                .send(SemanticRefreshEvent::Completed {
                    added_entries: Vec::new(),
                    updated_metadata: Vec::new(),
                    completed_paths: vec![files[BREAKER_TRIP_THRESHOLD - 1].clone()],
                })
                .unwrap();
            drain_semantic_refresh_events(&ctx);

            assert!(!semantic_refresh_circuit_is_open(&ctx));
            assert_eq!(semantic_refresh_transient_failure_count_for_test(&ctx), 0);

            let fresh_file = files[BREAKER_TRIP_THRESHOLD].clone();
            event_tx
                .send(SemanticRefreshEvent::Failed {
                    paths: vec![fresh_file.clone()],
                    error: transient_embedding_error(),
                })
                .unwrap();
            drain_semantic_refresh_events(&ctx);

            assert!(!semantic_refresh_circuit_is_open(&ctx));
            assert_eq!(semantic_refresh_transient_failure_count_for_test(&ctx), 1);
            assert_eq!(recv_files_request(&request_rx), vec![fresh_file]);
        });
    }

    #[test]
    fn semantic_refresh_retry_attempt_cap_stashes_path_without_hot_timer() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("cap.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        with_semantic_retry_backoff_ms(1, || {
            let ctx = make_ctx_with_root(&root);
            let (request_rx, _event_tx) = install_semantic_refresh_channels(&ctx);
            let error = transient_embedding_error();

            for _ in 0..MAX_RETRY_ATTEMPTS {
                assert!(schedule_semantic_refresh_retry(
                    &ctx,
                    vec![file.clone()],
                    &error
                ));
                assert_eq!(recv_files_request(&request_rx), vec![file.clone()]);
            }

            assert!(schedule_semantic_refresh_retry(
                &ctx,
                vec![file.clone()],
                &error
            ));
            assert!(request_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err());
            assert_eq!(ctx.take_pending_semantic_index_paths(), vec![file]);
        });
    }

    #[test]
    fn transient_corpus_failure_preserves_pending_semantic_paths() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("pending.vue");
        std::fs::write(&file, "<template />").unwrap();

        let ctx = make_ctx_with_root(&root);
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(root.clone(), 3));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "refreshing_corpus".into(),
            files: Some(1),
            entries_done: None,
            entries_total: None,
        };
        ctx.add_pending_semantic_index_paths(vec![file.clone()]);
        let (_request_rx, event_tx) = install_semantic_refresh_channels(&ctx);

        event_tx
            .send(SemanticRefreshEvent::CorpusFailed {
                error: format!(
                    "{}backend unavailable",
                    aft::semantic_index::TRANSIENT_EMBEDDING_MARKER
                ),
            })
            .unwrap();

        drain_semantic_refresh_events(&ctx);

        let pending = ctx.take_pending_semantic_index_paths();
        assert_eq!(pending, vec![file]);
        assert!(matches!(
            &*ctx
                .semantic_index_status()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SemanticIndexStatus::Ready { .. }
        ));
    }

    #[test]
    fn watcher_stale_mark_emits_status_without_semantic_change() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("notes.txt");
        std::fs::write(&file, "changed").unwrap();

        let ctx = make_ctx_with_root(&root);
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), false);
        let rx = status_frame_rx(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file)).unwrap();

        drain_watcher_events(&ctx);

        let snapshot = recv_status_changed(&rx);
        assert_eq!(
            snapshot["status_bar"]["tier2_stale"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn watcher_diagnostics_clear_emits_status_without_semantic_change() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let file = root.join("gone.txt");
        std::fs::write(&file, "deleted").unwrap();

        let ctx = make_ctx_with_root(&root);
        ctx.update_status_bar_tier2(Some(1), Some(2), Some(3), Some(4), true);
        {
            let key = ServerKey {
                kind: ServerKind::TypeScript,
                root: root.clone(),
            };
            let mut lsp = ctx.lsp();
            lsp.diagnostics_store_mut_for_test()
                .publish(key, file.clone(), vec![err_diag(&file)]);
        }
        assert_eq!(ctx.status_bar_counts().unwrap().errors, 1);

        std::fs::remove_file(&file).unwrap();
        let rx = status_frame_rx(&ctx);
        let watcher_tx = install_watcher_rx(&ctx);
        watcher_tx.send(watcher_paths_event(file)).unwrap();

        drain_watcher_events(&ctx);

        let snapshot = recv_status_changed(&rx);
        assert_eq!(snapshot["status_bar"]["errors"], serde_json::Value::from(0));
        assert_eq!(ctx.status_bar_counts().unwrap().errors, 0);
    }
}
