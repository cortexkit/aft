use aft::bash_background::BgTaskRegistry;
use aft::config::Config;
use aft::context::{AppContext, SemanticIndexEvent, SemanticIndexStatus};
use aft::log_ctx;
use aft::lsp::client::LspEvent;
use aft::parser::TreeSitterProvider;
use aft::protocol::{EchoParams, PushFrame, RawRequest, Response};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
    ctx.set_progress_sender(Some(std::sync::Arc::new(Box::new(
        move |frame: PushFrame| {
            let Ok(mut writer) = stdout_writer.lock() else {
                aft::slog_error!("stdout push frame lock poisoned");
                return;
            };
            if let Err(e) = write_push_frame(&mut *writer, &frame) {
                aft::slog_error!("stdout push frame write error: {}", e);
            }
        },
    ))));

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
        let line_result = match line_rx.recv_timeout(DRAIN_INTERVAL) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic drain so push frames flow even without requests.
                // Cheap on the idle path: each drain just checks try_recv
                // on a channel and bails if empty.
                drain_search_index_events(&ctx);
                drain_semantic_index_events(&ctx);
                drain_watcher_events(&ctx);
                drain_lsp_events(&ctx);
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

        let response = match serde_json::from_str::<RawRequest>(trimmed) {
            Ok(req) => {
                // Drain search index FIRST so watcher events apply to the latest index.
                // If reversed, watcher updates applied to the old index would be lost
                // when the background-built index replaces it.
                drain_search_index_events(&ctx);
                drain_semantic_index_events(&ctx);
                drain_watcher_events(&ctx);
                drain_lsp_events(&ctx);
                let session_id = req.session().to_string();
                let command = req.command.clone();
                let session_id_for_log = req.session_id.clone();
                // For `configure`, the response frame must be the first frame a
                // bridge observes for that request/session. The deferred
                // configure_warnings worker in configure.rs deliberately waits
                // until after dispatch returns so clients can register their
                // configured state before processing async warning pushes.
                let mut response =
                    log_ctx::with_session(session_id_for_log, || dispatch(req, &ctx));
                attach_bg_completions(&mut response, &ctx, &session_id, &command);
                response
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
            | "bash_promote"
            | "bash_drain_completions"
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
        "bash_promote" => aft::commands::bash_promote::handle(&req, ctx),
        "bash_kill" => aft::commands::bash_kill::handle(&req, ctx),
        "db_get_state" => aft::commands::state::handle_db_get_state(&req, ctx),
        "db_set_state" => aft::commands::state::handle_db_set_state(&req, ctx),
        "db_get_host_state" => aft::commands::state::handle_db_get_host_state(&req, ctx),
        "db_set_host_state" => aft::commands::state::handle_db_set_host_state(&req, ctx),
        "outline" => aft::commands::outline::handle_outline(&req, ctx),
        "zoom" => aft::commands::zoom::handle_zoom(&req, ctx),
        "read" => aft::commands::read::handle_read(&req, ctx),
        "undo" => aft::commands::undo::handle_undo(&req, ctx),
        "edit_history" => aft::commands::edit_history::handle_edit_history(&req, ctx),
        "checkpoint" => aft::commands::checkpoint::handle_checkpoint(&req, ctx),
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
        "semantic_search" => aft::commands::semantic_search::handle_semantic_search(&req, ctx),
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
        "impact" => aft::commands::impact::handle_impact(&req, ctx),
        "trace_data" => aft::commands::trace_data::handle_trace_data(&req, ctx),
        "move_symbol" => aft::commands::move_symbol::handle_move_symbol(&req, ctx),
        "extract_function" => aft::commands::extract_function::handle_extract_function(&req, ctx),
        "inline_symbol" => aft::commands::inline_symbol::handle_inline_symbol(&req, ctx),
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
const SOURCE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "py", "rs", "go"];

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

fn watcher_path_is_gitignore(path: &std::path::Path) -> bool {
    path.file_name().map(|n| n == ".gitignore").unwrap_or(false)
}

fn filter_watcher_raw_paths<I>(ctx: &AppContext, raw_paths: I) -> HashSet<std::path::PathBuf>
where
    I: IntoIterator<Item = std::path::PathBuf>,
{
    let raw_paths: Vec<std::path::PathBuf> = raw_paths.into_iter().collect();

    // If any .gitignore file changed, rebuild the matcher before filtering
    // this same batch so sibling events are checked against fresh rules.
    if raw_paths.iter().any(|path| watcher_path_is_gitignore(path)) {
        log::debug!("watcher: .gitignore changed, rebuilding matcher before filter");
        ctx.rebuild_gitignore();
    }

    raw_paths
        .into_iter()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .filter(|path| {
            if watcher_path_is_infra_skip(path) {
                return false;
            }

            if let Some(matcher) = ctx.gitignore() {
                if path.starts_with(matcher.path()) {
                    let is_dir = path.is_dir();
                    if matcher
                        .matched_path_or_any_parents(path, is_dir)
                        .is_ignore()
                    {
                        return false;
                    }
                }
            }
            true
        })
        .collect()
}

/// Borrows the watcher receiver and callgraph in separate phases to avoid
/// RefCell borrow conflicts. Events are deduplicated by PathBuf — notify
/// fires multiple events per file write (Create, Modify, etc.).
fn drain_watcher_events(ctx: &AppContext) {
    // Phase 1: collect changed paths from the receiver without applying the
    // gitignore matcher yet; .gitignore writes in this same batch must rebuild
    // the matcher before any sibling path is filtered.
    let changed: HashSet<std::path::PathBuf> = {
        let rx_ref = ctx.watcher_rx().borrow();
        let rx = match rx_ref.as_ref() {
            Some(rx) => rx,
            None => return, // No watcher configured
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

    if changed.is_empty() {
        return;
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
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if SOURCE_EXTENSIONS.contains(&ext) {
                    graph.invalidate_file(path);
                }
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
    if let Some(index) = semantic_index_ref.as_mut() {
        let mut semantic_stale = false;
        for path in &changed {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if SOURCE_EXTENSIONS.contains(&ext) {
                    index.invalidate_file(path);
                    if path.exists() {
                        semantic_stale = true;
                    }
                }
            }
        }
        if semantic_stale {
            *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                stage: "stale_after_file_change".to_string(),
                files: None,
                entries_done: None,
                entries_total: None,
            };
        }
    }

    aft::slog_info!("invalidated {} files", changed.len());
}

fn drain_search_index_events(ctx: &AppContext) {
    let latest = {
        let rx_ref = ctx.search_index_rx().borrow();
        let Some(rx) = rx_ref.as_ref() else {
            return;
        };

        let mut latest = None;
        while let Ok(pair) = rx.try_recv() {
            latest = Some(pair);
        }
        latest
    };

    if let Some(index) = latest {
        *ctx.search_index().borrow_mut() = Some(index);
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
    for event in events {
        match event {
            SemanticIndexEvent::Progress {
                stage,
                files,
                entries_done,
                entries_total,
            } => {
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
                    stage,
                    files,
                    entries_done,
                    entries_total,
                };
            }
            SemanticIndexEvent::Ready(index) => {
                *ctx.semantic_index().borrow_mut() = Some(index);
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Ready;
                keep_receiver = false;
                status_changed = true;
            }
            SemanticIndexEvent::Failed(error) => {
                *ctx.semantic_index().borrow_mut() = None;
                *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Failed(error);
                keep_receiver = false;
                status_changed = true;
            }
        }
    }

    if !keep_receiver {
        *ctx.semantic_index_rx().borrow_mut() = None;
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
    use super::{filter_watcher_raw_paths, watcher_event_invalidates};
    use aft::config::Config;
    use aft::context::AppContext;
    use aft::parser::TreeSitterProvider;
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
    fn gitignore_write_rebuilds_before_filtering_same_batch_paths() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let gitignore = root.join(".gitignore");
        let ignored = root.join("foo.txt");
        let kept = root.join("bar.txt");
        std::fs::write(&ignored, "ignored").unwrap();
        std::fs::write(&kept, "kept").unwrap();

        let ctx = make_ctx_with_root(root);
        ctx.rebuild_gitignore();
        assert!(ctx.gitignore().is_none());

        std::fs::write(&gitignore, "foo.txt\n").unwrap();
        let changed =
            filter_watcher_raw_paths(&ctx, vec![gitignore.clone(), ignored.clone(), kept.clone()]);

        let gitignore = std::fs::canonicalize(gitignore).unwrap();
        let ignored = std::fs::canonicalize(ignored).unwrap();
        let kept = std::fs::canonicalize(kept).unwrap();
        assert!(changed.contains(&gitignore));
        assert!(!changed.contains(&ignored));
        assert!(changed.contains(&kept));
    }
}
