use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::context::AppContext;
use crate::lsp::client::RustCheckState;
use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use crate::lsp::manager::{
    EnsureServerOutcomes, PreEditSnapshot, PullFileOutcome, PullFileResult, ServerAttemptResult,
};
use crate::lsp::registry::ServerKind;
use crate::lsp::roots::ServerKey;
use crate::protocol::{RawRequest, Response};

const MAX_WAIT_MS: u64 = 10_000;
const DIRECTORY_FILE_CAP: usize = 200;

#[derive(Debug, Deserialize)]
struct LspDiagnosticsParams {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

/// Handle an `lsp_diagnostics` request.
///
/// **Promise:** This is an on-demand LSP file/scope check. It is NOT a
/// replacement for project-wide type checkers. For "everything in the
/// project", run `tsc --noEmit`, `cargo check`, `pyright`, etc.
///
/// Behavior summary:
/// - **File mode** (`file`): ensures the relevant LSP server(s) are running
///   and the document is in sync, then prefers `textDocument/diagnostic`
///   (pull) when supported, falling back to `publishDiagnostics` (push) +
///   `wait_ms`. Reports per-server status so the agent can tell
///   "checked clean" from "no server registered" from "server crashed".
///
/// - **Directory mode** (`directory`): returns whatever the diagnostic cache
///   already knows for files under the directory plus, for servers that
///   support `workspace/diagnostic`, an active workspace pull. Files we have
///   no information for are listed in `unchecked_files`. The response sets
///   `complete: false` whenever some servers couldn't pull workspace-wide.
///
/// - **No-args**: returns all diagnostics in the cache.
///
/// Response shape:
/// ```json
/// {
///   "diagnostics": [...],
///   "total": N,
///   "files_with_errors": M,
///   "complete": true|false,
///   "lsp_servers_used": [{ "server_id", "scope", "status" }],
///   "unchecked_files": [...]   // directory mode only
/// }
/// ```
pub fn handle_lsp_diagnostics(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<LspDiagnosticsParams>(req.params.clone()) {
        Ok(params) => params,
        Err(err) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("lsp_diagnostics: invalid params: {err}"),
            );
        }
    };

    if params.file.is_some() && params.directory.is_some() {
        return Response::error(
            &req.id,
            "invalid_request",
            "lsp_diagnostics: provide either 'file' or 'directory', not both",
        );
    }

    let wait_ms = params.wait_ms.unwrap_or(0);
    if wait_ms > MAX_WAIT_MS {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("lsp_diagnostics: wait_ms must be <= {MAX_WAIT_MS}"),
        );
    }

    let severity_filter = match parse_severity_filter(params.severity.as_deref()) {
        Ok(filter) => filter,
        Err(message) => return Response::error(&req.id, "invalid_request", message),
    };

    match (&params.file, &params.directory) {
        (Some(file), None) => handle_file_mode(req, ctx, file, severity_filter, wait_ms),
        (None, Some(directory)) => {
            handle_directory_mode(req, ctx, directory, severity_filter, wait_ms)
        }
        (None, None) => handle_global_mode(req, ctx, severity_filter, wait_ms),
        _ => unreachable!("checked above"),
    }
}

/// File mode: ensure the LSP is running, prefer pull, fall back to push.
fn handle_file_mode(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    severity_filter: SeverityFilter,
    wait_ms: u64,
) -> Response {
    let canonical = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => normalize_query_path(&path),
        Err(resp) => return resp,
    };

    // Step 1: figure out what servers are registered for this file and try
    // to spawn them. The structured outcomes let us tell the agent honestly
    // which servers couldn't be brought up.
    let outcomes: EnsureServerOutcomes =
        crate::lsp::manager::ensure_server_for_file_detailed_unlocked(
            || ctx.lsp(),
            &canonical,
            &ctx.config(),
        );

    let mut server_status: Vec<ServerStatusEntry> = outcomes
        .attempts
        .iter()
        .map(|attempt| ServerStatusEntry::from_attempt(attempt, ServerScope::File))
        .collect();

    if outcomes.no_server_registered() {
        // Nothing in the registry handles this extension. This is the
        // honest "we cannot say anything" case.
        return Response::success(
            &req.id,
            serde_json::json!({
                "diagnostics": [],
                "total": 0,
                "files_with_errors": 0,
                "complete": true,
                "lsp_servers_used": [],
                "note": format!("no LSP server is registered for '{}'", file),
            }),
        );
    }

    if outcomes.successful.is_empty() {
        // Servers matched but none could start. Return an empty result with
        // detailed per-server status so the user can see why.
        return Response::success(
            &req.id,
            serde_json::json!({
                "diagnostics": [],
                "total": 0,
                "files_with_errors": 0,
                "complete": false,
                "lsp_servers_used": server_status,
            }),
        );
    }

    // Step 2: snapshot before syncing/opening so push fallback freshness is
    // anchored to LSP document versions, not wall-clock arrival time. A late
    // publish for an older document version can arrive inside our wait window;
    // version matching below rejects it.
    let pre_push_snapshot = {
        let lsp = ctx.lsp();
        lsp.snapshot_pre_edit_state(&canonical)
    };

    // rust-analyzer's pulled report holds only its own analysis, and while it
    // is still loading the workspace that analysis is not ready. The
    // compiler's errors arrive separately, when a `cargo check` finishes, and
    // an edit's save starts a new one; until then the stored errors describe
    // the files before the edit. Wait for both within `wait_ms`, before the
    // pull so the pull sees the loaded workspace. A server still busy after
    // the wait makes the answer incomplete.
    let wait_deadline = Instant::now() + Duration::from_millis(wait_ms);
    let rust_servers: Vec<ServerKey> = outcomes
        .successful
        .iter()
        .filter(|key| key.kind == ServerKind::Rust)
        .cloned()
        .collect();
    let still_checking = wait_for_rust_check(ctx, &rust_servers, wait_deadline);

    // Step 3: pull diagnostics from every server that supports it. Track
    // which servers we got a fresh result for. The manager lock is released
    // while servers work on the requests.
    let pull_results = {
        let config = ctx.config();
        match crate::lsp::manager::pull_file_diagnostics_with_cargo_check_unlocked(
            || ctx.lsp(),
            &canonical,
            &config,
            None,
        ) {
            Ok(results) => results,
            Err(err) => {
                crate::slog_warn!("[lsp_diagnostics] pull_file_diagnostics failed: {err}");
                Vec::new()
            }
        }
    };
    update_status_with_pull(&mut server_status, &pull_results);

    // Step 4: for servers that didn't support pull, drain push events for
    // the requested wait_ms. Empty publishes are preserved as "checked
    // clean" so we can read them back.
    if needs_push_wait(&pull_results) && wait_ms > 0 {
        let push_servers = servers_needing_push(&outcomes.successful, &pull_results);
        wait_for_push(ctx, &canonical, &push_servers, &pre_push_snapshot, wait_ms);
    }

    // Step 5: read the cache and build the response.
    //
    // v0.17.3 honest-reporting fix: `complete` is true only if every
    // expected server gave us a deterministically-fresh result for this
    // file. The rules:
    //
    //   - `pull_ok` / `pull_unchanged`: LSP protocol guarantees freshness.
    //   - `push_only`: server doesn't support pull. We can only claim
    //     freshness if we actually waited (`wait_ms > 0`) AND the cache
    //     has an entry from this server for this file (proving a publish
    //     arrived during the wait, not before it). With the default
    //     `wait_ms = 0` no wait happened, so push_only is reported but
    //     does NOT contribute to completeness.
    //   - everything else (`pull_failed`, `binary_not_installed`, etc.):
    //     not complete.
    //
    // Without this distinction, a tool with `wait_ms = 0` (the default)
    // could report `complete: true` against pre-existing stale cache for
    // a push-only server that hadn't published anything for the current
    // file state. That was Oracle's pre-release blocker for v0.17.3.
    let proven_push_servers: HashSet<ServerKey> = if wait_ms > 0 {
        let lsp = ctx.lsp();
        outcomes
            .successful
            .iter()
            .filter(|key| {
                let pre = pre_push_snapshot.get(key).copied().unwrap_or_default();
                lsp.diagnostic_entry_is_fresh_for_document(&canonical, key, pre)
            })
            .cloned()
            .collect()
    } else {
        HashSet::new()
    };

    let pull_fresh_servers: HashSet<ServerKey> = pull_results
        .iter()
        .filter(|result| {
            matches!(
                result.outcome,
                PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged
            )
        })
        .map(|result| result.server_key.clone())
        .collect();

    let mut proven_servers = pull_fresh_servers.clone();
    proven_servers.extend(proven_push_servers.iter().cloned());

    let mut pending_servers = Vec::new();
    let mut complete = true;
    for entry in &server_status {
        match entry.status.as_str() {
            "pull_ok" | "pull_unchanged" => {}
            "push_only" | "pull_rejected_push_fallback" | "pull_no_cache_for_unchanged" => {
                let fresh = outcomes.successful.iter().any(|key| {
                    key.kind.id_str() == entry.server_id && proven_push_servers.contains(key)
                });
                if !fresh {
                    complete = false;
                    pending_servers.push(entry.server_id.clone());
                }
            }
            _ => complete = false,
        }
    }
    for (key, _) in &still_checking {
        complete = false;
        let id = key.kind.id_str().to_string();
        if !pending_servers.contains(&id) {
            pending_servers.push(id);
        }
    }
    let diagnostics =
        collect_file_diagnostics_for_servers(ctx, &canonical, severity_filter, &proven_servers);
    // Documents opened only to answer this query are closed again; their
    // diagnostics stay stored.
    ctx.lsp()
        .close_documents_opened_for_pulls(&canonical, &pull_results);
    let mut response = build_response(
        &diagnostics,
        server_status,
        complete,
        Vec::new(),
        None,
        pending_servers,
    );
    if let Some((_, reason)) = still_checking.first() {
        response["note"] = serde_json::Value::from(*reason);
    }
    Response::success(&req.id, response)
}

/// Directory mode: return cached + try workspace-pull for each server.
/// Files we have NO information about are listed in `unchecked_files`.
fn handle_directory_mode(
    req: &RawRequest,
    ctx: &AppContext,
    directory: &str,
    severity_filter: SeverityFilter,
    _wait_ms: u64,
) -> Response {
    let canonical = match ctx.validate_path(&req.id, Path::new(directory)) {
        Ok(path) => normalize_query_path(&path),
        Err(resp) => return resp,
    };

    let mut server_status: Vec<ServerStatusEntry> = Vec::new();
    let mut all_complete = true;

    // Pull workspace diagnostics from active servers that support it. We
    // do NOT walk the directory and spawn new servers here — that's the
    // "open every file" anti-pattern Oracle rejected. Instead we use
    // whatever servers the agent has already triggered via prior file-mode
    // calls or post-write hooks. For each active server, attempt
    // `workspace/diagnostic`. Servers that don't support it return early
    // with `supports_workspace=false`.
    let server_keys_to_pull: Vec<crate::lsp::roots::ServerKey> = {
        let lsp = ctx.lsp();
        lsp.active_server_keys()
    };

    let pull_results = crate::lsp::manager::pull_workspace_diagnostics_many_unlocked(
        || ctx.lsp(),
        &server_keys_to_pull,
        None,
    );
    for (key, pull_result) in server_keys_to_pull.iter().zip(pull_results) {
        match pull_result {
            Ok(result) => {
                let status = if !result.supports_workspace {
                    "workspace_pull_unsupported"
                } else if result.cancelled {
                    all_complete = false;
                    "workspace_pull_timed_out"
                } else if result.complete {
                    "workspace_pull_ok"
                } else {
                    all_complete = false;
                    "workspace_pull_partial"
                };
                server_status.push(ServerStatusEntry {
                    server_id: key.kind.id_str().to_string(),
                    scope: ServerScope::Workspace,
                    status: status.to_string(),
                });
            }
            Err(err) => {
                crate::slog_warn!("[lsp_diagnostics] workspace pull failed for {key:?}: {err}");
                all_complete = false;
                server_status.push(ServerStatusEntry {
                    server_id: key.kind.id_str().to_string(),
                    scope: ServerScope::Workspace,
                    status: "request_failed".to_string(),
                });
            }
        }
    }

    // Now read the cache for the directory.
    let diagnostics = collect_directory_diagnostics(ctx, &canonical, severity_filter);

    // Compute unchecked_files: walk the directory (capped) and list any
    // file that has no entry in the diagnostic cache. We cap at
    // DIRECTORY_FILE_CAP to avoid pathological large-directory walks.
    let (unchecked_files, walk_truncated) = compute_unchecked_files(ctx, &canonical);
    if walk_truncated || !unchecked_files.is_empty() {
        all_complete = false;
    }

    let response = build_response(
        &diagnostics,
        server_status,
        all_complete,
        unchecked_files,
        Some(walk_truncated),
        Vec::new(),
    );
    Response::success(&req.id, response)
}

/// Global mode: just return everything in the cache.
fn handle_global_mode(
    req: &RawRequest,
    ctx: &AppContext,
    severity_filter: SeverityFilter,
    _wait_ms: u64,
) -> Response {
    let diagnostics: Vec<StoredDiagnostic> = {
        let lsp = ctx.lsp();
        lsp.get_all_diagnostics()
            .into_iter()
            .filter(|diagnostic| severity_filter.matches(diagnostic.severity))
            .cloned()
            .collect()
    };
    let response = build_response(&diagnostics, Vec::new(), true, Vec::new(), None, Vec::new());
    Response::success(&req.id, response)
}

fn collect_file_diagnostics_for_servers(
    ctx: &AppContext,
    canonical: &Path,
    severity_filter: SeverityFilter,
    proven_servers: &HashSet<ServerKey>,
) -> Vec<StoredDiagnostic> {
    if proven_servers.is_empty() {
        return Vec::new();
    }

    let lsp = ctx.lsp();
    lsp.diagnostics_store_for_test()
        .entries_for_file(canonical)
        .into_iter()
        .filter(|(key, entry)| proven_servers.contains(*key) && !entry.stale)
        .flat_map(|(_, entry)| entry.diagnostics.iter())
        .filter(|diagnostic| severity_filter.matches(diagnostic.severity))
        .cloned()
        .collect()
}

fn collect_directory_diagnostics(
    ctx: &AppContext,
    canonical: &Path,
    severity_filter: SeverityFilter,
) -> Vec<StoredDiagnostic> {
    let lsp = ctx.lsp();
    lsp.get_diagnostics_for_directory(canonical)
        .into_iter()
        .filter(|diagnostic| severity_filter.matches(diagnostic.severity))
        .cloned()
        .collect()
}

/// Wait, until `deadline`, for the given rust-analyzer servers to finish
/// loading the workspace and then a running or expected `cargo check`: one a
/// save asked for, or the one rust-analyzer starts on becoming quiescent
/// (until it reports, the published diagnostics lack every compiler error,
/// and an edit made meanwhile races with it).
/// While either is under way their published diagnostics describe an older
/// state of the files. A check that was expected and did not begin in time is
/// not waited for but still returned as busy: its results are unknown. The
/// manager lock is held only to drain events and read the state, never across
/// a sleep. Returns the servers still busy when the wait ended, each with the
/// reason to report.
fn wait_for_rust_check(
    ctx: &AppContext,
    servers: &[ServerKey],
    deadline: Instant,
) -> Vec<(ServerKey, &'static str)> {
    let wait = {
        let mut lsp = ctx.lsp();
        for key in servers {
            lsp.rearm_unreported_rust_check(key);
        }
        lsp.subscribe_events()
    };
    let mut event = None;
    loop {
        note_wait_wakeup();
        let (busy, waiting, next_timed_change) = {
            let mut lsp = ctx.lsp();
            lsp.handle_waited_event(event);
            let mut busy = Vec::new();
            let mut waiting = false;
            for key in servers {
                // A server that reported a failed load is not loading;
                // its failure is reported through its status instead.
                if lsp.producer_failure(key).is_none() && lsp.server_is_warming(key) {
                    busy.push((key.clone(), RUST_INDEXING_REASON));
                    waiting = true;
                    continue;
                }
                match lsp.rust_check_state(key) {
                    RustCheckState::Current => {}
                    state => {
                        busy.push((
                            key.clone(),
                            crate::inspect::diagnostics_category::RUST_CHECK_RUNNING_REASON,
                        ));
                        waiting |= state == RustCheckState::Running;
                    }
                }
            }
            let next_timed_change = servers
                .iter()
                .filter_map(|key| lsp.rust_check_next_timed_change(key))
                .min();
            wait.clear_pending_wake();
            (busy, waiting, next_timed_change)
        };
        if !waiting || Instant::now() >= deadline {
            ctx.lsp().unsubscribe_events(wait);
            return busy;
        }
        // Sleep until something can change: an event from a server (a
        // progress report, a publish, a status change) or a timed
        // transition of the check state. Polling at a fixed interval
        // instead took the manager lock every 50 ms for the whole wait.
        let until = next_timed_change.map_or(deadline, |change| change.min(deadline));
        event = wait.next_event(until);
    }
}

/// Why a Rust result is incomplete while rust-analyzer is still loading the
/// workspace: until it finishes, its analysis and its first `cargo check`
/// have not covered the files.
const RUST_INDEXING_REASON: &str = "rust-analyzer: still indexing; retry";

/// Wait up to `wait_ms` for each server that answers this file only through
/// `publishDiagnostics` to publish for the document's current version, and
/// return as soon as every one of them has (or has exited). The manager lock
/// is held only to handle events and check, never while waiting; the wait
/// wakes when an event arrives instead of sleeping out the whole budget.
fn wait_for_push(
    ctx: &AppContext,
    canonical: &Path,
    push_servers: &[ServerKey],
    pre_push_snapshot: &HashMap<ServerKey, PreEditSnapshot>,
    wait_ms: u64,
) {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    let wait = ctx.lsp().subscribe_events();
    let mut event = None;
    loop {
        note_wait_wakeup();
        let all_published = {
            let mut lsp = ctx.lsp();
            lsp.handle_waited_event(event);
            let all_published = push_servers.iter().all(|key| {
                !lsp.has_client(key) || {
                    let pre = pre_push_snapshot.get(key).copied().unwrap_or_default();
                    lsp.diagnostic_entry_is_fresh_for_document(canonical, key, pre)
                }
            });
            wait.clear_pending_wake();
            all_published
        };
        if all_published || Instant::now() >= deadline {
            break;
        }
        event = wait.next_event(deadline);
    }
    ctx.lsp().unsubscribe_events(wait);
}

/// The servers whose diagnostics for this file can only arrive by push: no
/// pull support, or a pull that fell back to push. With no pull results at
/// all, every started server.
fn servers_needing_push(
    successful: &[ServerKey],
    pull_results: &[PullFileResult],
) -> Vec<ServerKey> {
    successful
        .iter()
        .filter(|key| {
            match pull_results
                .iter()
                .find(|result| &result.server_key == *key)
            {
                None => true,
                Some(result) => match &result.outcome {
                    PullFileOutcome::PullNotSupported => true,
                    PullFileOutcome::RequestFailed { reason } => request_failure_needs_push(reason),
                    _ => false,
                },
            }
        })
        .cloned()
        .collect()
}

thread_local! {
    static WAIT_WAKEUPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_wait_wakeup() {
    WAIT_WAKEUPS.with(|count| count.set(count.get() + 1));
}

/// How many times the diagnostics waits run on this thread re-checked under
/// the manager lock. Tests compare it before and after a request to prove a
/// wait woke on arrival rather than polling.
#[doc(hidden)]
pub fn wait_wakeups_for_test() -> u64 {
    WAIT_WAKEUPS.with(std::cell::Cell::get)
}

fn needs_push_wait(pull_results: &[PullFileResult]) -> bool {
    pull_results.iter().any(|r| match &r.outcome {
        PullFileOutcome::PullNotSupported => true,
        PullFileOutcome::RequestFailed { reason } => request_failure_needs_push(reason),
        _ => false,
    }) || pull_results.is_empty()
}

fn request_failure_needs_push(reason: &str) -> bool {
    reason == "no_cache_for_unchanged" || reason.starts_with("pull_rejected_push_fallback:")
}

fn request_failure_status(reason: &str) -> String {
    if reason == "no_cache_for_unchanged" {
        "pull_no_cache_for_unchanged".to_string()
    } else if reason.starts_with("pull_rejected_push_fallback:") {
        "pull_rejected_push_fallback".to_string()
    } else {
        format!("pull_failed: {reason}")
    }
}

fn update_status_with_pull(
    server_status: &mut [ServerStatusEntry],
    pull_results: &[PullFileResult],
) {
    let mut by_id: HashMap<String, &PullFileResult> = HashMap::new();
    for result in pull_results {
        by_id.insert(result.server_key.kind.id_str().to_string(), result);
    }

    for entry in server_status.iter_mut() {
        if entry.status != "ok" {
            continue;
        }
        let Some(pull) = by_id.get(&entry.server_id) else {
            continue;
        };
        entry.status = match &pull.outcome {
            PullFileOutcome::Full { .. } => "pull_ok".to_string(),
            PullFileOutcome::Unchanged => "pull_unchanged".to_string(),
            PullFileOutcome::PullNotSupported => "push_only".to_string(),
            PullFileOutcome::PartialNotSupported => "pull_partial_skipped".to_string(),
            PullFileOutcome::RequestFailed { reason } => request_failure_status(reason),
        };
    }
}

fn compute_unchecked_files(ctx: &AppContext, dir: &Path) -> (Vec<String>, bool) {
    let mut resolvable_files = Vec::new();
    let config = ctx.config();

    // Prevent a disappearing child mount from making ReadDir::drop abort on ENXIO.
    let mut builder = ignore::WalkBuilder::new(dir);
    builder.same_file_system(true).standard_filters(true); // hidden-file rules
    let walker = crate::context::apply_project_ignore_rules(&mut builder, dir)
        .filter_entry(|e| {
            // Skip noisy directories that explode walk time on real repos.
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | "dist" | "build" | ".next" | ".turbo"
            )
        })
        .build();

    let mut walk_truncated = false;
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
        if !is_file {
            continue;
        }
        let path = entry.path();
        // Only track files that have a registered LSP server for their
        // extension — listing every random file is noise.
        if crate::lsp::registry::servers_for_file(path, &config).is_empty() {
            continue;
        }

        resolvable_files.push(path.to_path_buf());
        if resolvable_files.len() > DIRECTORY_FILE_CAP {
            walk_truncated = true;
            break;
        }
    }

    if walk_truncated {
        resolvable_files.truncate(DIRECTORY_FILE_CAP);
    }

    let mut unchecked = Vec::new();
    for path in resolvable_files {
        let missing_any_matching_server = {
            let lsp = ctx.lsp();
            crate::lsp::registry::servers_for_file(&path, &config)
                .into_iter()
                .any(|def| {
                    let Some(root) = def.workspace_root_for_file(&path) else {
                        return true;
                    };
                    let key = ServerKey {
                        kind: def.kind,
                        root,
                    };
                    !lsp.diagnostics_store_for_test()
                        .has_fresh_report_for_server_file(&key, &path)
                })
        };
        if missing_any_matching_server {
            unchecked.push(path.display().to_string());
        }
    }

    (unchecked, walk_truncated)
}

fn build_response(
    diagnostics: &[StoredDiagnostic],
    server_status: Vec<ServerStatusEntry>,
    complete: bool,
    unchecked_files: Vec<String>,
    walk_truncated: Option<bool>,
    pending_servers: Vec<String>,
) -> serde_json::Value {
    let mut sorted: Vec<&StoredDiagnostic> = diagnostics.iter().collect();
    sorted.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
            .then(left.end_line.cmp(&right.end_line))
            .then(left.end_column.cmp(&right.end_column))
            .then(left.message.cmp(&right.message))
    });

    let files_with_errors = sorted
        .iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        .map(|diagnostic| diagnostic.file.clone())
        .collect::<HashSet<PathBuf>>()
        .len();

    let diagnostics_json: Vec<serde_json::Value> = sorted
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "file": diagnostic.file.display().to_string(),
                "line": diagnostic.line,
                "column": diagnostic.column,
                "end_line": diagnostic.end_line,
                "end_column": diagnostic.end_column,
                "severity": diagnostic.severity.as_str(),
                "message": diagnostic.message,
                "code": diagnostic.code,
                "source": diagnostic.source,
            })
        })
        .collect();

    let mut response = serde_json::json!({
        "diagnostics": diagnostics_json,
        "total": diagnostics_json.len(),
        "files_with_errors": files_with_errors,
        "complete": complete,
        "lsp_servers_used": server_status,
    });

    if !unchecked_files.is_empty() {
        response["unchecked_files"] = serde_json::Value::Array(
            unchecked_files
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }
    if let Some(truncated) = walk_truncated {
        if truncated {
            response["walk_truncated"] = serde_json::Value::Bool(true);
        }
    }
    if !pending_servers.is_empty() {
        response["pending_servers"] = serde_json::Value::Array(
            pending_servers
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }

    response
}

#[derive(Debug, Clone, serde::Serialize)]
struct ServerStatusEntry {
    server_id: String,
    scope: ServerScope,
    status: String,
}

impl ServerStatusEntry {
    fn from_attempt(attempt: &crate::lsp::manager::ServerAttempt, scope: ServerScope) -> Self {
        let status = match &attempt.result {
            ServerAttemptResult::ProjectClosed => {
                "project closed; reopen the project and retry".into()
            }
            ServerAttemptResult::Ok { .. } => "ok".to_string(),
            ServerAttemptResult::NoRootMarker { looked_for } => {
                format!("no_root_marker (looked for: {})", looked_for.join(", "))
            }
            ServerAttemptResult::BinaryNotInstalled { binary } => {
                format!("binary_not_installed: {binary}")
            }
            ServerAttemptResult::SpawnFailed { binary, reason } => {
                format!("spawn_failed: {binary} ({reason})")
            }
        };
        Self {
            server_id: attempt.server_id.clone(),
            scope,
            status,
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum ServerScope {
    File,
    Workspace,
}

#[derive(Debug, Clone, Copy)]
enum SeverityFilter {
    All,
    Only(DiagnosticSeverity),
}

impl SeverityFilter {
    fn matches(self, severity: DiagnosticSeverity) -> bool {
        match self {
            Self::All => true,
            Self::Only(expected) => expected == severity,
        }
    }
}

fn parse_severity_filter(value: Option<&str>) -> Result<SeverityFilter, String> {
    match value.unwrap_or("all") {
        "all" => Ok(SeverityFilter::All),
        "error" => Ok(SeverityFilter::Only(DiagnosticSeverity::Error)),
        "warning" => Ok(SeverityFilter::Only(DiagnosticSeverity::Warning)),
        "information" => Ok(SeverityFilter::Only(DiagnosticSeverity::Information)),
        "hint" => Ok(SeverityFilter::Only(DiagnosticSeverity::Hint)),
        other => Err(format!(
            "lsp_diagnostics: invalid severity '{other}' (expected error, warning, information, hint, or all)"
        )),
    }
}

fn normalize_query_path(path: &Path) -> PathBuf {
    // Same normalized form as the LSP subsystem's storage keys; a bare
    // canonicalize would query with Windows verbatim spellings and miss.
    crate::inspect::job::canonicalize_normalized(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::parser::TreeSitterProvider;
    use std::fs;

    #[test]
    fn compute_unchecked_files_caps_resolvable_walk() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("workspace");
        let src = root.join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").expect("cargo toml");
        for index in 0..250 {
            fs::write(src.join(format!("file_{index:03}.rs")), "fn main() {}\n").expect("write rs");
        }

        let config = Config {
            project_root: Some(root),
            ..Config::default()
        };
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);

        let (unchecked, truncated) = compute_unchecked_files(&ctx, &src);

        assert!(truncated);
        assert_eq!(unchecked.len(), DIRECTORY_FILE_CAP);
    }
}
