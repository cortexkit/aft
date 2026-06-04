mod cli;
use aft::bash_background::BgTaskRegistry;
use aft::config::Config;
use aft::context::{
    AppContext, SemanticIndexEvent, SemanticIndexStatus, SemanticRefreshEvent,
    SemanticRefreshRequest,
};
use aft::log_ctx;
use aft::lsp::client::LspEvent;
use aft::parser::TreeSitterProvider;
use aft::protocol::{EchoParams, PushFrame, RawRequest, Response};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    // Handle --version flag before anything else
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

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            use std::io::Write;
            let prefix = if record.target().starts_with("aft::lsp")
                || record.target().starts_with("aft_lsp")
            {
                "[aft-lsp]"
            } else {
                "[aft]"
            };
            writeln!(buf, "{} {}", prefix, record.args())
        })
        .init();

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

    aft::slog_info!("started, pid {}", std::process::id());

    let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
    install_signal_handler(ctx.bash_background().clone(), ctx.lsp_child_registry());

    // Install bash output-compression closure on the BgTaskRegistry. The
    // closure captures the shared filter-registry handle and the shared
    // compress-flag (atomic) so the watchdog thread can compress without
    // touching the rest of AppContext. The flag is updated from `configure`
    // when `experimental.bash.compress` changes; the filter registry is
    // updated when `reset_filter_registry` is called.
    {
        let filter_registry_handle = ctx.shared_filter_registry();
        let compress_flag = ctx.bash_compress_flag();
        ctx.bash_background()
            .set_compressor(move |command: &str, output: String| {
                if !compress_flag.load(std::sync::atomic::Ordering::Relaxed) {
                    return output;
                }
                let registry_guard = match filter_registry_handle.read() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                aft::compress::compress_with_registry(command, &output, &registry_guard)
            });
    }

    let stdout_writer = ctx.stdout_writer();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let shutdown_from_push = Arc::clone(&shutdown_requested);
    ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame: PushFrame| {
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
    let (line_tx, line_rx) = mpsc::channel::<io::Result<String>>();
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

        let line_result = match line_rx.recv_timeout(DRAIN_INTERVAL) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic drain so push frames flow even without requests.
                // Cheap on the idle path: each drain just checks try_recv
                // on a channel and bails if empty.
                drain_configure_warning_events(&ctx);
                drain_search_index_events(&ctx);
                drain_semantic_index_events(&ctx);
                drain_semantic_refresh_events(&ctx);
                drain_inspect_events(&ctx);
                drain_watcher_events(&ctx);
                drain_semantic_refresh_events(&ctx);
                drain_lsp_events(&ctx);
                if shutdown_requested.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
                drain_configure_warning_events(&ctx);
                drain_search_index_events(&ctx);
                drain_semantic_index_events(&ctx);
                drain_semantic_refresh_events(&ctx);
                drain_inspect_events(&ctx);
                drain_watcher_events(&ctx);
                drain_semantic_refresh_events(&ctx);
                drain_lsp_events(&ctx);
                let request_id = req.id.clone();
                let session_id = req.session().to_string();
                let command = req.command.clone();
                let session_id_for_log = req.session_id.clone();
                let dispatch_result = catch_unwind(AssertUnwindSafe(|| {
                    log_ctx::with_session(session_id_for_log, || dispatch(req, &ctx))
                }));
                match dispatch_result {
                    Ok(mut response) => {
                        attach_bg_completions(&mut response, &ctx, &session_id, &command);
                        attach_status_bar(&mut response, &ctx, &command);
                        response
                    }
                    Err(payload) => {
                        shutdown_after_response = true;
                        dispatch_panic_response(request_id, &command, payload.as_ref())
                    }
                }
            }
            Err(e) => {
                aft::slog_error!("parse error: {} — input: {}", e, trimmed);
                Response::error(
                    "_parse_error",
                    "parse_error",
                    format!("failed to parse request: {}", e),
                )
            }
        };

        if let Err(e) = write_response(&ctx, &response) {
            aft::slog_error!("stdout write error: {}", e);
            break;
        }
        drain_configure_warning_events(&ctx);
        if shutdown_after_response || shutdown_requested.load(Ordering::SeqCst) {
            break;
        }
    }

    ctx.lsp().shutdown_all();
    ctx.bash_background().detach();
    aft::slog_info!("stdin closed, shutting down");
}

#[cfg(unix)]
fn install_signal_handler(
    bg_registry: BgTaskRegistry,
    lsp_children: aft::lsp::child_registry::LspChildRegistry,
) {
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
            bg_registry.detach();
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

#[cfg(not(unix))]
static WINDOWS_SIGNAL_REGISTRIES: std::sync::OnceLock<(
    BgTaskRegistry,
    aft::lsp::child_registry::LspChildRegistry,
)> = std::sync::OnceLock::new();

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
        if let Some((bg_registry, lsp_children)) = WINDOWS_SIGNAL_REGISTRIES.get() {
            bg_registry.detach();
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
fn install_signal_handler(
    bg_registry: BgTaskRegistry,
    lsp_children: aft::lsp::child_registry::LspChildRegistry,
) {
    #[cfg(windows)]
    {
        let _ = WINDOWS_SIGNAL_REGISTRIES.set((bg_registry, lsp_children));
        // SAFETY: registers a process-global console-control callback. The
        // callback only uses cloneable registries stored in OnceLock.
        let ok = unsafe { SetConsoleCtrlHandler(Some(windows_console_handler), 1) };
        if ok == 0 {
            aft::slog_error!("failed to install Windows console control handler");
        }
    }

    #[cfg(not(windows))]
    {
        let _ = (bg_registry, lsp_children);
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

fn drain_configure_warning_events(ctx: &AppContext) {
    for (generation, frame) in ctx.drain_configure_warnings() {
        if ctx.configure_generation() != generation {
            aft::slog_info!(
                "dropping stale configure_warnings for generation {} (current {})",
                generation,
                ctx.configure_generation()
            );
            continue;
        }

        if let Some(sender) = ctx.progress_sender_handle() {
            sender(PushFrame::ConfigureWarnings(frame));
        }
    }
}

fn drain_inspect_events(ctx: &AppContext) {
    let drained = ctx.inspect_manager().drain_completions();
    // Watcher-driven Tier-2 scans complete via the reuse path, which bypasses
    // `result_rx`/`drain_completions`. Poll the manager's reuse counter so a
    // background scan still refreshes the bar (#3) — otherwise the counts and
    // `~` marker would only update on a manual `aft_inspect`.
    let reuse_completed = ctx.take_new_reuse_completions();
    // A completed background Tier-2 scan refreshes the agent status-bar counts
    // to the freshly-persisted aggregate, and clears the stale marker — so the
    // bar reflects the new numbers on the next tool result without waiting for
    // an explicit aft_inspect call.
    if drained > 0 || reuse_completed {
        if let Some(project_root) = ctx.config().project_root.clone() {
            let (dead_code, unused_exports, duplicates) = ctx
                .inspect_manager()
                .latest_tier2_counts(ctx.inspect_dir(), project_root);
            // Don't clear the `~` stale marker until the whole serial Tier-2
            // cycle has drained — while any category is still in flight the
            // already-persisted categories may predate the latest edit, so
            // claiming fresh would be premature (#20). `None` counts preserve
            // the last-known value rather than fabricating a `0` (#1).
            let stale = ctx.inspect_manager().tier2_any_in_flight();
            ctx.update_status_bar_tier2(dead_code, unused_exports, duplicates, None, stale);
        }
    }
}

fn attach_bg_completions(
    response: &mut Response,
    ctx: &AppContext,
    session_id: &str,
    command: &str,
) {
    if matches!(
        command,
        "configure"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    let completions = ctx
        .bash_background()
        .drain_completions_for_session(Some(session_id));
    if completions.is_empty() {
        return;
    }
    let value = serde_json::json!(completions);
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("bg_completions".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "bg_completions": value });
        }
    }
}

/// Attach the agent status-bar counts to the response envelope so the plugin
/// after-hook can surface the IDE-style status bar (emit-on-change). Skips
/// internal/transport commands that don't represent agent tool calls (their
/// responses never reach the agent, and bash-lifecycle commands fire rapidly).
/// `errors`/`warnings` are read live from the LSP store here; Tier-2/todos are
/// last-known. Omitted entirely until the Tier-2 cache is populated once.
fn attach_status_bar(response: &mut Response, ctx: &AppContext, command: &str) {
    if matches!(
        command,
        "configure"
            | "ping"
            | "version"
            | "status"
            | "bash_status"
            | "bash_write"
            | "bash_promote"
            | "bash_drain_completions"
            | "bash_notify"
            | "bash_unnotify"
            | "bash_ack_completions"
    ) {
        return;
    }
    let Some(counts) = ctx.status_bar_counts() else {
        return;
    };
    let value = serde_json::json!({
        "errors": counts.errors,
        "warnings": counts.warnings,
        "dead_code": counts.dead_code,
        "unused_exports": counts.unused_exports,
        "duplicates": counts.duplicates,
        "todos": counts.todos,
        "tier2_stale": counts.tier2_stale,
    });
    match response.data.as_object_mut() {
        Some(data) => {
            data.insert("status_bar".to_string(), value);
        }
        None => {
            response.data = serde_json::json!({ "status_bar": value });
        }
    }
}

fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {
    match req.command.as_str() {
        "ping" => Response::success(&req.id, serde_json::json!({ "command": "pong" })),
        "version" => Response::success(
            &req.id,
            serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
        ),
        "echo" => handle_echo(&req),
        "bash" => aft::commands::bash::handle(&req, ctx),
        "bash_drain_completions" => aft::commands::bash_drain_completions::handle(&req, ctx),
        "bash_ack_completions" => aft::commands::bash_drain_completions::handle_ack(&req, ctx),
        "bash_status" => aft::commands::bash_status::handle(&req, ctx),
        "bash_notify" => aft::commands::bash_notify::handle(&req, ctx),
        "bash_unnotify" => aft::commands::bash_notify::handle_unnotify(&req, ctx),
        "bash_promote" => aft::commands::bash_promote::handle(&req, ctx),
        "bash_kill" => aft::commands::bash_kill::handle(&req, ctx),
        "bash_write" => aft::commands::bash_write::handle(&req, ctx),
        "db_get_state" => aft::commands::state::handle_db_get_state(&req, ctx),
        "db_set_state" => aft::commands::state::handle_db_set_state(&req, ctx),
        "db_get_host_state" => aft::commands::state::handle_db_get_host_state(&req, ctx),
        "db_set_host_state" => aft::commands::state::handle_db_set_host_state(&req, ctx),
        "outline" => aft::commands::outline::handle_outline(&req, ctx),
        "zoom" => aft::commands::zoom::handle_zoom(&req, ctx),
        "read" => aft::commands::read::handle_read(&req, ctx),
        "undo" => aft::commands::undo::handle_undo(&req, ctx),
        "undo_preview" => aft::commands::undo::handle_undo_preview(&req, ctx),
        "edit_history" => aft::commands::edit_history::handle_edit_history(&req, ctx),
        "checkpoint" => aft::commands::checkpoint::handle_checkpoint(&req, ctx),
        "checkpoint_paths" => aft::commands::checkpoint::handle_checkpoint_paths(&req, ctx),
        "restore_checkpoint" => {
            aft::commands::restore_checkpoint::handle_restore_checkpoint(&req, ctx)
        }
        "list_checkpoints" => aft::commands::list_checkpoints::handle_list_checkpoints(&req, ctx),
        "write" => aft::commands::write::handle_write(&req, ctx),
        "delete_file" => aft::commands::delete_file::handle_delete_file(&req, ctx),
        "move_file" => aft::commands::move_file::handle_move_file(&req, ctx),
        "edit_symbol" => aft::commands::edit_symbol::handle_edit_symbol(&req, ctx),
        "edit_match" => aft::commands::edit_match::handle_edit_match(&req, ctx),
        "batch" => aft::commands::batch::handle_batch(&req, ctx),
        "transaction" => aft::commands::transaction::handle_transaction(&req, ctx),
        "add_import" => aft::commands::add_import::handle_add_import(&req, ctx),
        "add_member" => aft::commands::add_member::handle_add_member(&req, ctx),
        "add_derive" => aft::commands::add_derive::handle_add_derive(&req, ctx),
        "add_decorator" => aft::commands::add_decorator::handle_add_decorator(&req, ctx),
        "add_struct_tags" => aft::commands::add_struct_tags::handle_add_struct_tags(&req, ctx),
        "wrap_try_catch" => aft::commands::wrap_try_catch::handle_wrap_try_catch(&req, ctx),
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
        drain_search_index_events(ctx);
        drain_semantic_index_events(ctx);

        match ctx.semantic_index_status().borrow().clone() {
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
    let mut backup = ctx.backup().borrow_mut();

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

/// Source file extensions that the call graph supports.
const SOURCE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "py", "pyi", "rs", "go",
];

/// Drain pending file watcher events and invalidate changed source files
/// in the call graph.
///
/// Decide whether a `notify::Event` represents a real content change worth
/// invalidating cached state for. Pulled out as a free function so unit
/// tests can exercise every notify event variant without setting up a
/// watcher pipeline.
///
/// The filter rejects:
/// - `Access(_)` (read syscalls; cause feedback loops on atime)
/// - `Modify(Metadata(AccessTime|Permissions|Ownership|Extended))`
///   (no content change — biome-lint reproducer)
/// - Anything that's not Create/Remove/Modify
///
/// And accepts:
/// - `Create(_)`, `Remove(_)`, `Modify(Name(_))` (rename)
/// - `Modify(Data(_))`, `Modify(Other)`, `Modify(Any)`
/// - `Modify(Metadata(WriteTime|Any|Other))` (real or unknown content change)
pub(crate) fn watcher_event_invalidates(kind: &notify::EventKind) -> bool {
    use notify::event::{MetadataKind, ModifyKind};
    use notify::EventKind;
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(meta)) => !matches!(
            meta,
            MetadataKind::AccessTime
                | MetadataKind::Permissions
                | MetadataKind::Ownership
                | MetadataKind::Extended
        ),
        EventKind::Modify(_) => true,
        _ => false,
    }
}

fn watcher_path_is_infra_skip(path: &std::path::Path) -> bool {
    use std::path::Component;
    path.components().any(|c| {
        matches!(c, Component::Normal(name) if matches!(
            name.to_str().unwrap_or(""),
            ".git" | ".opencode" | ".alfonso" | ".gsd" | "node_modules" | "target"
        ))
    })
}

fn watcher_path_is_ignore_file(path: &std::path::Path) -> bool {
    path.file_name()
        .map(|n| n == ".gitignore" || n == ".aftignore")
        .unwrap_or(false)
}

fn watcher_path_is_source(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext))
}

fn watcher_project_root(ctx: &AppContext) -> Option<std::path::PathBuf> {
    let configured_root = ctx.config().project_root.clone();
    ctx.canonical_cache_root_opt()
        .or_else(|| configured_root.map(canonicalize_watcher_path))
}

fn watcher_same_path(path: &std::path::Path, target: &std::path::Path) -> bool {
    if path == target {
        return true;
    }

    std::fs::canonicalize(target)
        .map(|target| path == target)
        .unwrap_or(false)
}

fn watcher_git_info_exclude_path(
    ctx: &AppContext,
    project_root: &std::path::Path,
) -> std::path::PathBuf {
    ctx.git_common_dir()
        .unwrap_or_else(|| project_root.join(".git"))
        .join("info")
        .join("exclude")
}

fn watcher_path_is_git_info_exclude(
    ctx: &AppContext,
    project_root: &std::path::Path,
    path: &std::path::Path,
) -> bool {
    watcher_same_path(path, &watcher_git_info_exclude_path(ctx, project_root))
}

fn watcher_path_is_global_gitignore(path: &std::path::Path) -> bool {
    ignore::gitignore::gitconfig_excludes_path()
        .as_deref()
        .is_some_and(|global_ignore| watcher_same_path(path, global_ignore))
}

fn watcher_path_can_change_corpus_ignore(
    ctx: &AppContext,
    project_root: Option<&std::path::Path>,
    path: &std::path::Path,
) -> bool {
    if watcher_path_is_global_gitignore(path) {
        return true;
    }
    if let Some(project_root) = project_root {
        if watcher_path_is_git_info_exclude(ctx, project_root, path) {
            return true;
        }
    }

    let Some(project_root) = project_root else {
        return false;
    };
    if !path.starts_with(project_root) {
        return false;
    }

    watcher_path_is_ignore_file(path) && !watcher_path_is_infra_skip(path)
}

fn canonicalize_watcher_path(path: std::path::PathBuf) -> std::path::PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        return canonical;
    }

    let parent = path.parent().map(std::path::Path::to_path_buf);
    let file_name = path.file_name().map(std::ffi::OsStr::to_os_string);
    match (parent, file_name) {
        (Some(parent), Some(file_name)) => std::fs::canonicalize(parent)
            .map(|canonical_parent| canonical_parent.join(file_name))
            .unwrap_or(path),
        _ => path,
    }
}

struct FilteredWatcherPaths {
    changed: HashSet<std::path::PathBuf>,
    ignore_file_changed: bool,
}

fn filter_watcher_raw_paths<I>(ctx: &AppContext, raw_paths: I) -> FilteredWatcherPaths
where
    I: IntoIterator<Item = std::path::PathBuf>,
{
    let raw_paths: Vec<std::path::PathBuf> = raw_paths
        .into_iter()
        .map(canonicalize_watcher_path)
        .collect();
    let project_root = watcher_project_root(ctx);

    // If any corpus-affecting ignore file changed, rebuild the matcher before
    // filtering this same batch so sibling events are checked against fresh
    // rules. The caller also needs this fact even if the ignore file itself is
    // filtered out: changing ignore rules changes the corpus shape, not just a
    // single path. Infra ignore files (for example, node_modules/.gitignore) do
    // not affect AFT's project corpus and should not trigger a corpus refresh.
    let ignore_file_changed = raw_paths
        .iter()
        .any(|path| watcher_path_can_change_corpus_ignore(ctx, project_root.as_deref(), path));
    if ignore_file_changed {
        log::debug!("watcher: project ignore file changed, rebuilding matcher before filter");
        ctx.rebuild_gitignore();
    }

    let changed = raw_paths
        .into_iter()
        .filter(|path| {
            if watcher_path_is_infra_skip(path) {
                return false;
            }

            if watcher_path_is_ignored_by_current_matcher(ctx, path) {
                return false;
            }
            true
        })
        .collect();

    FilteredWatcherPaths {
        changed,
        ignore_file_changed,
    }
}

fn semantic_project_files_for_refresh(
    root: &std::path::Path,
    max_files: usize,
) -> Result<Vec<std::path::PathBuf>, usize> {
    aft::search_index::walk_project_files_bounded_default_matching(
        root,
        max_files,
        aft::semantic_index::is_semantic_indexed_extension,
    )
}

fn watcher_path_is_ignored_by_current_matcher(ctx: &AppContext, path: &std::path::Path) -> bool {
    if watcher_path_is_infra_skip(path) {
        return true;
    }

    if let Some(matcher) = ctx.gitignore() {
        if path.starts_with(matcher.path()) {
            let is_dir = path.is_dir();
            return matcher
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore();
        }
    }

    false
}

fn replay_search_index_pending_updates(
    ctx: &AppContext,
    index: &mut aft::search_index::SearchIndex,
    pending_paths: Vec<std::path::PathBuf>,
) {
    for path in pending_paths {
        if path.exists() {
            if watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                index.remove_file(&path);
            } else {
                index.update_file(&path);
            }
        } else {
            index.remove_file(&path);
        }
    }
}

fn semantic_corpus_refresh_in_progress(ctx: &AppContext) -> bool {
    matches!(
        &*ctx.semantic_index_status().borrow(),
        SemanticIndexStatus::Building { stage, .. } if stage == "refreshing_corpus"
    )
}

#[cfg(debug_assertions)]
fn delay_search_rebuild_publish_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SEARCH_REBUILD_PUBLISH_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

#[cfg(not(debug_assertions))]
fn delay_search_rebuild_publish_for_debug() {}

fn spawn_search_corpus_refresh(
    ctx: &AppContext,
    root: std::path::PathBuf,
    config: aft::config::Config,
) {
    if let Some(index) = ctx.search_index().borrow_mut().as_mut() {
        index.ready = false;
    }

    let (tx, rx): (
        crossbeam_channel::Sender<aft::search_index::SearchIndex>,
        crossbeam_channel::Receiver<aft::search_index::SearchIndex>,
    ) = crossbeam_channel::unbounded();
    *ctx.search_index_rx().borrow_mut() = Some(rx);
    ctx.reset_symbol_cache();

    let is_worktree_bridge = ctx.is_worktree_bridge();
    let session_id = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            let cache_dir =
                aft::search_index::resolve_cache_dir(&root, config.storage_dir.as_deref());
            let _cache_lock = if is_worktree_bridge {
                None
            } else {
                match aft::search_index::CacheLock::acquire(&cache_dir) {
                    Ok(lock) => Some(lock),
                    Err(error) => {
                        aft::slog_warn!(
                            "failed to acquire search cache lock for ignore refresh: {}",
                            error
                        );
                        None
                    }
                }
            };
            let index = aft::search_index::SearchIndex::build_with_limit(
                &root,
                config.search_index_max_file_size,
            );
            delay_search_rebuild_publish_for_debug();
            if !is_worktree_bridge {
                index.write_to_disk(&cache_dir, index.stored_git_head());
            }
            let _ = tx.send(index);
        });
    });
}

fn refresh_corpus_after_ignore_change(ctx: &AppContext) {
    let Some(root) = ctx.canonical_cache_root_opt() else {
        return;
    };
    let config = ctx.config().clone();
    let mut status_changed = false;

    if let Some(graph) = ctx.callgraph().borrow_mut().as_mut() {
        graph.invalidate_file(&root.join(".gitignore"));
        graph.invalidate_file(&root.join(".aftignore"));
    }

    if config.search_index {
        spawn_search_corpus_refresh(ctx, root.clone(), config.clone());
        status_changed = true;
        aft::slog_info!("started search index refresh after ignore-rule change");
    }

    if config.semantic_search {
        match semantic_project_files_for_refresh(&root, config.semantic.max_files) {
            Ok(current_files) => {
                if let Some(sender) = ctx.semantic_refresh_sender() {
                    let file_count = current_files.len();
                    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                        stage: "refreshing_corpus".to_string(),
                        files: Some(file_count),
                        entries_done: None,
                        entries_total: None,
                        stats: None,
                    };
                    match sender.send(SemanticRefreshRequest::Corpus { current_files }) {
                        Ok(()) => {
                            status_changed = true;
                        }
                        Err(error) => {
                            *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(
                                format!("semantic corpus refresh worker unavailable: {error}"),
                            );
                            status_changed = true;
                        }
                    }
                } else if ctx.semantic_index_rx().borrow().is_some() {
                    ctx.mark_pending_semantic_corpus_refresh();
                }
            }
            Err(_) => {
                ctx.clear_semantic_refresh_worker();
                *ctx.semantic_index().borrow_mut() = None;
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(format!(
                    "too many files (>{}) for semantic indexing (max {})",
                    config.semantic.max_files, config.semantic.max_files
                ));
                status_changed = true;
            }
        }
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

/// Borrows the watcher receiver and callgraph in separate phases to avoid
/// RefCell borrow conflicts. Events are deduplicated by PathBuf — notify
/// fires multiple events per file write (Create, Modify, etc.).
fn drain_watcher_events(ctx: &AppContext) {
    // Phase 1: collect changed paths from the receiver without applying the
    // gitignore matcher yet; .gitignore writes in this same batch must rebuild
    // the matcher before any sibling path is filtered.
    let filtered = {
        let rx_ref = ctx.watcher_rx().borrow();
        let rx = match rx_ref.as_ref() {
            Some(rx) => rx,
            None => {
                ctx.tick_tier2_refresh_scheduler(0);
                return; // No watcher configured
            }
        };

        let mut raw_paths = Vec::new();
        while let Ok(event_result) = rx.try_recv() {
            if let Ok(event) = event_result {
                // Only process events that indicate actual file content changes.
                //
                // Skip Access events — on Linux with atime enabled, reading a file
                // during update_file triggers an access event, creating a feedback
                // loop.
                //
                // Skip Modify(Metadata(...)) events that don't imply content
                // changes: AccessTime, Permissions, Ownership, Extended.
                // The biome-lint case is the canonical reproducer — running
                // `biome check` opens every TS file for read, which on Linux
                // (and on macOS in some configurations) updates atime and fires
                // notify `Modify(Metadata(AccessTime))` events. Without this
                // filter, every read-only lint pass invalidates the entire
                // symbol cache, search index, and semantic index — completely
                // unnecessary work.
                //
                // We KEEP `Modify(Metadata(WriteTime))` because mtime change
                // does indicate a real content modification on every supported
                // platform. We KEEP `Modify(Metadata(Any))` and
                // `Modify(Metadata(Other))` as catch-all "we can't tell what
                // metadata changed" cases — better to over-invalidate than to
                // miss a real edit.
                if !watcher_event_invalidates(&event.kind) {
                    continue;
                }
                for path in event.paths {
                    raw_paths.push(path);
                }
            }
        }
        filter_watcher_raw_paths(ctx, raw_paths)
    }; // receiver borrow dropped here

    let ignore_file_changed = filtered.ignore_file_changed;
    if ignore_file_changed {
        refresh_corpus_after_ignore_change(ctx);
    }

    let changed = filtered.changed;
    let scheduler_changed_path_count = if ignore_file_changed {
        changed.len().max(1)
    } else {
        changed.len()
    };
    if changed.is_empty() {
        ctx.tick_tier2_refresh_scheduler(scheduler_changed_path_count);
        return;
    }

    // A real source change makes the last-known Tier-2 counts stale until the
    // next background scan reconciles them — surface that in the status bar
    // immediately (the `~` marker) so the agent never reads them as live.
    ctx.mark_status_bar_tier2_stale();

    if ctx.search_index_rx().borrow().is_some() {
        ctx.add_pending_search_index_paths(changed.iter().cloned());
    }
    let semantic_source_paths = changed
        .iter()
        .filter(|path| watcher_path_is_source(path))
        .cloned()
        .collect::<Vec<_>>();
    let semantic_build_in_progress = ctx.semantic_index_rx().borrow().is_some();
    let semantic_corpus_refresh_in_progress = semantic_corpus_refresh_in_progress(ctx);
    if (semantic_build_in_progress || semantic_corpus_refresh_in_progress)
        && !semantic_source_paths.is_empty()
    {
        ctx.add_pending_semantic_index_paths(semantic_source_paths.clone());
    }

    if let Ok(mut symbol_cache) = ctx.symbol_cache().write() {
        for path in &changed {
            symbol_cache.invalidate(path);
        }
    }

    // Phase 2: invalidate each changed file in the call graph
    let mut graph_ref = ctx.callgraph().borrow_mut();
    if let Some(graph) = graph_ref.as_mut() {
        for path in &changed {
            if watcher_path_is_source(path) {
                graph.invalidate_file(path);
            }
        }
    }

    let mut index_ref = ctx.search_index().borrow_mut();
    if let Some(index) = index_ref.as_mut() {
        for path in &changed {
            if path.exists() {
                index.update_file(path);
            } else {
                index.remove_file(path);
            }
        }
    }

    let mut semantic_index_ref = ctx.semantic_index().borrow_mut();
    let mut semantic_status_changed = false;
    let mut semantic_refresh_paths = Vec::new();
    if let Some(index) = semantic_index_ref.as_mut() {
        let mut stale_paths = Vec::new();
        for path in &semantic_source_paths {
            index.invalidate_file(path);
            stale_paths.push(path.clone());
        }
        if !stale_paths.is_empty() {
            let mut status = ctx.semantic_index_status().borrow_mut();
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &stale_paths {
                    status.add_refreshing_file(path.clone());
                }
                semantic_refresh_paths = stale_paths;
                semantic_status_changed = true;
            }
        }
    }

    drop(semantic_index_ref);
    drop(index_ref);

    // A vanished file's LSP diagnostics would otherwise linger in the warm set
    // forever (no server republishes for a path that no longer exists),
    // inflating the error/warning counts in the status bar and `aft_inspect`.
    // Clear them here so every deletion source is covered (AFT delete, `rm`,
    // `git checkout`, branch switch) — not just the delete command. The agent
    // status bar reads E/W live from the warm set on each response, so clearing
    // the store is sufficient; the next tool call's bar reflects the new count.
    //
    // Not gated on the trigram `SOURCE_EXTENSIONS` set: any registered LSP
    // server (Bash, YAML, Solidity, Vue, C/C++, custom servers, …) can publish
    // diagnostics for files outside that set, and gating on it left their
    // diagnostics stranded after deletion. `clear_for_file` is a cheap no-op
    // when the store holds nothing for the path, so clearing unconditionally
    // for every vanished path is safe.
    for path in &changed {
        if !path.exists() {
            ctx.lsp_clear_diagnostics_for_file(path);
        }
    }

    if !semantic_refresh_paths.is_empty() {
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: semantic_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} refreshing file(s)",
                semantic_refresh_paths.len()
            );
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &semantic_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            semantic_status_changed = true;
        }
    }

    aft::slog_info!("invalidated {} files", changed.len());
    if semantic_status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
    ctx.tick_tier2_refresh_scheduler(scheduler_changed_path_count);
}

fn drain_search_index_events(ctx: &AppContext) {
    let (latest, disconnected) = {
        let rx_ref = ctx.search_index_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(index) => latest = Some(index),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        (latest, disconnected)
    };

    let mut status_changed = false;
    let mut installed_index = false;
    if let Some(mut index) = latest {
        let pending_paths = ctx.take_pending_search_index_paths();
        if !pending_paths.is_empty() {
            replay_search_index_pending_updates(ctx, &mut index, pending_paths);
        }
        *ctx.search_index().borrow_mut() = Some(index);
        installed_index = true;
        status_changed = true;
    }

    if disconnected || installed_index {
        *ctx.search_index_rx().borrow_mut() = None;
        if disconnected && !installed_index {
            let _ = ctx.take_pending_search_index_paths();
        }
        status_changed = true;
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn drain_semantic_index_events(ctx: &AppContext) {
    let events = {
        let rx_ref = ctx.semantic_index_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    };

    if events.is_empty() {
        return;
    }

    let mut keep_receiver = true;
    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    let mut replay_corpus_refresh = false;
    for event in events {
        match event {
            SemanticIndexEvent::Progress {
                stage,
                files,
                entries_done,
                entries_total,
                stats,
            } => {
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                    stage,
                    files,
                    entries_done,
                    entries_total,
                    stats,
                };
                // Push progress to the sidebar. Without this, a long rebuild
                // (e.g. a slow local embedding backend re-indexing after a prior
                // failure) leaves the sidebar showing the stale prior state —
                // "failed" with an old error — for the entire build, even though
                // it is actively embedding. Progress transitions are exactly
                // when the user needs to see "building".
                status_changed = true;
            }
            SemanticIndexEvent::Ready(mut index) => {
                let pending_paths = ctx.take_pending_semantic_index_paths();
                for path in pending_paths {
                    if watcher_path_is_source(&path) {
                        index.invalidate_file(&path);
                        replay_refresh_paths.push(path);
                    }
                }
                replay_corpus_refresh = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index().borrow_mut() = Some(index);
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
                keep_receiver = false;
                status_changed = true;
            }
            SemanticIndexEvent::Failed(error) => {
                let _ = ctx.take_pending_semantic_index_paths();
                let _ = ctx.take_pending_semantic_corpus_refresh();
                *ctx.semantic_index().borrow_mut() = None;
                ctx.clear_semantic_refresh_worker();
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(error);
                keep_receiver = false;
                status_changed = true;
            }
        }
    }

    if !keep_receiver {
        *ctx.semantic_index_rx().borrow_mut() = None;
    }

    if replay_corpus_refresh {
        if let Some(root) = ctx.canonical_cache_root_opt() {
            let config = ctx.config().clone();
            match semantic_project_files_for_refresh(&root, config.semantic.max_files) {
                Ok(current_files) => {
                    let file_count = current_files.len();
                    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                        stage: "refreshing_corpus".to_string(),
                        files: Some(file_count),
                        entries_done: None,
                        entries_total: None,
                        stats: None,
                    };
                    let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
                        sender
                            .send(SemanticRefreshRequest::Corpus { current_files })
                            .is_ok()
                    });
                    if !sent {
                        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(
                            "semantic corpus refresh worker unavailable".to_string(),
                        );
                    }
                    status_changed = true;
                }
                Err(_) => {
                    ctx.clear_semantic_refresh_worker();
                    *ctx.semantic_index().borrow_mut() = None;
                    *ctx.semantic_index_status().borrow_mut() =
                        SemanticIndexStatus::Failed(format!(
                            "too many files (>{}) for semantic indexing (max {})",
                            config.semantic.max_files, config.semantic.max_files
                        ));
                    status_changed = true;
                }
            }
        }
    } else if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx.semantic_index_status().borrow_mut();
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn drain_semantic_refresh_events(ctx: &AppContext) {
    let events = {
        let rx_ref = ctx.semantic_refresh_event_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    };

    if events.is_empty() {
        return;
    }

    let mut status_changed = false;
    let mut replay_refresh_paths = Vec::new();
    for event in events {
        match event {
            SemanticRefreshEvent::Started { paths } => {
                let mut status = ctx.semantic_index_status().borrow_mut();
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in paths {
                        status.start_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::Completed {
                added_entries,
                updated_metadata,
                completed_paths,
            } => {
                if let Some(index) = ctx.semantic_index().borrow_mut().as_mut() {
                    index.apply_refresh_update(added_entries, updated_metadata, &completed_paths);
                }
                let mut status = ctx.semantic_index_status().borrow_mut();
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in &completed_paths {
                        status.complete_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusCompleted {
                mut index,
                changed,
                added,
                deleted,
                total_processed,
            } => {
                if changed > 0 || added > 0 || deleted > 0 {
                    aft::slog_info!(
                        "semantic corpus refresh completed: {} changed, {} new, {} deleted, {} total processed",
                        changed,
                        added,
                        deleted,
                        total_processed
                    );
                }
                let pending_paths = ctx.take_pending_semantic_index_paths();
                for path in pending_paths {
                    if !watcher_path_is_source(&path) {
                        continue;
                    }
                    index.invalidate_file(&path);
                    if !watcher_path_is_ignored_by_current_matcher(ctx, &path) {
                        replay_refresh_paths.push(path);
                    }
                }
                *ctx.semantic_index().borrow_mut() = Some(index);
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::ready();
                status_changed = true;
            }
            SemanticRefreshEvent::Failed { paths, error } => {
                aft::slog_warn!("semantic refresh failed: {}", error);
                let mut status = ctx.semantic_index_status().borrow_mut();
                if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                    for path in &paths {
                        status.complete_refreshing_file(path);
                    }
                    status_changed = true;
                }
            }
            SemanticRefreshEvent::CorpusFailed { error } => {
                aft::slog_warn!("semantic corpus refresh failed: {}", error);
                let _ = ctx.take_pending_semantic_index_paths();
                *ctx.semantic_index().borrow_mut() = None;
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(error);
                status_changed = true;
            }
        }
    }

    if !replay_refresh_paths.is_empty() {
        {
            let mut status = ctx.semantic_index_status().borrow_mut();
            if matches!(&*status, SemanticIndexStatus::Ready { .. }) {
                for path in &replay_refresh_paths {
                    status.add_refreshing_file(path.clone());
                }
                status_changed = true;
            }
        }
        let sent = ctx.semantic_refresh_sender().is_some_and(|sender| {
            sender
                .send(SemanticRefreshRequest::Files {
                    paths: replay_refresh_paths.clone(),
                })
                .is_ok()
        });
        if !sent {
            aft::slog_warn!(
                "semantic refresh worker unavailable; dropping {} replayed corpus file(s)",
                replay_refresh_paths.len()
            );
            let mut status = ctx.semantic_index_status().borrow_mut();
            for path in &replay_refresh_paths {
                status.cancel_refreshing_file(path);
            }
            status_changed = true;
        }
    }

    if status_changed {
        ctx.status_emitter().signal(ctx.build_status_snapshot());
    }
}

fn drain_lsp_events(ctx: &AppContext) {
    let events = {
        let mut lsp = ctx.lsp();
        lsp.drain_events()
    };
    for event in events {
        match event {
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] notification {:?} {} {} {}",
                    server_kind,
                    root.display(),
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerRequest {
                server_kind,
                root,
                id,
                method,
                params,
            } => {
                log::debug!(
                    "[aft-lsp] request {:?} {} {:?} {} {}",
                    server_kind,
                    root.display(),
                    id,
                    method,
                    params.unwrap_or(serde_json::Value::Null)
                );
            }
            LspEvent::ServerExited { server_kind, root } => {
                aft::slog_info!("exited {:?} {}", server_kind, root.display());
                ctx.status_emitter().signal(ctx.build_status_snapshot());
            }
        }
    }
}

#[cfg(test)]
mod watcher_filter_tests {
    use super::{
        dispatch_panic_response, drain_configure_warning_events, filter_watcher_raw_paths,
        watcher_event_invalidates, write_push_frame_or_request_shutdown,
    };
    use aft::config::Config;
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
    use aft::protocol::{ConfigureWarningsFrame, PushFrame};
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind,
        RenameMode,
    };
    use notify::EventKind;
    use tempfile::TempDir;

    fn make_ctx_with_root(root: &std::path::Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    /// Run `f` with global git-ignore discovery neutralized.
    ///
    /// `rebuild_gitignore` loads git's global excludes (the `ignore` crate
    /// resolves `$XDG_CONFIG_HOME/git/ignore`, falling back to
    /// `$HOME/.config/git/ignore`). A developer machine commonly has that file,
    /// so any "no project ignore → None" baseline is only deterministic when
    /// global discovery is pointed at an empty directory. Pointing
    /// `XDG_CONFIG_HOME` at a fresh tempdir does that without touching `HOME`.
    /// Serialized by a process-local mutex; env is restored before use.
    fn with_neutralized_global_gitignore<R>(f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized by LOCK above; restored immediately after `f`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        match result {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
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
        let frame = PushFrame::ConfigureWarnings(ConfigureWarningsFrame::new(
            "/repo",
            0,
            false,
            5_000,
            Vec::new(),
        ));

        write_push_frame_or_request_shutdown(&mut BrokenWriter, &frame, &shutdown);

        assert!(shutdown.load(std::sync::atomic::Ordering::SeqCst));
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
                ConfigureWarningsFrame::new("/stale", 1, false, 5_000, Vec::new()),
            ))
            .unwrap();
        warnings_tx
            .send((
                current_generation,
                ConfigureWarningsFrame::new("/current", 2, false, 5_000, Vec::new()),
            ))
            .unwrap();

        drain_configure_warning_events(&ctx);

        let frame = frame_rx.try_recv().expect("current warning frame");
        match frame {
            PushFrame::ConfigureWarnings(frame) => {
                assert_eq!(frame.project_root, "/current");
                assert_eq!(frame.source_file_count, 2);
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
}
