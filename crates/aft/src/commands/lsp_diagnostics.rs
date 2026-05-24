use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::context::AppContext;
use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
use crate::lsp::manager::{
    EnsureServerOutcomes, PullFileOutcome, PullFileResult, ServerAttemptResult,
};
use crate::lsp::roots::find_workspace_root;
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
    let outcomes: EnsureServerOutcomes = {
        let mut lsp = ctx.lsp();
        lsp.ensure_server_for_file_detailed(&canonical, &ctx.config())
    };

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

    // Step 3: pull diagnostics from every server that supports it. Track
    // which servers we got a fresh result for.
    let pull_results = {
        let mut lsp = ctx.lsp();
        match lsp.pull_file_diagnostics(&canonical, &ctx.config()) {
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
        wait_for_push(ctx, wait_ms);
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
            "push_only" => {
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
    let diagnostics =
        collect_file_diagnostics_for_servers(ctx, &canonical, severity_filter, &proven_servers);
    let response = build_response(
        &diagnostics,
        server_status,
        complete,
        Vec::new(),
        None,
        pending_servers,
    );
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

    for key in &server_keys_to_pull {
        let pull_result = {
            let mut lsp = ctx.lsp();
            lsp.pull_workspace_diagnostics(key, None)
        };
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
        .filter(|(key, _)| proven_servers.contains(*key))
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

fn wait_for_push(ctx: &AppContext, wait_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        {
            let mut lsp = ctx.lsp();
            lsp.drain_events();
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

fn needs_push_wait(pull_results: &[PullFileResult]) -> bool {
    pull_results
        .iter()
        .any(|r| matches!(r.outcome, PullFileOutcome::PullNotSupported))
        || pull_results.is_empty()
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
            PullFileOutcome::RequestFailed { reason } => format!("pull_failed: {reason}"),
        };
    }
}

fn compute_unchecked_files(ctx: &AppContext, dir: &Path) -> (Vec<String>, bool) {
    let mut resolvable_files = Vec::new();
    let config = ctx.config();

    let walker = ignore::WalkBuilder::new(dir)
        .standard_filters(true) // honors .gitignore + hidden-file rules
        .filter_entry(|e| {
            // Skip noisy directories that explode walk time on real repos.
            let name = e.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git" | "node_modules" | "target" | "dist" | "build" | ".next" | ".turbo"
            )
        })
        .build();

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
    }

    if resolvable_files.len() > DIRECTORY_FILE_CAP {
        let unchecked = resolvable_files[DIRECTORY_FILE_CAP..]
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        return (unchecked, true);
    }

    let mut unchecked = Vec::new();
    for path in resolvable_files {
        let missing_any_matching_server = {
            let lsp = ctx.lsp();
            crate::lsp::registry::servers_for_file(&path, &config)
                .into_iter()
                .any(|def| {
                    let Some(root) = find_workspace_root(&path, &def.root_markers) else {
                        return true;
                    };
                    let key = ServerKey {
                        kind: def.kind,
                        root,
                    };
                    !lsp.diagnostics_store_for_test()
                        .has_report_for_server_file(&key, &path)
                })
        };
        if missing_any_matching_server {
            unchecked.push(path.display().to_string());
        }
    }

    (unchecked, false)
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
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
