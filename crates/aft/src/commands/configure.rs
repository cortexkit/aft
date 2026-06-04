use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::unbounded;
use notify::{RecursiveMode, Watcher};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

use crate::callgraph::CallGraph;
use crate::config::{SemanticBackend, SemanticBackendConfig, UserServerDef};
use crate::context::{
    AppContext, SemanticIndexEvent, SemanticIndexStatus, SemanticProgressStats,
    SemanticRefreshEvent, SemanticRefreshRequest, SemanticRefreshWorkerSlot,
};
use crate::harness::Harness;
use crate::log_ctx;
use crate::lsp::registry::{resolve_lsp_binary, servers_for_file, ServerKind};
use crate::parser::{detect_language, LangId, SharedSymbolCache};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, current_git_head, project_cache_key, resolve_cache_dir,
    walk_project_files_bounded_matching, CacheLock, SearchIndex,
};
use crate::semantic_index::{
    is_semantic_indexed_extension, SemanticIndex, SemanticIndexLock, SemanticMemoryEstimate,
};
use crate::{slog_info, slog_warn};

static WATCHER_GENERATION: AtomicU64 = AtomicU64::new(0);

const MAX_SEARCH_INDEX_FILES: usize = 20_000;
const SEMANTIC_REFRESH_QUIET_WINDOW_MS: u64 = 250;
const SEMANTIC_REFRESH_MAX_BATCH_PATHS: usize = 50;
const MAX_SEMANTIC_TIMEOUT_MS: u64 = 120_000;
const MAX_SEMANTIC_BATCH_SIZE: usize = 1_024;

fn semantic_progress_stats_from_estimate(
    estimate: SemanticMemoryEstimate,
) -> SemanticProgressStats {
    SemanticProgressStats {
        indexed_files: Some(estimate.indexed_files),
        entries: Some(estimate.entries),
        vector_bytes: Some(estimate.vector_bytes),
        snippet_bytes: Some(estimate.snippet_bytes),
        embed_text_bytes: Some(estimate.embed_text_bytes),
        metadata_bytes: Some(estimate.metadata_bytes),
        estimated_payload_bytes: Some(estimate.estimated_payload_bytes),
        cache_bytes: None,
        clone_estimated_bytes: None,
    }
}

fn semantic_clone_stats_from_estimate(estimate: SemanticMemoryEstimate) -> SemanticProgressStats {
    SemanticProgressStats {
        clone_estimated_bytes: Some(estimate.estimated_payload_bytes),
        ..semantic_progress_stats_from_estimate(estimate)
    }
}

fn semantic_progress_stats_with_cache(
    index: &SemanticIndex,
    cache_bytes: u64,
) -> SemanticProgressStats {
    SemanticProgressStats {
        cache_bytes: Some(cache_bytes),
        ..semantic_progress_stats_from_estimate(index.memory_estimate())
    }
}

fn resolve_home_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(std::fs::canonicalize(&raw).unwrap_or(raw))
}

fn create_project_watcher(
    root_path: PathBuf,
    tx: mpsc::Sender<notify::Result<notify::Event>>,
) -> notify::Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(&root_path, RecursiveMode::Recursive)?;
    Ok(watcher)
}

fn install_project_watcher_with<W, E, F>(
    ctx: &AppContext,
    root_path: &Path,
    attach: F,
) -> thread::JoinHandle<()>
where
    W: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce(PathBuf, mpsc::Sender<notify::Result<notify::Event>>) -> Result<W, E>
        + Send
        + 'static,
{
    // Drop old synchronous watcher/receiver before replacing them (re-configure).
    *ctx.watcher().borrow_mut() = None;
    *ctx.watcher_rx().borrow_mut() = None;

    let generation = WATCHER_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1);
    let (tx, rx) = mpsc::channel();
    *ctx.watcher_rx().borrow_mut() = Some(rx);

    let root_path = root_path.to_path_buf();
    let session_id_for_bg = log_ctx::current_session();
    thread::spawn(move || {
        log_ctx::with_session(session_id_for_bg, || match attach(root_path.clone(), tx) {
            Ok(_watcher) => {
                if WATCHER_GENERATION.load(Ordering::SeqCst) == generation {
                    slog_info!("watcher started: {}", root_path.display());
                }
                while WATCHER_GENERATION.load(Ordering::SeqCst) == generation {
                    thread::sleep(Duration::from_millis(50));
                }
            }
            Err(error) => {
                if WATCHER_GENERATION.load(Ordering::SeqCst) == generation {
                    log::debug!(
                        "watcher init failed: {} — callers will work with stale data",
                        error
                    );
                }
            }
        });
    })
}

fn install_project_watcher(ctx: &AppContext, root_path: &Path) {
    let _ = install_project_watcher_with(ctx, root_path, create_project_watcher);
}

fn spawn_semantic_refresh_worker(
    project_root: PathBuf,
    mut index: SemanticIndex,
    mut model: crate::semantic_index::EmbeddingModel,
    max_batch_size: usize,
    max_files: usize,
    request_rx: crossbeam_channel::Receiver<SemanticRefreshRequest>,
    event_tx: crossbeam_channel::Sender<SemanticRefreshEvent>,
    session_id: Option<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            while let Ok(first_request) = request_rx.recv() {
                let mut paths = Vec::new();
                let mut corpus_files = None;
                match first_request {
                    SemanticRefreshRequest::Files {
                        paths: request_paths,
                    } => {
                        paths.extend(request_paths);
                    }
                    SemanticRefreshRequest::Corpus { current_files } => {
                        corpus_files = Some(current_files);
                    }
                }

                let mut disconnected = false;
                let quiet_window = Duration::from_millis(SEMANTIC_REFRESH_QUIET_WINDOW_MS);
                let mut deadline = Instant::now() + quiet_window;

                while corpus_files.is_none() && paths.len() < SEMANTIC_REFRESH_MAX_BATCH_PATHS {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match request_rx.recv_timeout(remaining) {
                        Ok(SemanticRefreshRequest::Files {
                            paths: request_paths,
                        }) => {
                            paths.extend(request_paths);
                            if paths.len() >= SEMANTIC_REFRESH_MAX_BATCH_PATHS {
                                break;
                            }
                            deadline = Instant::now() + quiet_window;
                        }
                        Ok(SemanticRefreshRequest::Corpus { current_files }) => {
                            paths.clear();
                            corpus_files = Some(current_files);
                            break;
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                if disconnected {
                    break;
                }

                if let Some(mut current_files) = corpus_files {
                    current_files.sort();
                    current_files.dedup();
                    if current_files.len() > max_files {
                        let error = format!(
                            "too many files (>{}) for semantic indexing (max {})",
                            max_files, max_files
                        );
                        let _ = event_tx.send(SemanticRefreshEvent::CorpusFailed { error });
                        continue;
                    }

                    let mut embed = |texts: Vec<String>| model.embed(texts);
                    let mut progress = |_done: usize, _total: usize| {};
                    match index.refresh_stale_files(
                        &project_root,
                        &current_files,
                        &mut embed,
                        max_batch_size,
                        &mut progress,
                    ) {
                        Ok(summary) => {
                            if !summary.is_noop() {
                                slog_info!(
                                    "semantic corpus refresh: {} changed, {} new, {} deleted, {} total processed",
                                    summary.changed,
                                    summary.added,
                                    summary.deleted,
                                    summary.total_processed,
                                );
                            }
                            let clone_estimate = index.memory_estimate();
                            let clone_started = Instant::now();
                            let refreshed_index = index.clone();
                            slog_info!(
                                "semantic corpus refresh clone: {} entries, {} estimated payload bytes, {} ms",
                                clone_estimate.entries,
                                clone_estimate.estimated_payload_bytes,
                                clone_started.elapsed().as_millis(),
                            );
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusCompleted {
                                    index: refreshed_index,
                                    changed: summary.changed,
                                    added: summary.added,
                                    deleted: summary.deleted,
                                    total_processed: summary.total_processed,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            slog_warn!("semantic corpus refresh failed: {}", error);
                            if event_tx
                                .send(SemanticRefreshEvent::CorpusFailed { error })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    continue;
                }

                paths.sort();
                paths.dedup();
                if paths.is_empty() {
                    continue;
                }

                if event_tx
                    .send(SemanticRefreshEvent::Started {
                        paths: paths.clone(),
                    })
                    .is_err()
                {
                    break;
                }

                let mut embed = |texts: Vec<String>| model.embed(texts);
                let mut progress = |_done: usize, _total: usize| {};
                match index.refresh_invalidated_files(
                    &project_root,
                    &paths,
                    &mut embed,
                    max_batch_size,
                    max_files,
                    &mut progress,
                ) {
                    Ok(update) => {
                        if !update.summary.is_noop() {
                            slog_info!(
                                "semantic refresh: {} changed, {} new, {} deleted, {} total processed",
                                update.summary.changed,
                                update.summary.added,
                                update.summary.deleted,
                                update.summary.total_processed,
                            );
                        }
                        if event_tx
                            .send(SemanticRefreshEvent::Completed {
                                added_entries: update.added_entries,
                                updated_metadata: update.updated_metadata,
                                completed_paths: update.completed_paths,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        slog_warn!(
                            "semantic refresh failed for {} file(s): {}",
                            paths.len(),
                            error
                        );
                        if event_tx
                            .send(SemanticRefreshEvent::Failed { paths, error })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    })
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    normalized
}

fn validate_storage_dir(raw: &str) -> Result<PathBuf, String> {
    let storage_dir = PathBuf::from(raw);
    if !storage_dir.is_absolute() {
        return Err("configure: storage_dir must be an absolute path".to_string());
    }

    let normalized = normalize_absolute_path(&storage_dir);
    if normalized
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("configure: storage_dir must not escape via '..' traversal".to_string());
    }

    Ok(normalized)
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn detect_worktree_bridge(project_root: &Path) -> (bool, Option<PathBuf>) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output();
    let Ok(output) = output else {
        return (false, None);
    };
    if !output.status.success() {
        return (false, None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let Some(git_dir) = lines.next().map(PathBuf::from) else {
        return (false, None);
    };
    let Some(common_dir) = lines.next().map(PathBuf::from) else {
        return (false, None);
    };
    let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
    let common_dir = std::fs::canonicalize(&common_dir).unwrap_or(common_dir);
    (git_dir != common_dir, Some(common_dir))
}

fn semantic_fingerprint_config_changed(
    previous: &SemanticBackendConfig,
    next: &SemanticBackendConfig,
) -> bool {
    previous.backend != next.backend
        || previous.model != next.model
        || previous.base_url != next.base_url
}

fn parse_semantic_config(
    value: &serde_json::Value,
    current: &SemanticBackendConfig,
) -> Result<SemanticBackendConfig, String> {
    let Some(obj) = value.as_object() else {
        return Err("configure: semantic must be an object".to_string());
    };

    let mut semantic = current.clone();

    if let Some(raw) = obj.get("backend") {
        let name = raw
            .as_str()
            .ok_or_else(|| "configure: semantic.backend must be a string".to_string())?
            .trim();
        semantic.backend = SemanticBackend::from_name(name)
            .ok_or_else(|| format!("configure: unsupported semantic.backend '{name}'"))?;
    }
    if let Some(raw) = obj.get("model") {
        semantic.model = raw
            .as_str()
            .ok_or_else(|| "configure: semantic.model must be a string".to_string())?
            .trim()
            .to_string();
    }
    if let Some(raw) = obj.get("base_url") {
        let base_url = raw
            .as_str()
            .ok_or_else(|| "configure: semantic.base_url must be a string".to_string())?
            .trim()
            .to_string();
        semantic.base_url = if base_url.is_empty() {
            None
        } else {
            // Reject private/loopback IPs at configure time to prevent SSRF.
            crate::semantic_index::validate_base_url_no_ssrf(&base_url)?;
            Some(base_url)
        };
    }
    if let Some(raw) = obj.get("api_key_env") {
        let api_key_env = raw
            .as_str()
            .ok_or_else(|| "configure: semantic.api_key_env must be a string".to_string())?
            .trim()
            .to_string();
        semantic.api_key_env = if api_key_env.is_empty() {
            None
        } else {
            Some(api_key_env)
        };
    }
    if let Some(raw) = obj.get("timeout_ms") {
        let timeout_ms = raw.as_u64().ok_or_else(|| {
            "configure: semantic.timeout_ms must be an unsigned integer".to_string()
        })?;
        semantic.timeout_ms = timeout_ms.min(MAX_SEMANTIC_TIMEOUT_MS);
    }
    if let Some(raw) = obj.get("max_batch_size") {
        let max_batch_size = raw.as_u64().ok_or_else(|| {
            "configure: semantic.max_batch_size must be an unsigned integer".to_string()
        })?;
        semantic.max_batch_size = usize::try_from(max_batch_size)
            .map_err(|_| "configure: semantic.max_batch_size is too large".to_string())?
            .min(MAX_SEMANTIC_BATCH_SIZE);
    }
    if let Some(raw) = obj.get("max_files") {
        let max_files = raw.as_u64().filter(|value| *value >= 1).ok_or_else(|| {
            format!(
                "configure: semantic.max_files must be a positive integer (>= 1); got {}",
                raw
            )
        })?;
        semantic.max_files = usize::try_from(max_files)
            .map_err(|_| "configure: semantic.max_files is too large".to_string())?;
    }

    Ok(semantic)
}

fn parse_lsp_servers(value: &Value) -> Result<Vec<UserServerDef>, String> {
    let Some(entries) = value.as_array() else {
        return Err("configure: lsp_servers must be an array".to_string());
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_lsp_server(entry, index))
        .collect()
}

fn parse_lsp_server(value: &Value, index: usize) -> Result<UserServerDef, String> {
    let Some(obj) = value.as_object() else {
        return Err(format!("configure: lsp_servers[{index}] must be an object"));
    };

    let id = required_string(obj.get("id"), index, "id")?;
    // extensions/binary are optional: when a user overrides a *built-in* server
    // (e.g. `rust`) to tweak one field, the built-in's extensions/binary are
    // inherited downstream in `resolved_servers`. Requiring them here silently
    // dropped the entire `lsp` section on a partial override (issue from #84).
    let extensions = optional_extension_array(obj.get("extensions"), index)?;
    let binary = optional_lsp_binary(obj.get("binary"), index)?;
    let args = optional_string_array(obj.get("args"), index, "args")?;
    let root_markers = optional_string_array(obj.get("root_markers"), index, "root_markers")?;
    let env = parse_lsp_server_env(obj.get("env"), index)?;
    let initialization_options = obj.get("initialization_options").cloned();
    let disabled = obj
        .get("disabled")
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                format!("configure: lsp_servers[{index}].disabled must be a boolean")
            })
        })
        .transpose()?
        .unwrap_or(false);

    Ok(UserServerDef {
        id,
        extensions,
        binary,
        args,
        root_markers,
        env,
        initialization_options,
        disabled,
    })
}

fn parse_lsp_server_env(
    value: Option<&Value>,
    index: usize,
) -> Result<HashMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let Some(obj) = value.as_object() else {
        return Err(format!(
            "configure: lsp_servers[{index}].env must be an object"
        ));
    };

    let mut env = HashMap::with_capacity(obj.len());
    for (key, value) in obj {
        let Some(value) = value.as_str() else {
            return Err(format!(
                "configure: lsp_servers[{index}].env.{key} must be a string"
            ));
        };
        env.insert(key.clone(), value.to_string());
    }
    Ok(env)
}

fn required_string(value: Option<&Value>, index: usize, field: &str) -> Result<String, String> {
    let raw = value
        .and_then(Value::as_str)
        .ok_or_else(|| format!("configure: lsp_servers[{index}].{field} must be a string"))?
        .trim();
    if raw.is_empty() {
        return Err(format!(
            "configure: lsp_servers[{index}].{field} must not be empty"
        ));
    }
    Ok(raw.to_string())
}

/// Parse the `extensions` array for an LSP server override. An absent value is
/// an empty list (no validation error) so a partial override of a built-in
/// server can omit `extensions` and inherit the built-in's set downstream.
fn optional_extension_array(value: Option<&Value>, index: usize) -> Result<Vec<String>, String> {
    let values = optional_string_array(value, index, "extensions")?;
    Ok(values
        .into_iter()
        .map(|value| value.trim_start_matches('.').to_string())
        .collect())
}

/// Like `required_string` for `binary` but treats an absent value as empty
/// (inherited from the built-in downstream). A present-but-blank value is still
/// rejected so typos like `"binary": ""` surface instead of silently inheriting.
fn optional_lsp_binary(value: Option<&Value>, index: usize) -> Result<String, String> {
    match value {
        None => Ok(String::new()),
        Some(value) => required_string(Some(value), index, "binary"),
    }
}

fn optional_string_array(
    value: Option<&Value>,
    index: usize,
    field: &str,
) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(entries) = value.as_array() else {
        return Err(format!(
            "configure: lsp_servers[{index}].{field} must be an array of strings"
        ));
    };

    let mut values = Vec::with_capacity(entries.len());
    for (entry_index, entry) in entries.iter().enumerate() {
        let Some(raw) = entry.as_str() else {
            return Err(format!(
                "configure: lsp_servers[{index}].{field}[{entry_index}] must be a string"
            ));
        };
        values.push(raw.trim().to_string());
    }
    Ok(values)
}

/// Parse the `lsp_paths_extra` config param: an array of absolute directory
/// paths the plugin wants AFT to search when resolving LSP binaries (used
/// for the auto-install cache, e.g.
/// `~/.cache/aft/lsp-packages/<pkg>/node_modules/.bin/`).
///
/// Rejects non-array values, non-string entries, empty strings, relative paths,
/// parent traversal, and existing paths that do not resolve to directories.
/// Non-existent paths are accepted silently — the resolver tolerates them and
/// falls through to the next candidate.
fn parse_lsp_paths_extra(value: &Value) -> Result<Vec<PathBuf>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "configure: lsp_paths_extra must be an array of strings".to_string())?;

    let mut paths = Vec::with_capacity(array.len());
    for (index, entry) in array.iter().enumerate() {
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("configure: lsp_paths_extra[{index}] must be a string"))?;
        if raw.is_empty() {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must not be empty"
            ));
        }
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must be an absolute path: {raw}"
            ));
        }
        if has_parent_component(&path) {
            return Err(format!(
                "configure: lsp_paths_extra[{index}] must not contain '..' traversal: {raw}"
            ));
        }

        match std::fs::canonicalize(&path) {
            Ok(canonical) => {
                if has_parent_component(&canonical) {
                    return Err(format!(
                        "configure: lsp_paths_extra[{index}] resolved path must not contain '..' traversal: {}",
                        canonical.display()
                    ));
                }
                if !canonical.is_dir() {
                    return Err(format!(
                        "configure: lsp_paths_extra[{index}] must resolve to a directory: {}",
                        canonical.display()
                    ));
                }
                paths.push(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                paths.push(path);
            }
            Err(error) => {
                return Err(format!(
                    "configure: lsp_paths_extra[{index}] could not be resolved: {error}"
                ));
            }
        }
    }
    Ok(paths)
}

fn parse_disabled_lsp(value: &Value) -> Result<std::collections::HashSet<String>, String> {
    let Some(entries) = value.as_array() else {
        return Err("configure: disabled_lsp must be an array of strings".to_string());
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry
                .as_str()
                .map(|value| value.to_ascii_lowercase())
                .ok_or_else(|| format!("configure: disabled_lsp[{index}] must be a string"))
        })
        .collect()
}

fn parse_string_set(
    value: &Value,
    field: &str,
) -> Result<std::collections::HashSet<String>, String> {
    let Some(entries) = value.as_array() else {
        return Err(format!("configure: {field} must be an array of strings"));
    };

    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry
                .as_str()
                .map(|value| value.to_string())
                .ok_or_else(|| format!("configure: {field}[{index}] must be a string"))
        })
        .collect()
}

fn is_custom_server(kind: &ServerKind) -> bool {
    matches!(kind, ServerKind::Custom(_))
}

fn lsp_missing_hint(binary: &str) -> String {
    crate::format::install_hint(binary)
}

fn lang_key(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => "typescript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
    }
}

fn has_project_config(project_root: Option<&Path>, filenames: &[&str]) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    filenames.iter().any(|file| root.join(file).exists())
}

fn has_pyproject_tool(project_root: Option<&Path>, tool_name: &str) -> bool {
    let Some(root) = project_root else {
        return false;
    };
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return false;
    }
    std::fs::read_to_string(pyproject)
        .map(|content| content.contains(&format!("[tool.{tool_name}]")))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct ConfigureToolCandidate {
    tool: String,
    source: String,
    required: bool,
}

fn configure_tool_candidate(tool: &str, source: &str, required: bool) -> ConfigureToolCandidate {
    ConfigureToolCandidate {
        tool: tool.to_string(),
        source: source.to_string(),
        required,
    }
}

fn explicit_formatter_candidate(name: &str) -> Vec<ConfigureToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "biome" | "oxfmt" | "prettier" | "deno" | "ruff" | "black" | "rustfmt" | "goimports"
        | "gofmt" => {
            vec![configure_tool_candidate(name, "formatter config", true)]
        }
        _ => Vec::new(),
    }
}

fn explicit_checker_candidate(name: &str) -> Vec<ConfigureToolCandidate> {
    match name {
        "none" | "off" | "false" => Vec::new(),
        "tsc" | "tsgo" | "cargo" | "go" | "biome" | "pyright" | "ruff" | "staticcheck" => {
            vec![configure_tool_candidate(name, "checker config", true)]
        }
        _ => Vec::new(),
    }
}

fn formatter_candidates(
    lang: LangId,
    config: &crate::config::Config,
) -> Vec<ConfigureToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.formatter.get(lang_key(lang)) {
        return explicit_formatter_candidate(preferred);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![configure_tool_candidate("biome", "biome.json", true)]
            } else if has_project_config(
                project_root,
                &[".oxfmtrc.json", ".oxfmtrc.jsonc", "oxfmt.config.ts"],
            ) {
                vec![configure_tool_candidate("oxfmt", "oxfmt config", true)]
            } else if has_project_config(
                project_root,
                &[
                    ".prettierrc",
                    ".prettierrc.json",
                    ".prettierrc.yml",
                    ".prettierrc.yaml",
                    ".prettierrc.js",
                    ".prettierrc.cjs",
                    ".prettierrc.mjs",
                    ".prettierrc.toml",
                    "prettier.config.js",
                    "prettier.config.cjs",
                    "prettier.config.mjs",
                ],
            ) {
                vec![configure_tool_candidate(
                    "prettier",
                    "Prettier config",
                    true,
                )]
            } else if has_project_config(project_root, &["deno.json", "deno.jsonc"]) {
                vec![configure_tool_candidate("deno", "deno.json", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else if has_pyproject_tool(project_root, "black") {
                vec![configure_tool_candidate("black", "pyproject.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![configure_tool_candidate("rustfmt", "Cargo.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    configure_tool_candidate("goimports", "go.mod", false),
                    configure_tool_candidate("gofmt", "go.mod", true),
                ]
            } else {
                Vec::new()
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Scss
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn checker_candidates(lang: LangId, config: &crate::config::Config) -> Vec<ConfigureToolCandidate> {
    let project_root = config.project_root.as_deref();
    if let Some(preferred) = config.checker.get(lang_key(lang)) {
        return explicit_checker_candidate(preferred);
    }

    match lang {
        LangId::TypeScript | LangId::JavaScript | LangId::Tsx => {
            if has_project_config(project_root, &["biome.json", "biome.jsonc"]) {
                vec![configure_tool_candidate("biome", "biome.json", true)]
            } else if has_project_config(project_root, &["tsconfig.json"]) {
                vec![configure_tool_candidate("tsc", "tsconfig.json", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Python => {
            if has_project_config(project_root, &["pyrightconfig.json"])
                || has_pyproject_tool(project_root, "pyright")
            {
                vec![configure_tool_candidate("pyright", "pyright config", true)]
            } else if has_project_config(project_root, &["ruff.toml", ".ruff.toml"])
                || has_pyproject_tool(project_root, "ruff")
            {
                vec![configure_tool_candidate("ruff", "ruff config", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Rust => {
            if has_project_config(project_root, &["Cargo.toml"]) {
                vec![configure_tool_candidate("cargo", "Cargo.toml", true)]
            } else {
                Vec::new()
            }
        }
        LangId::Go => {
            if has_project_config(project_root, &["go.mod"]) {
                vec![
                    configure_tool_candidate("staticcheck", "go.mod", false),
                    configure_tool_candidate("go", "go.mod", true),
                ]
            } else {
                Vec::new()
            }
        }
        LangId::C
        | LangId::Cpp
        | LangId::Zig
        | LangId::CSharp
        | LangId::Bash
        | LangId::Solidity
        | LangId::Scss
        | LangId::Vue
        | LangId::Json
        | LangId::Scala
        | LangId::Java
        | LangId::Ruby
        | LangId::Kotlin
        | LangId::Swift
        | LangId::Php
        | LangId::Lua
        | LangId::Perl => Vec::new(),
        LangId::Html | LangId::Markdown | LangId::Yaml => Vec::new(),
    }
}

fn resolve_tool_cached(
    tool: &str,
    project_root: Option<&Path>,
    cache: &mut HashMap<String, bool>,
) -> bool {
    if let Some(is_available) = cache.get(tool) {
        return *is_available;
    }

    let is_available = crate::format::tool_available_for_missing_warning(tool, project_root);
    cache.insert(tool.to_string(), is_available);
    is_available
}

fn should_warn_missing_formatters(config: &crate::config::Config, lang: LangId) -> bool {
    config.format_on_edit || config.formatter.contains_key(lang_key(lang))
}

fn should_warn_missing_checkers(config: &crate::config::Config, lang: LangId) -> bool {
    let mode = config.validate_on_edit.as_deref().unwrap_or("off");
    (mode == "syntax" || mode == "full") || config.checker.contains_key(lang_key(lang))
}

fn missing_tool_warning(
    kind: &str,
    language: &str,
    candidate: &ConfigureToolCandidate,
    project_root: Option<&Path>,
    tool_cache: &mut HashMap<String, bool>,
) -> Option<crate::format::MissingTool> {
    if !candidate.required || resolve_tool_cached(&candidate.tool, project_root, tool_cache) {
        return None;
    }

    Some(crate::format::MissingTool {
        kind: kind.to_string(),
        language: language.to_string(),
        tool: candidate.tool.clone(),
        // GitHub issue #47: word this so the user understands the tool may
        // be installed but missing from AFT's PATH (common with GUI-launched
        // editors that don't inherit a login shell). format.rs has the same
        // wording in `configured_tool_hint`.
        hint: format!(
            "{} is configured in {} but was not found on PATH or in common install locations. {}",
            candidate.tool,
            candidate.source,
            crate::format::install_hint(&candidate.tool)
        ),
    })
}

fn detect_missing_tools_for_languages(
    languages: &HashSet<LangId>,
    config: &crate::config::Config,
) -> Vec<crate::format::MissingTool> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut tool_cache = HashMap::new();

    for &lang in languages {
        let language = lang_key(lang);

        if should_warn_missing_formatters(config, lang) {
            for candidate in formatter_candidates(lang, config) {
                if let Some(warning) = missing_tool_warning(
                    "formatter_not_installed",
                    language,
                    &candidate,
                    config.project_root.as_deref(),
                    &mut tool_cache,
                ) {
                    if seen.insert((
                        warning.kind.clone(),
                        warning.language.clone(),
                        warning.tool.clone(),
                    )) {
                        warnings.push(warning);
                    }
                }
            }
        }

        if should_warn_missing_checkers(config, lang) {
            for candidate in checker_candidates(lang, config) {
                if let Some(warning) = missing_tool_warning(
                    "checker_not_installed",
                    language,
                    &candidate,
                    config.project_root.as_deref(),
                    &mut tool_cache,
                ) {
                    if seen.insert((
                        warning.kind.clone(),
                        warning.language.clone(),
                        warning.tool.clone(),
                    )) {
                        warnings.push(warning);
                    }
                }
            }
        }
    }

    warnings.sort_by(|left, right| {
        (&left.kind, &left.language, &left.tool).cmp(&(&right.kind, &right.language, &right.tool))
    });
    warnings
}

fn parse_validate_on_edit(raw: &Value) -> Result<String, String> {
    if let Some(value) = raw.as_bool() {
        return Ok(if value { "syntax" } else { "off" }.to_string());
    }

    let Some(value) = raw.as_str() else {
        return Err(
            "configure: validate_on_edit must be a boolean or one of 'off', 'syntax', 'full'"
                .to_string(),
        );
    };

    match value {
        "off" | "syntax" | "full" => Ok(value.to_string()),
        "true" | "false" => Err(
            "configure: validate_on_edit string booleans are not accepted; use a JSON boolean or one of 'off', 'syntax', 'full'"
                .to_string(),
        ),
        other => Err(format!(
            "configure: validate_on_edit must be one of 'off', 'syntax', 'full'; got '{other}'"
        )),
    }
}

fn parse_string_map(value: &Value, field: &str) -> Result<HashMap<String, String>, String> {
    let Some(object) = value.as_object() else {
        return Err(format!(
            "configure: {field} must be an object of string values"
        ));
    };

    let mut parsed = HashMap::with_capacity(object.len());
    for (key, raw_value) in object {
        let Some(value) = raw_value.as_str() else {
            return Err(format!("configure: {field}.{key} must be a string"));
        };
        parsed.insert(key.clone(), value.to_string());
    }
    Ok(parsed)
}

fn detect_missing_lsp_binaries(files: &[PathBuf], config: &crate::config::Config) -> Vec<Value> {
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut resolved_binaries = HashSet::new();
    let mut missing_binaries = HashSet::new();

    let project_root = config.project_root.as_deref();
    let extra_paths = &config.lsp_paths_extra;

    for file in files {
        for server in servers_for_file(&file, config) {
            if is_custom_server(&server.kind)
                || !seen.insert((server.kind.id_str().to_string(), server.binary.clone()))
            {
                continue;
            }

            if !config.lsp_auto_install_binaries.contains(&server.binary) {
                continue;
            }

            if config.lsp_inflight_installs.contains(&server.binary) {
                continue;
            }

            if !resolved_binaries.contains(&server.binary) {
                if resolve_lsp_binary(&server.binary, project_root, extra_paths).is_some() {
                    resolved_binaries.insert(server.binary.clone());
                } else {
                    missing_binaries.insert(server.binary.clone());
                }
            }

            if missing_binaries.contains(&server.binary) {
                warnings.push(json!({
                    "kind": "lsp_binary_missing",
                    "server": server.binary,
                    "binary": server.binary,
                    "hint": lsp_missing_hint(&server.binary),
                }));
            }
        }
    }

    for server in &config.lsp_servers {
        // A blank binary means "partial built-in override, inherit the built-in
        // binary" — the resolvable binary is already covered by the built-in
        // pass above, so skip the missing-binary probe here (probing "" never
        // resolves and would emit a bogus warning).
        if server.binary.is_empty() {
            continue;
        }

        if server.disabled || !seen.insert((server.id.clone(), server.binary.clone())) {
            continue;
        }

        if config.lsp_inflight_installs.contains(&server.binary) {
            continue;
        }

        if !resolved_binaries.contains(&server.binary) {
            if resolve_lsp_binary(&server.binary, project_root, extra_paths).is_some() {
                resolved_binaries.insert(server.binary.clone());
            } else {
                missing_binaries.insert(server.binary.clone());
            }
        }

        if missing_binaries.contains(&server.binary) {
            warnings.push(json!({
                "kind": "lsp_binary_missing",
                "server": server.id,
                "binary": server.binary,
                "hint": lsp_missing_hint(&server.binary),
            }));
        }
    }

    warnings.sort_by_key(|warning| warning.to_string());
    warnings
}

type SearchIndexSymbolFile = (PathBuf, SystemTime);

fn search_index_symbol_files(index: &SearchIndex) -> Vec<SearchIndexSymbolFile> {
    index
        .files
        .iter()
        .filter(|entry| !entry.path.as_os_str().is_empty())
        .map(|entry| (entry.path.clone(), entry.modified))
        .collect()
}

fn spawn_symbol_cache_prewarm(
    root: PathBuf,
    symbol_cache: SharedSymbolCache,
    symbol_storage: Option<PathBuf>,
    symbol_project_key: String,
    symbol_cache_generation: u64,
    symbol_files: Vec<SearchIndexSymbolFile>,
    is_worktree_bridge: bool,
    session_id: Option<String>,
) {
    thread::spawn(move || {
        log_ctx::with_session(session_id, || {
            prewarm_symbol_cache_from_search_files(
                root,
                symbol_cache,
                symbol_storage,
                symbol_project_key,
                symbol_cache_generation,
                symbol_files,
                is_worktree_bridge,
            );
        });
    });
}

fn prewarm_symbol_cache_from_search_files(
    root: PathBuf,
    symbol_cache: SharedSymbolCache,
    symbol_storage: Option<PathBuf>,
    symbol_project_key: String,
    symbol_cache_generation: u64,
    symbol_files: Vec<SearchIndexSymbolFile>,
    is_worktree_bridge: bool,
) {
    #[cfg(debug_assertions)]
    delay_symbol_prewarm_for_debug();

    let mut warmed_files = 0usize;
    let mut skipped_files = 0usize;
    if let Ok(mut cache) = symbol_cache.write() {
        if !cache.set_project_root_for_generation(symbol_cache_generation, root.clone()) {
            slog_info!("skipping stale symbol cache prewarm after reconfigure");
            return;
        }
        if let Some(storage_dir) = symbol_storage.as_deref() {
            let loaded_count = cache.load_from_disk_for_generation(
                symbol_cache_generation,
                storage_dir,
                &symbol_project_key,
                &root,
            );
            slog_info!("loaded symbol cache from disk: {} files", loaded_count);
        }
    } else {
        return;
    }

    let mut parser = crate::parser::FileParser::with_symbol_cache_generation(
        symbol_cache.clone(),
        Some(symbol_cache_generation),
    );
    for (path, modified) in &symbol_files {
        let cached = symbol_cache
            .read()
            .map(|cache| cache.contains_path_with_mtime(path, *modified))
            .unwrap_or(false);
        if cached {
            skipped_files += 1;
            continue;
        }
        if parser.extract_symbols(path).is_ok() {
            warmed_files += 1;
        }
    }

    let total_files = symbol_cache.read().map(|cache| cache.len()).unwrap_or(0);
    if !is_worktree_bridge {
        if let Some(storage_dir) = symbol_storage.as_deref() {
            if let Ok(cache) = symbol_cache.read() {
                if cache.generation() != symbol_cache_generation {
                    slog_info!("skipping stale symbol cache persistence after reconfigure");
                    return;
                }
                match crate::symbol_cache_disk::write_to_disk(
                    &cache,
                    storage_dir,
                    &symbol_project_key,
                ) {
                    Ok(()) => {
                        slog_info!("persisted symbol cache: {} files", cache.len());
                    }
                    Err(error) => {
                        slog_warn!("failed to persist symbol cache: {}", error);
                    }
                }
            }
        }
    }
    slog_info!(
        "pre-warmed symbol cache: {} new, {} cached, {} files total",
        warmed_files,
        skipped_files,
        total_files
    );
}

#[cfg(debug_assertions)]
fn delay_symbol_prewarm_for_debug() {
    let Some(delay_ms) = std::env::var("AFT_TEST_SYMBOL_PREWARM_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    else {
        return;
    };
    thread::sleep(Duration::from_millis(delay_ms));
}

fn walk_semantic_project_files_bounded(
    root: &Path,
    max_files: usize,
) -> Result<Vec<PathBuf>, usize> {
    let filters = build_path_filters(&[], &[]).unwrap_or_default();
    walk_project_files_bounded_matching(root, &filters, max_files, is_semantic_indexed_extension)
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

#[cfg(debug_assertions)]
fn mark_search_rebuild_spawn_for_debug() {
    let Some(path) = std::env::var_os("AFT_TEST_SEARCH_REBUILD_THREAD_MARKER") else {
        return;
    };
    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"spawned");
}

/// Handle a `configure` request.
///
/// Expects `project_root` (string, required) — absolute path to the project root.
/// Sets the project root on `Config`, initializes the `CallGraph` with that root,
/// spawns a file watcher for live invalidation, and returns success with the
/// configured path.
///
/// Stderr log: `[aft] project root set: <path>`
/// Stderr log: `[aft] watcher started: <path>`
pub fn handle_configure(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = req.params.get("params").unwrap_or(&req.params);
    let harness = match params.get("harness") {
        Some(raw) => match serde_json::from_value::<Harness>(raw.clone()) {
            Ok(harness) => harness,
            Err(_) => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "configure payload invalid field 'harness'; expected 'opencode' or 'pi'",
                );
            }
        },
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure payload missing required field 'harness'; expected 'opencode' or 'pi'",
            );
        }
    };
    let root = match params.get("project_root").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: missing required param 'project_root'",
            );
        }
    };

    let root_path = PathBuf::from(root);
    if !root_path.is_absolute() {
        return Response::error(
            &req.id,
            "invalid_request",
            "project_root must be an absolute path",
        );
    }
    if !root_path.is_dir() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("configure: project_root is not a directory: {}", root),
        );
    }
    let canonical_cache_root =
        std::fs::canonicalize(&root_path).unwrap_or_else(|_| root_path.clone());
    debug_assert!(canonical_cache_root.is_absolute());
    let (is_worktree_bridge, git_common_dir) = detect_worktree_bridge(&canonical_cache_root);

    let previous_config = ctx.config().clone();
    let previous_project_root = previous_config.project_root.clone();
    let mut next_config = previous_config.clone();
    next_config.project_root = Some(root_path.clone());
    next_config.harness = Some(harness);

    // Parse and validate every configure field into a temporary config first.
    // AppContext is mutated only after this phase succeeds, so an invalid late
    // field cannot leave the bridge half-configured.
    if let Some(v) = params.get("format_on_edit").and_then(|v| v.as_bool()) {
        next_config.format_on_edit = v;
    }
    if let Some(raw) = params.get("validate_on_edit") {
        let value = match parse_validate_on_edit(raw) {
            Ok(value) => value,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
        next_config.validate_on_edit = Some(value);
    }
    if let Some(v) = params.get("formatter") {
        next_config.formatter = match parse_string_map(v, "formatter") {
            Ok(formatter) => formatter,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params
        .get("restrict_to_project_root")
        .and_then(|v| v.as_bool())
    {
        next_config.restrict_to_project_root = v;
    }
    if let Some(raw) = params.get("formatter_timeout_secs") {
        let Some(v) = raw.as_u64() else {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "configure: formatter_timeout_secs must be in 1..=600, got {}",
                    raw
                ),
            );
        };
        if v == 0 || v > 600 {
            return Response::error(
                &req.id,
                "invalid_request",
                format!(
                    "configure: formatter_timeout_secs must be in 1..=600, got {}",
                    v
                ),
            );
        }
        next_config.formatter_timeout_secs = v as u32;
    }
    if let Some(v) = params.get("checker") {
        next_config.checker = match parse_string_map(v, "checker") {
            Ok(checker) => checker,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }

    if let Some(v) = params.get("search_index").and_then(|v| v.as_bool()) {
        next_config.search_index = v;
    }
    if let Some(v) = params.get("semantic_search").and_then(|v| v.as_bool()) {
        next_config.semantic_search = v;
    }
    if let Some(v) = params
        .get("experimental_bash_rewrite")
        .and_then(|v| v.as_bool())
    {
        next_config.experimental_bash_rewrite = v;
    }
    if let Some(v) = params
        .get("experimental_bash_compress")
        .and_then(|v| v.as_bool())
    {
        next_config.experimental_bash_compress = v;
    }
    if let Some(v) = params
        .get("experimental_bash_background")
        .and_then(|v| v.as_bool())
    {
        next_config.experimental_bash_background = v;
    }
    if let Some(v) = params.get("experimental_lsp_ty").and_then(|v| v.as_bool()) {
        next_config.experimental_lsp_ty = v;
    }
    if let Some(v) = params.get("lsp_servers") {
        next_config.lsp_servers = match parse_lsp_servers(v) {
            Ok(servers) => servers,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params.get("bash_permissions").and_then(|v| v.as_bool()) {
        next_config.bash_permissions = v;
    }
    if let Some(v) = params.get("disabled_lsp") {
        next_config.disabled_lsp = match parse_disabled_lsp(v) {
            Ok(disabled_lsp) => disabled_lsp,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params.get("lsp_paths_extra") {
        next_config.lsp_paths_extra = match parse_lsp_paths_extra(v) {
            Ok(paths) => paths,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params.get("lsp_auto_install_binaries") {
        next_config.lsp_auto_install_binaries =
            match parse_string_set(v, "lsp_auto_install_binaries") {
                Ok(binaries) => binaries,
                Err(error) => return Response::error(&req.id, "invalid_request", error),
            };
    }
    if let Some(v) = params.get("lsp_inflight_installs") {
        next_config.lsp_inflight_installs = match parse_string_set(v, "lsp_inflight_installs") {
            Ok(binaries) => binaries,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(v) = params
        .get("search_index_max_file_size")
        .and_then(|v| v.as_u64())
    {
        next_config.search_index_max_file_size = v;
    }
    if let Some(raw) = params.get("storage_dir") {
        let Some(value) = raw.as_str() else {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: storage_dir must be a string",
            );
        };
        next_config.storage_dir = match validate_storage_dir(value) {
            Ok(path) => Some(path),
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(raw) = params.get("url_fetch_allow_private") {
        let Some(value) = raw.as_bool() else {
            return Response::error(
                &req.id,
                "invalid_request",
                "configure: url_fetch_allow_private must be a boolean",
            );
        };
        next_config.url_fetch_allow_private = value;
    }
    if let Some(v) = params.get("semantic") {
        next_config.semantic = match parse_semantic_config(v, &next_config.semantic) {
            Ok(config) => config,
            Err(error) => return Response::error(&req.id, "invalid_request", error),
        };
    }
    if let Some(raw) = params.get("max_callgraph_files") {
        // Reject invalid values explicitly so user typos surface instead of
        // being silently swallowed (Oracle v0.15.1 review blocker).
        // Accepts: positive integers (u64).
        // Rejects: 0, negatives, non-integers, non-numbers.
        let parsed = raw.as_u64().filter(|v| *v >= 1);
        match parsed {
            Some(v) => next_config.max_callgraph_files = v as usize,
            None => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "max_callgraph_files must be a positive integer (>= 1); got {}",
                        raw
                    ),
                );
            }
        }
    }
    if let Some(raw) = params.get("max_background_bash_tasks") {
        let parsed = raw.as_u64().filter(|v| *v >= 1);
        match parsed.and_then(|v| usize::try_from(v).ok()) {
            Some(v) => next_config.max_background_bash_tasks = v,
            None => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "max_background_bash_tasks must be a positive integer (>= 1); got {}",
                        raw
                    ),
                );
            }
        }
    }
    if let Some(v) = params
        .get("bash_long_running_reminder_enabled")
        .and_then(|v| v.as_bool())
    {
        next_config.bash_long_running_reminder_enabled = v;
    }
    if let Some(raw) = params.get("bash_long_running_reminder_interval_ms") {
        let parsed = raw.as_u64().filter(|v| *v >= 1);
        match parsed {
            Some(v) => next_config.bash_long_running_reminder_interval_ms = v,
            None => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "bash_long_running_reminder_interval_ms must be a positive integer (>= 1); got {}",
                        raw
                    ),
                );
            }
        }
    }

    // Detect "this is not really a project root" scenarios before any walks
    // that traverse `project_root`.
    let mut degraded_reasons: Vec<String> = Vec::new();
    let home_match = resolve_home_dir().is_some_and(|home| home == canonical_cache_root);
    if home_match {
        degraded_reasons.push("home_root".to_string());
    }

    // `_bypass_size_limits` (set by `aft warmup --force`) lifts all three
    // file-count caps so a very large repo is fully indexed for measurement:
    // callgraph (`max_callgraph_files`), search index (`MAX_SEARCH_INDEX_FILES`),
    // and semantic (`semantic.max_files`). Not a user-facing config knob — it is
    // an internal benchmarking escape hatch. We raise to a large-but-safe value
    // (not usize::MAX) so the `+ 1` below cannot overflow.
    let bypass_size_limits = params
        .get("_bypass_size_limits")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if bypass_size_limits {
        const UNCAPPED: usize = 1_000_000_000;
        next_config.max_callgraph_files = next_config.max_callgraph_files.max(UNCAPPED);
        next_config.semantic.max_files = next_config.semantic.max_files.max(UNCAPPED);
    }

    // The cap-bounded count below uses `take(max + 1)` so it costs O(cap),
    // not O(project), and feeds both call-graph viability and search-index
    // auto-disable decisions.
    let max_callgraph_files = next_config.max_callgraph_files;
    // `--force`/bypass also lifts the hardcoded search-index file limit.
    let search_index_limit = if bypass_size_limits {
        usize::MAX - 1
    } else {
        MAX_SEARCH_INDEX_FILES
    };
    let (source_file_count, exceeds, exceeds_search_threshold) = if home_match {
        (max_callgraph_files + 1, true, true)
    } else {
        let walk_limit = max_callgraph_files
            .max(search_index_limit)
            .saturating_add(1);
        let count = crate::callgraph::walk_project_files(&root_path)
            .take(walk_limit)
            .count();
        let exceeds_cg = count > max_callgraph_files;
        let exceeds_search = count > search_index_limit;
        (count, exceeds_cg, exceeds_search)
    };
    if exceeds {
        slog_warn!(
            "project has >{} source files. Call-graph operations (callers, trace_to, trace_to_symbol, trace_data, impact) will be disabled. Open a specific subdirectory for call-graph features.",
            max_callgraph_files
        );
    }
    if exceeds_search_threshold && next_config.search_index {
        slog_warn!(
            "project has >{} source files. Search index auto-disabled — open a project subdirectory for grep/aft_search.",
            MAX_SEARCH_INDEX_FILES
        );
        next_config.search_index = false;
        degraded_reasons.push(format!("search_too_many_files:{}", MAX_SEARCH_INDEX_FILES));
    }

    if home_match {
        if next_config.search_index {
            next_config.search_index = false;
            slog_warn!(
                "search_index auto-disabled: project root is the user home directory \
                 ({}). Open a project subdirectory for full features.",
                canonical_cache_root.display()
            );
        }
        if next_config.semantic_search {
            next_config.semantic_search = false;
            slog_warn!(
                "semantic_search auto-disabled: project root is the user home directory \
                 ({}). Open a project subdirectory for full features.",
                canonical_cache_root.display()
            );
        }
    }

    if previous_project_root.as_ref() != Some(&root_path) {
        crate::format::clear_tool_cache();
    }

    // Commit phase: no validation returns after this point.
    *ctx.config_mut() = next_config.clone();
    ctx.set_harness(harness);
    ctx.backup().borrow().set_db_harness(harness);
    ctx.set_canonical_cache_root(canonical_cache_root.clone());
    ctx.set_cache_role(is_worktree_bridge, git_common_dir);
    ctx.reset_tier2_refresh_scheduler();
    ctx.backup()
        .borrow()
        .set_db_project_key(crate::search_index::project_cache_key(
            &canonical_cache_root,
        ));
    if let Some(storage_dir) = next_config.storage_dir.clone() {
        // Ensure the storage root directory exists so subsystems (trust,
        // backups, checkpoints, DB, persistence) can create their sub-trees
        // without a separate create_dir_all per subsystem. On fresh installs
        // this directory hasn't been created yet, and every subsystem
        // currently creates its own subdirectory lazily — but the root must
        // exist for status/diagnostics to report a valid path.
        if let Err(err) = fs::create_dir_all(&storage_dir) {
            slog_warn!(
                "failed to create storage directory {}: {}",
                storage_dir.display(),
                err
            );
        }
        ctx.backup().borrow_mut().set_storage_dir_for_harness(
            storage_dir,
            harness,
            next_config.checkpoint_ttl_hours,
        );
    }

    // Rebuild gitignore matcher used by the watcher event filter to honor the
    // user's `.gitignore` files instead of a hardcoded directory list. Skipped
    // entirely when `home_match` is true — the walk would traverse `$HOME`.
    if !home_match {
        ctx.rebuild_gitignore();
    } else {
        ctx.clear_gitignore();
    }

    let storage_root = crate::bash_background::storage_dir(next_config.storage_dir.as_deref());
    match crate::url_fetch::cleanup_url_cache(&storage_root) {
        Ok(0) => {}
        Ok(n) => slog_info!("URL cache cleanup: removed {} stale entries", n),
        Err(err) => slog_warn!("URL cache cleanup failed: {}", err),
    }
    let db_path = storage_root.join("aft.db");
    match crate::db::open(&db_path) {
        Ok(conn) => {
            let shared = Arc::new(Mutex::new(conn));
            ctx.set_db(shared.clone());
            ctx.backup().borrow().set_db_pool(shared.clone());
            ctx.bash_background().set_db_pool(shared);
        }
        Err(err) => {
            ctx.clear_db();
            ctx.backup().borrow().clear_db_pool();
            ctx.bash_background().clear_db_pool();
            slog_warn!(
                "failed to open aft.db at {}: {} — running with JSON-only persistence",
                db_path.display(),
                err
            );
        }
    }
    match crate::migrate_storage::cleanup_staging_dirs(&storage_root, harness) {
        Ok(0) => {}
        Ok(n) => slog_info!(
            "swept {} staging directory orphans from prior migrations",
            n
        ),
        Err(err) => slog_warn!(
            "staging cleanup failed: {} (will retry next configure)",
            err
        ),
    }
    ctx.bash_background().configure_long_running_reminders(
        next_config.bash_long_running_reminder_enabled,
        next_config.bash_long_running_reminder_interval_ms,
    );

    let search_index = ctx.config().search_index;
    let semantic_search = ctx.config().semantic_search;
    let search_index_max_file_size = ctx.config().search_index_max_file_size;
    let semantic_config = ctx.config().semantic.clone();

    let search_build_in_progress = ctx.search_index_rx().borrow().is_some();
    let semantic_build_in_progress = ctx.semantic_index_rx().borrow().is_some();
    // Note: We intentionally only WARN on rapid reconfigure (rather than tracking
    // JoinHandles to cancel old threads) because:
    //   1. Old thread results are dropped when ctx.search_index_rx() is reset
    //   2. Atomic tempfile writes via std::fs::rename are race-safe (last writer wins)
    //   3. Only CPU is wasted; no correctness issue
    //   4. Tracking handles would add complexity for negligible benefit
    // If reconfigure rate becomes a real problem, switch to a single
    // generation-counter + cancellation-token pattern.
    if search_build_in_progress {
        slog_warn!(
            "configure called while search index build is still in progress; previous build will continue detached"
        );
    }
    if semantic_build_in_progress {
        slog_warn!(
            "configure called while semantic index build is still in progress; previous build will continue detached"
        );
    }

    *ctx.search_index().borrow_mut() = None;
    *ctx.search_index_rx().borrow_mut() = None;
    let symbol_cache_generation = ctx.reset_symbol_cache();
    *ctx.semantic_index().borrow_mut() = None;
    *ctx.semantic_index_rx().borrow_mut() = None;
    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Disabled;
    ctx.clear_semantic_refresh_worker();
    *ctx.semantic_embedding_model().borrow_mut() = None;
    ctx.clear_pending_index_updates();

    // Snapshot accumulated degraded reasons on the context so status /
    // sidebar / future tool calls all see the same state. Reasons emitted
    // synchronously so far: `home_root` and `search_too_many_files:N`.
    // The semantic-build thread may push its own "skipped — too many files"
    // status downstream; we don't yet thread that back into the persistent
    // reasons list because semantic auto-skip is already surfaced through
    // `SemanticIndexStatus::Failed`. If that ever becomes inconsistent UX
    // we can wire it through a channel; for now status snapshot sources
    // semantic state from the live SemanticIndexStatus, not the reasons.
    ctx.set_degraded_reasons(degraded_reasons.clone());

    let storage_dir = ctx.config().storage_dir.clone();
    let mut search_index_cache_reused = false;

    if search_index {
        let cache_dir = resolve_cache_dir(&canonical_cache_root, storage_dir.as_deref());
        let current_head = current_git_head(&canonical_cache_root);
        let baseline = SearchIndex::read_from_disk(&cache_dir, &canonical_cache_root);
        search_index_cache_reused = baseline.is_some();

        let root_for_prewarm = canonical_cache_root.clone();
        let symbol_cache = ctx.symbol_cache();
        let symbol_storage = storage_dir.clone();
        let symbol_project_key = project_cache_key(&canonical_cache_root);
        let is_worktree_bridge_for_search = is_worktree_bridge;
        let session_id_for_bg = log_ctx::current_session();

        match baseline {
            Some(mut index) if index.stored_git_head() == current_head.as_deref() => {
                index.verify_against_disk(current_head.clone());
                let symbol_files = search_index_symbol_files(&index);
                *ctx.search_index().borrow_mut() = Some(index);
                spawn_symbol_cache_prewarm(
                    root_for_prewarm,
                    symbol_cache,
                    symbol_storage,
                    symbol_project_key,
                    symbol_cache_generation,
                    symbol_files,
                    is_worktree_bridge_for_search,
                    session_id_for_bg,
                );
            }
            mut baseline => {
                if let Some(index) = baseline.as_mut() {
                    index.set_ready(false);
                    *ctx.search_index().borrow_mut() = Some(index.clone());
                }

                let (tx, rx): (
                    crossbeam_channel::Sender<SearchIndex>,
                    crossbeam_channel::Receiver<SearchIndex>,
                ) = unbounded();
                *ctx.search_index_rx().borrow_mut() = Some(rx);

                #[cfg(debug_assertions)]
                mark_search_rebuild_spawn_for_debug();

                let root_clone = canonical_cache_root.clone();
                thread::spawn(move || {
                    let session_id_for_prewarm = session_id_for_bg.clone();
                    log_ctx::with_session(session_id_for_bg, || {
                        let index = {
                            let _cache_lock = if is_worktree_bridge_for_search {
                                None
                            } else {
                                match CacheLock::acquire(&cache_dir) {
                                    Ok(lock) => Some(lock),
                                    Err(error) => {
                                        slog_warn!(
                                            "failed to acquire search cache lock: {}",
                                            error
                                        );
                                        None
                                    }
                                }
                            };
                            let index = SearchIndex::rebuild_or_refresh(
                                &root_clone,
                                search_index_max_file_size,
                                current_head,
                                baseline,
                            );
                            delay_search_rebuild_publish_for_debug();
                            if !is_worktree_bridge_for_search {
                                index.write_to_disk(&cache_dir, index.stored_git_head());
                            }
                            index
                        };

                        let symbol_files = search_index_symbol_files(&index);
                        let _ = tx.send(index);
                        spawn_symbol_cache_prewarm(
                            root_clone,
                            symbol_cache,
                            symbol_storage,
                            symbol_project_key,
                            symbol_cache_generation,
                            symbol_files,
                            is_worktree_bridge_for_search,
                            session_id_for_prewarm,
                        );
                    });
                });
            }
        }
    }

    if semantic_search {
        let semantic_initial_stage = if previous_config.semantic_search
            && previous_project_root.as_deref() == Some(root_path.as_path())
            && semantic_fingerprint_config_changed(&previous_config.semantic, &semantic_config)
        {
            "fingerprint_change"
        } else {
            "initial"
        };
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
            stage: semantic_initial_stage.to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
            stats: None,
        };
        let (tx, rx): (
            crossbeam_channel::Sender<SemanticIndexEvent>,
            crossbeam_channel::Receiver<SemanticIndexEvent>,
        ) = unbounded();
        *ctx.semantic_index_rx().borrow_mut() = Some(rx);

        let (refresh_tx, refresh_rx) = unbounded::<SemanticRefreshRequest>();
        let (refresh_event_tx, refresh_event_rx) = unbounded::<SemanticRefreshEvent>();
        let refresh_worker_slot: SemanticRefreshWorkerSlot = Arc::new(Mutex::new(None));
        ctx.install_semantic_refresh_worker(
            refresh_tx,
            refresh_event_rx,
            Arc::clone(&refresh_worker_slot),
        );

        let root_clone = canonical_cache_root.clone();
        let semantic_storage = storage_dir.clone();
        let semantic_project_key = crate::search_index::project_cache_key(&canonical_cache_root);
        let semantic_config = semantic_config.clone();
        let tx_progress = tx.clone();
        let is_worktree_bridge_for_semantic = is_worktree_bridge;
        let session_id_for_bg2 = log_ctx::current_session();
        thread::spawn(move || {
            log_ctx::with_session(session_id_for_bg2, || {
                // Cap file count to bound memory on huge project roots (e.g.,
                // /home/user). The local fastembed model (~200MB) + embeddings +
                // batch buffers can exceed memory on constrained systems when
                // indexing tens of thousands of files. Configurable via
                // `semantic.max_files` (default 20k); remote backends that embed
                // server-side can raise it freely.
                let max_semantic_files = semantic_config.max_files;

                let build_result = catch_unwind(AssertUnwindSafe(
                    || -> Result<(SemanticIndex, crate::semantic_index::EmbeddingModel), String> {
                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "initializing_embedding_model".to_string(),
                            files: None,
                            entries_done: None,
                            entries_total: None,
                            stats: None,
                        });
                        let mut model =
                            crate::semantic_index::EmbeddingModel::from_config(&semantic_config)?;
                        let fingerprint = model.fingerprint(&semantic_config)?;
                        let fingerprint_key = fingerprint.as_string();
                        let _semantic_cache_lock = (!is_worktree_bridge_for_semantic)
                            .then(|| ())
                            .and_then(|_| semantic_storage.as_ref())
                            .and_then(|dir| {
                                match SemanticIndexLock::acquire(dir, &semantic_project_key) {
                                    Ok(lock) => Some(lock),
                                    Err(error) => {
                                        slog_warn!(
                                            "failed to acquire semantic cache lock: {}",
                                            error
                                        );
                                        None
                                    }
                                }
                            });

                        if let Some(ref dir) = semantic_storage {
                            if let Some(cached) = SemanticIndex::read_from_disk(
                                dir,
                                &semantic_project_key,
                                &root_clone,
                                is_worktree_bridge_for_semantic,
                                Some(&fingerprint_key),
                            ) {
                                // Try incremental refresh: re-embed only changed/new files,
                                // drop entries for deleted files, keep everything else.
                                // This is the hot path for restart on a project with a
                                // handful of edits — avoids re-embedding 4000+ unchanged
                                // files just to pick up 10 changes.
                                let current_files = match walk_semantic_project_files_bounded(
                                    &root_clone,
                                    max_semantic_files,
                                ) {
                                    Ok(files) => files,
                                    Err(observed) => {
                                        slog_warn!(
                                            "skipping semantic index: more than {} files exceeds limit of {}. \
                                             Raise semantic.max_files or open a specific project directory.",
                                            observed.saturating_sub(1),
                                            max_semantic_files
                                        );
                                        return Err(format!(
                                            "too many files (>{}) for semantic indexing (max {})",
                                            max_semantic_files, max_semantic_files
                                        ));
                                    }
                                };

                                let mut cached = cached;
                                let mut embed = |texts: Vec<String>| model.embed(texts);
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "refreshing_stale_files".to_string(),
                                    files: None,
                                    entries_done: None,
                                    entries_total: None,
                                    stats: None,
                                });
                                let mut progress = |done: usize, total: usize| {
                                    let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                        stage: "embedding_stale_symbols".to_string(),
                                        files: None,
                                        entries_done: Some(done),
                                        entries_total: Some(total),
                                        stats: None,
                                    });
                                };

                                match cached.refresh_stale_files(
                                    &root_clone,
                                    &current_files,
                                    &mut embed,
                                    semantic_config.max_batch_size.max(1),
                                    &mut progress,
                                ) {
                                    Ok(summary) => {
                                        if summary.is_noop() {
                                            slog_info!(
                                                "semantic index: cached index is current ({} entries)",
                                                cached.entry_count(),
                                            );
                                        } else {
                                            slog_info!(
                                                "semantic index: refreshed incrementally — {} changed, {} new, {} deleted, {} total processed (kept {} cached)",
                                                summary.changed,
                                                summary.added,
                                                summary.deleted,
                                                summary.total_processed,
                                                cached.len(),
                                            );
                                            cached.set_fingerprint(fingerprint);
                                            if !is_worktree_bridge_for_semantic {
                                                if let Some(ref dir) = semantic_storage {
                                                    cached
                                                        .write_to_disk(dir, &semantic_project_key);
                                                }
                                            }
                                        }
                                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                            stage: "loaded_cached_index".to_string(),
                                            files: None,
                                            entries_done: Some(cached.entry_count()),
                                            entries_total: Some(cached.entry_count()),
                                            stats: Some(semantic_progress_stats_with_cache(
                                                &cached,
                                                cached.serialized_size_estimate(),
                                            )),
                                        });
                                        return Ok((cached, model));
                                    }
                                    Err(error) => {
                                        // Hard failure (dimension mismatch, embed backend
                                        // error). Drop the cache and do a full rebuild.
                                        slog_warn!(
                                            "incremental refresh failed ({}), falling back to full rebuild",
                                            error
                                        );
                                    }
                                }
                            }
                        }

                        let files = match walk_semantic_project_files_bounded(
                            &root_clone,
                            max_semantic_files,
                        ) {
                            Ok(files) => {
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "scanned_project_files".to_string(),
                                    files: Some(files.len()),
                                    entries_done: None,
                                    entries_total: None,
                                    stats: None,
                                });
                                files
                            }
                            Err(observed) => {
                                let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                    stage: "scanned_project_files".to_string(),
                                    files: Some(observed),
                                    entries_done: None,
                                    entries_total: None,
                                    stats: None,
                                });
                                slog_warn!(
                                    "skipping semantic index: more than {} files exceeds limit of {}. \
                                     Raise semantic.max_files or open a specific project directory.",
                                    observed.saturating_sub(1),
                                    max_semantic_files
                                );
                                return Err(format!(
                                    "too many files (>{}) for semantic indexing (max {})",
                                    max_semantic_files, max_semantic_files
                                ));
                            }
                        };

                        let mut embed = |texts: Vec<String>| model.embed(texts);

                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "extracting_symbols".to_string(),
                            files: Some(files.len()),
                            entries_done: None,
                            entries_total: None,
                            stats: None,
                        });
                        let mut progress = |done: usize, total: usize| {
                            let _ = tx_progress.send(SemanticIndexEvent::Progress {
                                stage: "embedding_symbols".to_string(),
                                files: Some(files.len()),
                                entries_done: Some(done),
                                entries_total: Some(total),
                                stats: None,
                            });
                        };
                        let index = SemanticIndex::build_with_progress(
                            &root_clone,
                            &files,
                            &mut embed,
                            semantic_config.max_batch_size.max(1),
                            &mut progress,
                        )?;
                        let mut index = index;
                        index.set_fingerprint(fingerprint);
                        slog_info!(
                            "built semantic index: {} files, {} entries",
                            files.len(),
                            index.len()
                        );
                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "persisting_index".to_string(),
                            files: Some(files.len()),
                            entries_done: Some(index.len()),
                            entries_total: Some(index.len()),
                            stats: Some(semantic_progress_stats_with_cache(
                                &index,
                                index.serialized_size_estimate(),
                            )),
                        });

                        if !is_worktree_bridge_for_semantic {
                            if let Some(ref dir) = semantic_storage {
                                index.write_to_disk(dir, &semantic_project_key);
                            }
                        }

                        Ok((index, model))
                    },
                ));

                let event = match build_result {
                    Ok(Ok((index, model))) => {
                        let clone_estimate = index.memory_estimate();
                        let clone_started = Instant::now();
                        let worker_index = index.clone();
                        slog_info!(
                            "semantic index clone for refresh worker: {} entries, {} estimated payload bytes, {} ms",
                            clone_estimate.entries,
                            clone_estimate.estimated_payload_bytes,
                            clone_started.elapsed().as_millis(),
                        );
                        let _ = tx_progress.send(SemanticIndexEvent::Progress {
                            stage: "starting_refresh_worker".to_string(),
                            files: Some(clone_estimate.indexed_files),
                            entries_done: Some(clone_estimate.entries),
                            entries_total: Some(clone_estimate.entries),
                            stats: Some(semantic_clone_stats_from_estimate(clone_estimate)),
                        });
                        let worker_handle = spawn_semantic_refresh_worker(
                            root_clone.clone(),
                            worker_index,
                            model,
                            semantic_config.max_batch_size.max(1),
                            semantic_config.max_files,
                            refresh_rx,
                            refresh_event_tx,
                            log_ctx::current_session(),
                        );
                        if let Ok(mut slot) = refresh_worker_slot.lock() {
                            *slot = Some(worker_handle);
                        }
                        SemanticIndexEvent::Ready(index)
                    }
                    Ok(Err(error)) => {
                        slog_warn!("failed to build semantic index: {}", error);
                        SemanticIndexEvent::Failed(error)
                    }
                    Err(_) => {
                        let error = "semantic index build panicked".to_string();
                        slog_warn!("{}", error);
                        SemanticIndexEvent::Failed(error)
                    }
                };

                let _ = tx.send(event);
            });
        });
    }

    // Initialize call graph with the project root
    let graph = CallGraph::new(root_path.clone());
    *ctx.callgraph().borrow_mut() = Some(graph);

    let bg_storage_root = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
    crate::bash_background::repair_legacy_root_tasks(&bg_storage_root, harness);
    let bg_storage_dir = ctx.harness_dir();
    if let Err(error) =
        ctx.bash_background()
            .replay_session_for_project(&bg_storage_dir, req.session(), &root_path)
    {
        slog_warn!("failed to replay background bash tasks: {error}");
    }

    // Spawn file watcher for live invalidation off the configure foreground.
    // FSEvents startup can synchronously wait for seconds on very large roots;
    // configure should return while the watcher attaches in the background.
    install_project_watcher(ctx, &canonical_cache_root);

    slog_info!("project root set: {}", root_path.display());

    // Sync compression/filter state before snapshotting the async warning worker.
    ctx.sync_bash_compress_flag();
    ctx.reset_filter_registry();

    let configure_generation = ctx.advance_configure_generation();
    let config_snapshot = ctx.config().clone();

    // Defer the full source-file walk + language detection +
    // formatter/checker/LSP missing-binary detection to a background thread.
    // On a normal project this finishes in <1 s and pushes a
    // `ConfigureWarningsFrame` for the plugin to surface; on a huge directory
    // it may take seconds-to-minutes, but configure itself returns now.
    if ctx.progress_sender_handle().is_some() {
        let warning_tx = ctx.configure_warnings_sender();
        let warning_generation = configure_generation;
        let walk_root = root_path.clone();
        let max_files = config_snapshot.max_callgraph_files;
        let project_root_display = root_path.display().to_string();
        let config_for_bg = config_snapshot.clone();
        let session_id_for_bg = log_ctx::current_session();
        let session_id_for_frame = session_id_for_bg.clone();
        thread::spawn(move || {
            log_ctx::with_session(session_id_for_bg, || {
                let source_files: Vec<PathBuf> =
                    crate::callgraph::walk_project_files(&walk_root).collect();
                let detected_languages: HashSet<LangId> = source_files
                    .iter()
                    .filter_map(|path| detect_language(path))
                    .collect();
                let full_count = source_files.len();
                let full_exceeds = full_count > max_files;

                let mut warnings =
                    detect_missing_tools_for_languages(&detected_languages, &config_for_bg)
                        .into_iter()
                        .map(|warning| json!(warning))
                        .collect::<Vec<_>>();
                warnings.extend(detect_missing_lsp_binaries(&source_files, &config_for_bg));

                let frame = crate::protocol::ConfigureWarningsFrame::new_with_session_id(
                    session_id_for_frame,
                    project_root_display,
                    full_count,
                    full_exceeds,
                    max_files,
                    warnings,
                );
                let _ = warning_tx.send((warning_generation, frame));
            });
        });
    }

    // Configure now returns immediately. The plugin should treat the response
    // as the "configured" signal and listen for a follow-up
    // `configure_warnings` push frame for missing-binary warnings and the
    // accurate file count. The bounded source_file_count below is good
    // enough for an early "is this project too big for callgraph" hint.
    let response = Response::success(
        &req.id,
        json!({
            "project_root": root_path.display().to_string(),
            "source_file_count": source_file_count,
            "source_file_count_exceeds_max": exceeds,
            "max_callgraph_files": config_snapshot.max_callgraph_files,
            "source_file_count_bounded": true,
            "warnings": [],
            "warnings_pending": true,
            "search_index_cache_reused": search_index_cache_reused,
        }),
    );
    ctx.status_emitter().signal(ctx.build_status_snapshot());
    response
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    #[cfg(unix)]
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    use super::{
        install_project_watcher_with, parse_lsp_paths_extra, parse_semantic_config,
        validate_storage_dir, WATCHER_GENERATION,
    };
    use crate::config::{Config, SemanticBackendConfig};
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;

    fn test_context() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    fn configure_request(project_root: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({ "project_root": project_root, "harness": "opencode" }),
        }
    }

    fn configure_request_with_params(params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    #[test]
    fn configure_without_harness_returns_invalid_request() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({ "project_root": temp.path() }));

        let response = super::handle_configure(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
        assert_eq!(
            response.data["message"],
            "configure payload missing required field 'harness'; expected 'opencode' or 'pi'"
        );
    }

    #[test]
    fn configure_with_invalid_harness_returns_invalid_request() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "claude_code"
        }));

        let response = super::handle_configure(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
    }

    #[test]
    fn harness_set_on_appcontext_after_configure() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": temp.path(),
            "harness": "pi"
        }));

        let response = super::handle_configure(&req, &ctx);

        assert!(response.success);
        assert_eq!(ctx.harness(), crate::harness::Harness::Pi);
        assert_eq!(ctx.config().harness, Some(crate::harness::Harness::Pi));
    }

    #[test]
    fn handle_configure_rejects_relative_project_root() {
        let ctx = test_context();
        let req = configure_request(json!("relative/path"));

        let response = super::handle_configure(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
    }

    #[test]
    fn handle_configure_populates_canonical_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request(json!(temp.path()));

        let response = super::handle_configure(&req, &ctx);

        assert!(response.success);
        assert_eq!(
            ctx.canonical_cache_root(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
        assert_eq!(ctx.cache_role(), "main");
    }

    #[test]
    fn parse_semantic_config_clamps_expensive_limits() {
        let parsed = super::parse_semantic_config(
            &json!({
                "timeout_ms": 999_999_999_u64,
                "max_batch_size": 999_999_999_u64,
            }),
            &SemanticBackendConfig::default(),
        )
        .expect("parse semantic config");

        assert_eq!(parsed.timeout_ms, super::MAX_SEMANTIC_TIMEOUT_MS);
        assert_eq!(parsed.max_batch_size, super::MAX_SEMANTIC_BATCH_SIZE);
    }

    #[test]
    fn semantic_file_cap_counts_only_semantic_extensions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn one() {}\n").unwrap();
        for index in 0..5 {
            std::fs::write(
                temp.path().join(format!("asset-{index}.bin")),
                format!("asset {index}"),
            )
            .unwrap();
        }

        let files = super::walk_semantic_project_files_bounded(temp.path(), 1)
            .expect("one semantic file should be within cap");
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src/lib.rs"));

        std::fs::write(temp.path().join("src/second.rs"), "pub fn two() {}\n").unwrap();
        assert!(super::walk_semantic_project_files_bounded(temp.path(), 1).is_err());
    }

    #[test]
    fn configure_missing_tools_warns_for_explicit_oxfmt_formatter() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "oxfmt".to_string());
        let candidates = super::formatter_candidates(crate::parser::LangId::TypeScript, &config);
        assert_eq!(candidates.len(), 1);
        let mut tool_cache = std::collections::HashMap::from([("oxfmt".to_string(), false)]);
        let warning = super::missing_tool_warning(
            "formatter_not_installed",
            "typescript",
            &candidates[0],
            config.project_root.as_deref(),
            &mut tool_cache,
        )
        .expect("expected missing oxfmt warning");

        assert_eq!(warning.kind, "formatter_not_installed");
        assert_eq!(warning.language, "typescript");
        assert_eq!(warning.tool, "oxfmt");
    }

    #[test]
    fn detect_missing_tools_skips_formatters_when_format_on_edit_disabled() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("biome.json"), "{}\n").unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        };
        let languages = std::collections::HashSet::from([crate::parser::LangId::TypeScript]);
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);
        assert!(
            warnings.is_empty(),
            "format_on_edit:false should suppress derived formatter warnings: {warnings:?}"
        );
    }

    #[test]
    fn detect_missing_tools_still_warns_explicit_formatter_when_format_on_edit_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config {
            project_root: Some(temp.path().to_path_buf()),
            format_on_edit: false,
            ..Config::default()
        };
        config
            .formatter
            .insert("typescript".to_string(), "biome".to_string());
        let languages = std::collections::HashSet::from([crate::parser::LangId::TypeScript]);
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].tool, "biome");
    }

    #[test]
    fn configure_missing_tools_warns_for_oxfmt_project_config() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".oxfmtrc.json"), "{}\n").unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };

        let candidates = super::formatter_candidates(crate::parser::LangId::TypeScript, &config);
        assert_eq!(candidates.len(), 1);
        let mut tool_cache = std::collections::HashMap::from([("oxfmt".to_string(), false)]);
        let warning = super::missing_tool_warning(
            "formatter_not_installed",
            "typescript",
            &candidates[0],
            config.project_root.as_deref(),
            &mut tool_cache,
        )
        .expect("expected missing oxfmt warning");

        assert_eq!(warning.kind, "formatter_not_installed");
        assert_eq!(warning.language, "typescript");
        assert_eq!(warning.tool, "oxfmt");
    }

    #[cfg(unix)]
    #[test]
    fn configure_missing_tools_uses_shared_go_tool_resolution() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("go.mod"), "module example.test\ngo 1.21\n").unwrap();
        let bin_dir = temp.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;

        let go = bin_dir.join("go");
        std::fs::write(
            &go,
            "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then exit 0; fi\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&go, std::fs::Permissions::from_mode(0o755)).unwrap();

        let gofmt = bin_dir.join("gofmt");
        std::fs::write(
            &gofmt,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 2; fi\ncat >/dev/null\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&gofmt, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut languages = std::collections::HashSet::new();
        languages.insert(crate::parser::LangId::Go);
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        let warnings = super::detect_missing_tools_for_languages(&languages, &config);

        assert!(
            warnings.is_empty(),
            "expected shared Go resolver to avoid false missing-tool warnings, got {warnings:?}"
        );
    }

    /// Shared mutex serializing the home-root tests below. Both tests
    /// mutate process-global `HOME` / `USERPROFILE` env vars, and `cargo
    /// test` runs unit tests concurrently within the same process — without
    /// serialization a parallel `set_var("HOME", X)` in test A can race
    /// `resolve_home_dir()` in test B and produce flaky failures.
    fn home_env_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn handle_configure_enters_degraded_mode_when_project_root_is_home() {
        let _guard = home_env_mutex().lock().unwrap();
        // Simulate the Desktop-launches-from-`~` case by pointing `HOME` at a
        // tempdir and using that same tempdir as `project_root`. The
        // canonical-equality check inside `handle_configure` is the same
        // mechanism that catches real `$HOME` regardless of HOME mutation.
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();

        // Save + restore HOME so we don't pollute other tests in the
        // same process (Rust runs tests in parallel by default but env
        // mutation is process-global). For Windows, USERPROFILE is the
        // var `resolve_home_dir` checks after HOME.
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        // SAFETY: env mutation is sound here — this is single-threaded test
        // setup. The matching restore at the bottom runs even on assertion
        // failure because Rust's panic path executes drop order, but we make
        // the env writes explicit for clarity.
        unsafe {
            std::env::set_var("HOME", &canonical);
            std::env::set_var("USERPROFILE", &canonical);
        }

        let ctx = test_context();
        ctx.config_mut().search_index = true;
        ctx.config_mut().semantic_search = true;
        let req = configure_request(json!(temp.path()));
        let response = super::handle_configure(&req, &ctx);

        // Restore env immediately so a later assertion failure doesn't leak.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }

        assert!(response.success);
        assert!(ctx.is_degraded(), "expected degraded mode for HOME root");
        assert!(
            ctx.degraded_reasons().contains(&"home_root".to_string()),
            "expected `home_root` reason, got {:?}",
            ctx.degraded_reasons()
        );
        // Heavy subsystems must have been force-disabled regardless of user config.
        assert!(
            !ctx.config().search_index,
            "search_index must be auto-disabled at HOME root"
        );
        assert!(
            !ctx.config().semantic_search,
            "semantic_search must be auto-disabled at HOME root"
        );
    }

    #[test]
    fn handle_configure_stays_full_featured_for_subdirectory_of_home() {
        let _guard = home_env_mutex().lock().unwrap();
        // A real subdirectory of `$HOME` (the legitimate case: most projects
        // live under `~/Work`, `~/Documents`, etc.) must NOT trip the
        // degraded gate. We point HOME at a tempdir and configure against a
        // nested subdir to confirm subdirs pass through.
        let temp = tempfile::tempdir().unwrap();
        let subdir = temp.path().join("project");
        std::fs::create_dir(&subdir).unwrap();
        let canonical_home = std::fs::canonicalize(temp.path()).unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &canonical_home);
            std::env::set_var("USERPROFILE", &canonical_home);
        }

        let ctx = test_context();
        ctx.config_mut().search_index = true;
        let req = configure_request(json!(subdir));
        let response = super::handle_configure(&req, &ctx);

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }

        assert!(response.success);
        assert!(
            !ctx.is_degraded(),
            "subdirectories of $HOME must not enter degraded mode"
        );
        assert!(
            ctx.degraded_reasons().is_empty(),
            "expected no degraded reasons, got {:?}",
            ctx.degraded_reasons()
        );
        // User config preserved.
        assert!(ctx.config().search_index);
    }

    #[test]
    fn parse_lsp_server_preserves_dotted_root_markers() {
        let value = json!([
            {
                "id": "oxlint-cli",
                "extensions": [".ts", "tsx"],
                "binary": "oxlint",
                "args": ["--lsp", ".keep-dotted-arg"],
                "root_markers": [".oxlintrc.json", ".oxlintrc"]
            }
        ]);

        let servers = super::parse_lsp_servers(&value).expect("parse lsp servers");

        assert_eq!(servers[0].extensions, vec!["ts", "tsx"]);
        assert_eq!(servers[0].args, vec!["--lsp", ".keep-dotted-arg"]);
        assert_eq!(servers[0].root_markers, vec![".oxlintrc.json", ".oxlintrc"]);
    }

    // A partial override of a built-in server (only `args`/`binary`) must parse
    // successfully with empty extensions/binary inherited downstream — requiring
    // them used to drop the entire `lsp` config section silently.
    #[test]
    fn parse_lsp_server_allows_partial_builtin_override() {
        let value = json!([
            {
                "id": "rust",
                "args": ["--extra-flag"]
            }
        ]);

        let servers = super::parse_lsp_servers(&value).expect("partial override should parse");
        assert_eq!(servers[0].id, "rust");
        assert!(servers[0].extensions.is_empty());
        assert!(servers[0].binary.is_empty());
        assert_eq!(servers[0].args, vec!["--extra-flag"]);
    }

    // A present-but-blank binary is still rejected — that's a typo, not an
    // intentional inherit (which is expressed by omitting the field entirely).
    #[test]
    fn parse_lsp_server_rejects_blank_binary() {
        let value = json!([{ "id": "rust", "binary": "  " }]);
        assert!(super::parse_lsp_servers(&value).is_err());
    }

    #[cfg(unix)]
    fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(windows)]
    fn create_dir_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::windows::fs::symlink_dir(src, dst).unwrap();
    }

    #[cfg(unix)]
    fn create_file_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::unix::fs::symlink(src, dst).unwrap();
    }

    #[cfg(windows)]
    fn create_file_symlink(src: &std::path::Path, dst: &std::path::Path) {
        std::os::windows::fs::symlink_file(src, dst).unwrap();
    }

    #[test]
    fn validate_storage_dir_requires_absolute_paths() {
        assert!(validate_storage_dir("relative/cache").is_err());
    }

    #[test]
    fn validate_storage_dir_normalizes_safe_parents() {
        let base = std::env::temp_dir();
        let path = base.join("aft-config-test").join("..").join("cache");
        assert_eq!(
            validate_storage_dir(path.to_str().unwrap()).unwrap(),
            base.join("cache")
        );
    }

    #[test]
    fn validate_storage_dir_rejects_relative_with_dotdot() {
        // Relative paths with .. are rejected (not absolute)
        assert!(validate_storage_dir("../../../etc/passwd").is_err());
    }

    // Unix-only: on Windows, `\..\..\cache` isn't an absolute path (no
    // drive letter), so the dotdot-normalization-of-absolute-path
    // semantics this test asserts don't apply.
    #[cfg(unix)]
    #[test]
    fn validate_storage_dir_accepts_absolute_with_dotdot_that_normalizes() {
        // /../../cache normalizes to /cache which is a valid absolute path
        let mut path = PathBuf::from(std::path::MAIN_SEPARATOR.to_string());
        path.push("..");
        path.push("..");
        path.push("cache");
        assert!(validate_storage_dir(path.to_str().unwrap()).is_ok());
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_existing_directory_after_canonicalize() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cache").join("node_modules").join(".bin");
        std::fs::create_dir_all(&dir).unwrap();

        let paths = parse_lsp_paths_extra(&json!([dir])).unwrap();

        assert_eq!(paths, vec![std::fs::canonicalize(&dir).unwrap()]);
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_nonexistent_directory_for_later_install() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("pending").join("node_modules").join(".bin");

        let paths = parse_lsp_paths_extra(&json!([missing])).unwrap();

        assert_eq!(paths, vec![missing]);
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "not a directory").unwrap();

        let error = parse_lsp_paths_extra(&json!([file])).unwrap_err();

        assert!(error.contains("must resolve to a directory"));
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let traversing = tmp.path().join("project").join("..").join("outside");

        let error = parse_lsp_paths_extra(&json!([traversing])).unwrap_err();

        assert!(error.contains("must not contain '..' traversal"));
    }

    #[test]
    fn parse_lsp_paths_extra_accepts_symlink_to_directory_as_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target-dir");
        let link = tmp.path().join("linked-dir");
        std::fs::create_dir_all(&target).unwrap();
        create_dir_symlink(&target, &link);

        let paths = parse_lsp_paths_extra(&json!([link])).unwrap();

        assert_eq!(paths, vec![std::fs::canonicalize(&target).unwrap()]);
    }

    #[test]
    fn parse_lsp_paths_extra_rejects_symlink_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target-file");
        let link = tmp.path().join("linked-file");
        std::fs::write(&target, "not a directory").unwrap();
        create_file_symlink(&target, &link);

        let error = parse_lsp_paths_extra(&json!([link])).unwrap_err();

        assert!(error.contains("must resolve to a directory"));
    }

    #[test]
    fn watcher_attach_runs_off_configure_foreground_when_slow() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let attach_started = Arc::new(Barrier::new(2));
        let attach_started_for_thread = Arc::clone(&attach_started);

        let started = Instant::now();
        let handle = install_project_watcher_with(&ctx, root.path(), move |_root, _tx| {
            attach_started_for_thread.wait();
            std::thread::sleep(Duration::from_millis(250));
            Ok::<(), &'static str>(())
        });

        assert!(
            started.elapsed() < Duration::from_millis(100),
            "watcher installation should not wait for slow attach"
        );
        assert!(ctx.watcher_rx().borrow().is_some());
        assert!(ctx.watcher().borrow().is_none());

        attach_started.wait();
        WATCHER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        handle.join().unwrap();
    }

    #[test]
    fn invalid_late_configure_field_does_not_mutate_existing_context() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let first_req = configure_request_with_params(json!({
            "project_root": first.path(),
            "harness": "opencode",
            "max_callgraph_files": 1000
        }));
        let first_response = super::handle_configure(&first_req, &ctx);
        assert!(first_response.success);
        let canonical_before = ctx.canonical_cache_root();

        let invalid_req = configure_request_with_params(json!({
            "project_root": second.path(),
            "harness": "pi",
            "formatter_timeout_secs": 0
        }));
        let invalid_response = super::handle_configure(&invalid_req, &ctx);

        assert!(!invalid_response.success);
        assert_eq!(invalid_response.data["code"], "invalid_request");
        assert_eq!(ctx.harness_opt(), Some(crate::harness::Harness::Opencode));
        assert_eq!(ctx.canonical_cache_root(), canonical_before);
        let config = ctx.config();
        assert_eq!(config.project_root.as_deref(), Some(first.path()));
        assert_eq!(config.harness, Some(crate::harness::Harness::Opencode));
        assert_eq!(config.max_callgraph_files, 1000);
    }

    #[test]
    fn configure_replaces_formatter_and_checker_maps_when_present() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let first_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "formatter": { "typescript": "biome", "python": "ruff" },
            "checker": { "typescript": "tsc" }
        }));
        assert!(super::handle_configure(&first_req, &ctx).success);

        let second_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "formatter": { "rust": "rustfmt" },
            "checker": { "go": "go" }
        }));
        assert!(super::handle_configure(&second_req, &ctx).success);

        let config = ctx.config();
        assert_eq!(
            config.formatter.get("rust").map(String::as_str),
            Some("rustfmt")
        );
        assert!(!config.formatter.contains_key("typescript"));
        assert!(!config.formatter.contains_key("python"));
        assert_eq!(config.checker.get("go").map(String::as_str), Some("go"));
        assert!(!config.checker.contains_key("typescript"));
    }

    #[test]
    fn configure_rejects_validate_on_edit_string_booleans_without_mutation() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "validate_on_edit": "true"
        }));

        let response = super::handle_configure(&req, &ctx);

        assert!(!response.success);
        assert_eq!(response.data["code"], "invalid_request");
        assert!(ctx.config().project_root.is_none());
        assert!(ctx.harness_opt().is_none());
    }

    #[test]
    fn configure_generation_advances_only_after_successful_configure() {
        let root = tempfile::tempdir().unwrap();
        let ctx = test_context();
        let invalid_req = configure_request_with_params(json!({
            "project_root": root.path(),
            "harness": "opencode",
            "max_callgraph_files": 0
        }));
        assert!(!super::handle_configure(&invalid_req, &ctx).success);
        assert_eq!(ctx.configure_generation(), 0);

        let valid_req = configure_request(json!(root.path()));
        assert!(super::handle_configure(&valid_req, &ctx).success);
        assert_eq!(ctx.configure_generation(), 1);
    }

    #[test]
    fn semantic_max_files_defaults_to_20k() {
        assert_eq!(SemanticBackendConfig::default().max_files, 20_000);
    }

    #[test]
    fn parse_semantic_config_reads_max_files() {
        let cfg = parse_semantic_config(
            &json!({ "max_files": 50_000 }),
            &SemanticBackendConfig::default(),
        )
        .expect("valid max_files");
        assert_eq!(cfg.max_files, 50_000);
    }

    #[test]
    fn parse_semantic_config_max_files_omitted_keeps_existing() {
        let existing = SemanticBackendConfig {
            max_files: 7_500,
            ..SemanticBackendConfig::default()
        };
        let cfg = parse_semantic_config(&json!({ "model": "x" }), &existing).expect("valid config");
        assert_eq!(cfg.max_files, 7_500);
    }

    #[test]
    fn parse_semantic_config_rejects_non_integer_max_files() {
        let base = SemanticBackendConfig::default();
        assert!(parse_semantic_config(&json!({ "max_files": "lots" }), &base).is_err());
        assert!(parse_semantic_config(&json!({ "max_files": 1.5 }), &base).is_err());
    }
}
