use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::context::AppContext;
use crate::grep_executor::{
    bounded_fallback_walk_files_for_target, diagnose_scope_counts, ScopeFileCounts,
};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, resolve_search_scope, sort_paths_by_cached_mtime_desc,
};

use super::multi_path::{canonical_key, resolve_path_or_multi, SearchPathResolution};

#[derive(Debug)]
struct GlobDiscovery {
    files: Vec<PathBuf>,
    indexed_mtimes: HashMap<PathBuf, SystemTime>,
    walk_truncated: bool,
    walk_time_limit_reached: bool,
    source: &'static str,
    entries_visited: usize,
    walk_time: Duration,
    scope_has_files: bool,
    scope_probe: Duration,
    skipped_foreign_mounts: usize,
}

pub const DEFAULT_MAX_RESULTS: usize = 100;
const GLOB_TRUNCATED_MESSAGE: &str =
    "(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)";
const MAX_FLAT_FILES: usize = 20;
const MAX_FILES_PER_DIRECTORY: usize = 7;
const MAX_DISPLAY_FILES_PER_DIRECTORY: usize = 5;
const MAX_DIRECTORY_SECTIONS: usize = 8;
const MAX_DISPLAY_DIRECTORIES: usize = 6;

pub fn handle_glob(req: &RawRequest, ctx: &AppContext) -> Response {
    crate::context::confinement::with_floor_disclosure(|| {
        crate::bounded_io::command(req, ctx, handle_glob_inner)
    })
}

fn handle_glob_inner(req: &RawRequest, ctx: &AppContext) -> Response {
    let pattern = match req.params.get("pattern").and_then(|value| value.as_str()) {
        Some(pattern) => pattern,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "glob: missing required param 'pattern'",
            );
        }
    };

    let filters = match build_path_filters(&[pattern.to_string()], &[]) {
        Ok(filters) => filters,
        Err(error) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("glob: invalid pattern: {}", error),
            )
        }
    };

    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let project_root = crate::bounded_io::canonicalize(&project_root).unwrap_or(project_root);
    crate::context::confinement::set_floor_filter(&project_root, &filters);
    let search_roots = match req.params.get("path").and_then(|value| value.as_str()) {
        Some(path) => match resolve_path_or_multi(
            path,
            &project_root,
            |candidate| {
                ctx.validate_search_path(
                    &req.id,
                    req.session(),
                    candidate,
                    crate::context::WorkerSearchTool::Glob,
                )
            },
            &req.id,
        ) {
            Ok(SearchPathResolution::Single(root)) => vec![root],
            Ok(SearchPathResolution::Multi(roots)) => roots,
            Err(resp) => return resp,
        },
        None => vec![resolve_search_scope(&project_root, None).root],
    };
    // In a confined call, refuse a search root outside the confined root or
    // denied by the read policy before any walk starts there.
    for root in &search_roots {
        if let Err(refusal) = crate::context::confinement::check_walk_root(&req.id, root) {
            return refusal;
        }
    }

    // Return clear error if the search path doesn't exist
    if let Some(missing_root) = search_roots
        .iter()
        .find(|root| crate::bounded_io::metadata(root).is_err())
    {
        return Response::error(
            &req.id,
            "path_not_found",
            format!(
                "glob: search path does not exist: {}",
                missing_root.display()
            ),
        );
    }
    let total_started = Instant::now();
    let mut parent_gaps = Vec::new();
    let mut indexed_mtimes = HashMap::new();
    let ignored_targets = search_roots
        .iter()
        .map(|root| {
            !crate::context::confinement::active()
                && req
                    .params
                    .get("path")
                    .and_then(|value| value.as_str())
                    .is_some()
                && !crate::grep_executor::is_same_directory(root, &project_root)
                && crate::grep_executor::target_is_ignored(root)
        })
        .collect::<Vec<_>>();
    let mut named_ignored_roots = Vec::new();
    for root in &search_roots {
        match named_ignored_root(req, ctx, root, pattern) {
            Ok(named) => named_ignored_roots.push(named),
            Err(refusal) => return refusal,
        }
    }
    let has_named_ignored_root = named_ignored_roots.iter().any(Option::is_some);
    let parent_answer = if crate::context::confinement::active()
        || ignored_targets.iter().any(|ignored| *ignored)
        || has_named_ignored_root
    {
        None
    } else {
        crate::views::parent::glob_fan_out(ctx, &search_roots, pattern)
    };
    let (
        mut files,
        walk_truncated,
        walk_time_limit_reached,
        source,
        entries_visited,
        walk_time,
        scope_has_files,
        scope_probe,
        skipped_foreign_mounts,
    ) = if let Some(answer) = parent_answer {
        // A parent folder session answers from its child repositories' indexes.
        parent_gaps = answer.gaps;
        (
            answer.files,
            false,
            false,
            "index",
            answer.entries_visited,
            Duration::ZERO,
            answer.scope_has_files,
            Duration::ZERO,
            0,
        )
    } else if search_roots.len() == 1 {
        let discovery = discover_glob_root(
            named_ignored_roots[0].as_deref(),
            ctx,
            &project_root,
            &search_roots[0],
            pattern,
            DEFAULT_MAX_RESULTS + 1,
            ignored_targets[0],
            req.params
                .get("path")
                .and_then(|value| value.as_str())
                .map(|_| req.session()),
        );
        indexed_mtimes = discovery.indexed_mtimes;
        (
            discovery.files,
            discovery.walk_truncated,
            discovery.walk_time_limit_reached,
            discovery.source,
            discovery.entries_visited,
            discovery.walk_time,
            discovery.scope_has_files,
            discovery.scope_probe,
            discovery.skipped_foreign_mounts,
        )
    } else {
        let discoveries: Vec<GlobDiscovery> = search_roots
            .iter()
            .zip(&ignored_targets)
            .zip(&named_ignored_roots)
            .map(|((root, ignored), named)| {
                discover_glob_root(
                    named.as_deref(),
                    ctx,
                    &project_root,
                    root,
                    pattern,
                    DEFAULT_MAX_RESULTS + 1,
                    *ignored,
                    req.params
                        .get("path")
                        .and_then(|value| value.as_str())
                        .map(|_| req.session()),
                )
            })
            .collect();
        let walk_truncated = discoveries.iter().any(|d| d.walk_truncated);
        let walk_time_limit_reached = discoveries.iter().any(|d| d.walk_time_limit_reached);
        let entries_visited = discoveries.iter().map(|d| d.entries_visited).sum();
        let walk_time = discoveries.iter().map(|d| d.walk_time).sum();
        let scope_has_files = discoveries.iter().any(|d| d.scope_has_files);
        let scope_probe = discoveries.iter().map(|d| d.scope_probe).sum();
        let skipped_foreign_mounts = discoveries.iter().map(|d| d.skipped_foreign_mounts).sum();
        let source = if discoveries.iter().all(|d| d.source == "index") {
            "index"
        } else {
            "mixed/fallback"
        };
        for discovery in &discoveries {
            indexed_mtimes.extend(
                discovery
                    .indexed_mtimes
                    .iter()
                    .map(|(path, time)| (path.clone(), *time)),
            );
        }
        let files = merge_glob_files(discoveries.into_iter().flat_map(|d| d.files).collect());
        (
            files,
            walk_truncated,
            walk_time_limit_reached,
            source,
            entries_visited,
            walk_time,
            scope_has_files,
            scope_probe,
            skipped_foreign_mounts,
        )
    };
    let mut admitted_files = Vec::new();
    for path in files {
        match ctx.validate_read_path(&req.id, req.session(), &path) {
            Ok(_) => admitted_files.push(path),
            Err(response) if response.data["code"] == "denied_floor" => {
                crate::context::confinement::capture().record_floor(&path);
            }
            Err(response) => return response,
        }
    }
    files = admitted_files;
    crate::slog_debug!(
        "perf glob phases: source={} walk={:.3}ms entries_visited={} scope_probe={:.3}ms discovery_total={:.3}ms",
        source,
        walk_time.as_secs_f64() * 1000.0,
        entries_visited,
        scope_probe.as_secs_f64() * 1000.0,
        total_started.elapsed().as_secs_f64() * 1000.0,
    );
    // Glob's public contract is newest-first. A timed-out walk keeps discovery
    // order because metadata reads to sort its partial matches could outlast the
    // I/O deadline and discard the results entirely.
    if !walk_time_limit_reached {
        sort_paths_by_cached_mtime_desc(&mut files, &project_root, &indexed_mtimes);
    }
    // Backstop against a stale index (for example a borrowed snapshot that
    // still lists another checkout's files): only paths present on disk in this
    // checkout are returned. Only the page is checked, and the page is refilled
    // from the following matches, so present files keep their order.
    let presence = crate::search_index::take_present_on_disk(
        files,
        DEFAULT_MAX_RESULTS,
        |path| path.as_path(),
        "glob",
    );
    let missing_on_disk = presence.dropped;
    let presence_budget_exhausted = presence.budget_exhausted;
    let files = presence.present;
    let total = files.len() + presence.unexamined;
    let result_truncated = total > DEFAULT_MAX_RESULTS || presence_budget_exhausted;

    let mut body = serde_json::json!({
        "text": format_glob_text(&files, pattern, &project_root, result_truncated),
        "complete": !walk_truncated && skipped_foreign_mounts == 0 && !presence_budget_exhausted,
        "no_files_matched_scope": !scope_has_files,
        "skipped_foreign_mounts": skipped_foreign_mounts,
        "files": files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "total": total,
        "truncated": result_truncated,
    });
    if walk_truncated {
        body["walk_truncated"] = serde_json::Value::Bool(true);
    }
    if has_named_ignored_root {
        body["walk_entries_examined"] = serde_json::json!(entries_visited);
    }
    if walk_time_limit_reached {
        body["walk_entries_examined"] = serde_json::json!(entries_visited);
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(Fallback directory walk reached its time limit after examining {entries_visited} entries; results may be incomplete and are in discovery order because they could not be sorted before the I/O deadline. Narrow with a smaller path or a more specific pattern.)",
            body["text"].as_str().unwrap_or_default(),
        ));
    }
    if !scope_has_files && has_named_ignored_root {
        // The prefix walk already examined this scope. Probing the original
        // root would count the overridden ignore rule instead of those entries.
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(0 files searched: no files matched the pattern in the named folder scope; nothing was searched.)",
            body["text"].as_str().unwrap_or_default(),
        ));
    } else if !scope_has_files {
        let counts = {
            let filters = build_path_filters(&[pattern.to_string()], &[]).unwrap_or_default();
            search_roots.iter().zip(&ignored_targets).try_fold(
                ScopeFileCounts::default(),
                |mut counts, (root, ignored)| {
                    counts.add(diagnose_scope_counts(root, root, &filters, *ignored)?);
                    Some(counts)
                },
            )
        };
        // Say in the text itself that nothing was searched, so an empty scope
        // never reads as a searched directory with zero files.
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n{}",
            body["text"].as_str().unwrap_or_default(),
            counts.map(|counts| counts.empty_glob_note()).unwrap_or_else(|| {
                "(0 files searched: the bounded walk could not determine why no files matched the scope; nothing was searched.)".to_string()
            })
        ));
        if let Some(counts) = counts {
            body["files_examined"] = serde_json::json!(counts.examined);
            body["ignored_items"] = serde_json::json!(counts.ignored);
            body["files_excluded_by_patterns"] = serde_json::json!(counts.filtered);
        } else {
            body["complete"] = serde_json::json!(false);
        }
    }
    if missing_on_disk > 0 {
        body["missing_on_disk_dropped"] = serde_json::json!(missing_on_disk);
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n{}",
            body["text"].as_str().unwrap_or_default(),
            missing_on_disk_note(missing_on_disk, presence_budget_exhausted)
        ));
    }
    // Disclose the trigram index state and whether any root was answered by a
    // filesystem walk rather than the index.
    body["index"] = if crate::context::confinement::active() {
        serde_json::json!({"trigram": {"status":"fallback", "reason":"confined root walk"}})
    } else {
        crate::commands::grep::trigram_index_disclosure(ctx)
    };
    body["fallback"] = if source == "index" {
        serde_json::Value::Null
    } else {
        serde_json::Value::String("filesystem".to_string())
    };
    crate::views::parent::attach_gaps(&mut body, parent_gaps);
    crate::context::confinement::disclose_denied(&mut body);

    let rendered_count =
        crate::subc_format::report_rendered_row_count("glob", &body).unwrap_or(files.len());

    let envelope = crate::list_surfaces::glob::build_glob_envelope_with_time_limit(
        rendered_count,
        total,
        result_truncated,
        walk_truncated,
        walk_time_limit_reached,
        skipped_foreign_mounts,
    );

    if let Some(env) = envelope {
        if walk_time_limit_reached {
            body["text"] =
                serde_json::Value::String(crate::subc_format::append_glob_envelope_trailer(
                    body["text"].as_str().unwrap_or_default(),
                    &env,
                ));
        }
        body["files_list_envelope"] = serde_json::to_value(&env).unwrap_or_default();
    } else {
        if walk_truncated {
            let note = "(Fallback directory walk stopped early: file-count or time budget reached; results may be incomplete.)";
            body["text"] = serde_json::Value::String(format!(
                "{}\n\n{}",
                body["text"].as_str().unwrap_or_default(),
                note
            ));
        }
        if skipped_foreign_mounts > 0 {
            body["text"] = serde_json::Value::String(format!(
                "{}\n\n(Fallback directory walk skipped {skipped_foreign_mounts} foreign filesystem mount(s); results may be incomplete.)",
                body["text"].as_str().unwrap_or_default(),
            ));
        }
    }

    Response::success(&req.id, body)
}

fn named_ignored_root(
    req: &RawRequest,
    ctx: &AppContext,
    search_root: &Path,
    pattern: &str,
) -> Result<Option<PathBuf>, Response> {
    let Some((folders, _)) = pattern.rsplit_once('/') else {
        return Ok(None);
    };
    let mut prefix = PathBuf::new();
    // The final component is a filename pattern, even when it is literal.
    for segment in folders.split('/') {
        if segment.contains(['*', '?', '[', ']', '{', '}']) {
            break;
        }
        if segment.is_empty() {
            return Ok(None);
        }
        prefix.push(segment);
    }
    if prefix.as_os_str().is_empty()
        || prefix.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            ) || crate::grep_executor::is_fallback_skipped_directory_name(
                &component.as_os_str().to_string_lossy(),
            )
        })
    {
        return Ok(None);
    }
    // A pattern narrows its existing search root; unlike `path`, it must not
    // introduce a new external scope, including through a directory symlink.
    let Ok(candidate) = crate::bounded_io::canonicalize(&search_root.join(&prefix)) else {
        return Ok(None);
    };
    if !candidate.starts_with(search_root)
        || candidate.strip_prefix(search_root).is_ok_and(|relative| {
            relative.components().any(|component| {
                crate::grep_executor::is_fallback_skipped_directory_name(
                    &component.as_os_str().to_string_lossy(),
                )
            })
        })
        || !crate::bounded_io::metadata(&candidate).is_ok_and(|metadata| metadata.is_dir())
    {
        return Ok(None);
    }
    let ignored = match crate::context::confinement::ConfinedIgnoreRules::current(
        crate::context::confinement::SEARCH_IGNORE_FILES,
    ) {
        Some(rules) => candidate
            .ancestors()
            .take_while(|path| path.starts_with(search_root))
            .any(|path| rules.is_ignored(path, true)),
        None => crate::grep_executor::target_is_ignored(&candidate),
    };
    if !ignored {
        return Ok(None);
    }
    ctx.validate_read_path(&req.id, req.session(), &candidate)?;
    crate::context::confinement::check_walk_root(&req.id, &candidate)?;
    Ok(Some(candidate))
}

fn discover_glob_root(
    named_ignored_root: Option<&Path>,
    ctx: &AppContext,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    max_results: usize,
    ignored_target: bool,
    explicit_session: Option<&str>,
) -> GlobDiscovery {
    if let Some(named_root) = named_ignored_root {
        let filters = build_path_filters(&[pattern.to_owned()], &[]).unwrap_or_default();
        let started = Instant::now();
        let outcome = crate::grep_executor::bounded_glob_walk_in_named_ignored_folder(
            search_root,
            named_root,
            &filters,
        );
        return GlobDiscovery {
            scope_has_files: !outcome.files.is_empty()
                || outcome.walk_truncated
                || outcome.skipped_foreign_mounts > 0,
            files: outcome.files,
            indexed_mtimes: HashMap::new(),
            walk_truncated: outcome.walk_truncated,
            walk_time_limit_reached: outcome.walk_time_limit_reached,
            source: "fallback",
            entries_visited: outcome.entries_visited,
            walk_time: started.elapsed(),
            scope_probe: Duration::ZERO,
            skipped_foreign_mounts: outcome.skipped_foreign_mounts,
        };
    }
    glob_root(
        ctx,
        project_root,
        search_root,
        pattern,
        max_results,
        ignored_target,
        explicit_session,
    )
}

/// Text disclosure for index entries dropped because they are not on disk.
fn missing_on_disk_note(dropped: usize, budget_exhausted: bool) -> String {
    let mut note = format!(
        "({dropped} indexed path(s) no longer on disk in this checkout were left out; the index is out of date."
    );
    if budget_exhausted {
        note.push_str(" The check stopped early, so more results may exist.");
    }
    note.push(')');
    note
}

fn glob_root(
    ctx: &AppContext,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    max_results: usize,
    ignored_target: bool,
    explicit_session: Option<&str>,
) -> GlobDiscovery {
    if crate::context::confinement::active() {
        return fallback_glob(project_root, search_root, pattern, ignored_target);
    }
    if let Some(outputs) = explicit_session.and_then(|session| {
        ctx.bash_background()
            .explicit_task_artifacts(session, search_root)
    }) {
        let filters = build_path_filters(&[pattern.to_owned()], &[]).unwrap_or_default();
        let filter_root = if search_root.is_file() {
            search_root.parent().unwrap_or(search_root)
        } else {
            search_root
        };
        let entries_visited = outputs.len();
        let files: Vec<_> = outputs
            .into_iter()
            .filter(|path| filters.matches(filter_root, path))
            .collect();
        return GlobDiscovery {
            scope_has_files: !files.is_empty(),
            files,
            indexed_mtimes: HashMap::new(),
            walk_truncated: false,
            walk_time_limit_reached: false,
            source: "task-output",
            entries_visited,
            walk_time: Duration::ZERO,
            scope_probe: Duration::ZERO,
            skipped_foreign_mounts: 0,
        };
    }
    let search_root_text = search_root.to_string_lossy();
    let search_scope = resolve_search_scope(project_root, Some(search_root_text.as_ref()));
    let indexed_snapshot = {
        let search_index = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match search_index.as_ref() {
            Some(index) if index.ready && search_scope.use_index && !ignored_target => {
                Some(index.snapshot())
            }
            _ => None,
        }
    };
    let index_covers_root = indexed_snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.has_file_in_scope(&search_scope.root));
    let indexed = indexed_snapshot.map(|snapshot| {
        let (files, scope_has_files, entries_visited) =
            snapshot.glob_profiled(pattern, &search_scope.root, false);
        let indexed_mtimes = files
            .iter()
            .filter_map(|path| {
                snapshot
                    .file_modified(path)
                    .map(|time| (path.clone(), time))
            })
            .collect();
        GlobDiscovery {
            entries_visited,
            files,
            indexed_mtimes,
            walk_truncated: false,
            walk_time_limit_reached: false,
            source: "index",
            walk_time: Duration::ZERO,
            scope_has_files,
            scope_probe: Duration::ZERO,
            skipped_foreign_mounts: 0,
        }
    });

    match indexed {
        Some(discovery)
            if index_covers_root
                || crate::grep_executor::is_same_directory(&search_scope.root, project_root) =>
        {
            discovery
        }
        // The index is ready but holds no file under this explicitly named
        // subdirectory: an ignore rule keeps it out of the index (a gitignored
        // `node_modules/pkg`, for example) or it is a symlinked directory the
        // index walk never follows. Walk it rather than report zero files; the
        // walk applies ignore rules only to entries nested inside the named
        // root. A project-wide glob answers from the index even when empty.
        Some(_) => fallback_glob(project_root, &search_scope.root, pattern, ignored_target),
        None => {
            if search_scope.use_index {
                super::configure::trigger_search_index_reload_if_evicted(ctx);
            }
            if !search_scope.use_index && !ignored_target {
                let walk_started = Instant::now();
                if let Some(outcome) =
                    super::grep::ripgrep_glob(&search_scope.root, pattern, max_results)
                {
                    let walk_time = walk_started.elapsed();
                    let scope_started = Instant::now();
                    let scope_has_files = !outcome.files.is_empty() || outcome.walk_truncated;
                    return GlobDiscovery {
                        files: outcome.files,
                        indexed_mtimes: HashMap::new(),
                        walk_truncated: outcome.walk_truncated,
                        walk_time_limit_reached: outcome.walk_time_limit_reached,
                        source: "fallback",
                        entries_visited: outcome.entries_visited,
                        walk_time,
                        scope_has_files,
                        scope_probe: scope_started.elapsed(),
                        skipped_foreign_mounts: 0,
                    };
                }
            }
            fallback_glob(project_root, &search_scope.root, pattern, ignored_target)
        }
    }
}

/// Runs one `glob_root` discovery and returns the files it found, the source
/// that answered, and how many walk entries it visited, so tests outside this
/// module can check which branch a call took and whether it walked at all.
#[cfg(all(test, unix))]
pub(crate) fn glob_root_files_for_test(
    ctx: &AppContext,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    max_results: usize,
    explicit_session: Option<&str>,
) -> (Vec<PathBuf>, &'static str, usize) {
    let discovery = glob_root(
        ctx,
        project_root,
        search_root,
        pattern,
        max_results,
        false,
        explicit_session,
    );
    (discovery.files, discovery.source, discovery.entries_visited)
}

fn merge_glob_files(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for file in files {
        if seen.insert(canonical_key(&file)) {
            deduped.push(file);
        }
    }
    deduped
}

fn fallback_glob(
    _project_root: &std::path::Path,
    search_root: &std::path::Path,
    pattern: &str,
    ignored_target: bool,
) -> GlobDiscovery {
    let filters = build_path_filters(&[pattern.to_string()], &[]).unwrap_or_default();
    // Match the glob pattern relative to the search root so that a bare
    // filename like `a.rs` matches a file directly under `path` (e.g.
    // `src/a.rs` when `path` is `src`). Using `project_root` as the filter
    // root would make the relative path `src/a.rs`, which `a.rs` does not
    // match. The `ripgrep_glob` path already passes `search_root` as both
    // arguments, so this keeps the two fallback routes in agreement.
    let walk_started = Instant::now();
    let outcome =
        bounded_fallback_walk_files_for_target(search_root, search_root, &filters, ignored_target);
    let walk_time = walk_started.elapsed();
    let scope_started = Instant::now();
    // Reuse the filtered walk; an incomplete walk cannot prove an empty scope.
    let scope_has_files =
        !outcome.files.is_empty() || outcome.walk_truncated || outcome.skipped_foreign_mounts > 0;
    GlobDiscovery {
        files: outcome.files,
        indexed_mtimes: HashMap::new(),
        walk_truncated: outcome.walk_truncated,
        walk_time_limit_reached: outcome.walk_time_limit_reached,
        source: "fallback",
        entries_visited: outcome.entries_visited,
        walk_time,
        scope_has_files,
        scope_probe: scope_started.elapsed(),
        skipped_foreign_mounts: outcome.skipped_foreign_mounts,
    }
}

fn format_glob_text(
    files: &[PathBuf],
    pattern: &str,
    project_root: &Path,
    truncated: bool,
) -> String {
    // Convert to relative paths within project
    let relative_files: Vec<PathBuf> = files
        .iter()
        .map(|p| p.strip_prefix(project_root).unwrap_or(p).to_path_buf())
        .collect();

    let header = format!(
        "{} {} matching {}",
        relative_files.len(),
        if relative_files.len() == 1 {
            "file"
        } else {
            "files"
        },
        pattern
    );

    let text = if relative_files.is_empty() {
        header
    } else if relative_files.len() <= MAX_FLAT_FILES {
        let body = relative_files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        format!("{}\n\n{}", header, body)
    } else {
        let grouped = group_files_by_directory(&relative_files);
        let total_directories = grouped.len();
        let displayed_directories = if total_directories > MAX_DIRECTORY_SECTIONS {
            MAX_DISPLAY_DIRECTORIES
        } else {
            total_directories
        };

        let mut sections = Vec::new();
        for (directory, names) in grouped.iter().take(displayed_directories) {
            let file_word = if names.len() == 1 { "file" } else { "files" };
            let names_text = if names.len() > MAX_FILES_PER_DIRECTORY {
                format!(
                    "{}, ...",
                    names
                        .iter()
                        .take(MAX_DISPLAY_FILES_PER_DIRECTORY)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                names.join(", ")
            };
            sections.push(format!(
                "{} ({} {})\n  {}",
                directory,
                names.len(),
                file_word,
                names_text
            ));
        }

        let mut body = format!("{}\n\n{}", header, sections.join("\n\n"));

        if total_directories > MAX_DIRECTORY_SECTIONS {
            let hidden_directories = &grouped[displayed_directories..];
            let hidden_file_count: usize = hidden_directories
                .iter()
                .map(|(_, names)| names.len())
                .sum();
            let hidden_directory_count = total_directories - displayed_directories;
            body.push_str(&format!(
                "\n\n... and {} more {} in {} {}",
                hidden_file_count,
                if hidden_file_count == 1 {
                    "file"
                } else {
                    "files"
                },
                hidden_directory_count,
                if hidden_directory_count == 1 {
                    "directory"
                } else {
                    "directories"
                }
            ));
        }

        body
    };

    if truncated {
        format!("{}\n\n{}", text, GLOB_TRUNCATED_MESSAGE)
    } else {
        text
    }
}

fn group_files_by_directory(files: &[PathBuf]) -> Vec<(String, Vec<String>)> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for file in files {
        let directory = format_directory_label(file.parent());
        let file_name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.display().to_string());
        grouped.entry(directory).or_default().push(file_name);
    }

    grouped.into_iter().collect()
}

fn format_directory_label(directory: Option<&Path>) -> String {
    match directory {
        Some(path) if !path.as_os_str().is_empty() && path != Path::new(".") => {
            format!("{}/", path.display())
        }
        _ => "./".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_file_glob_fixture() -> (tempfile::TempDir, AppContext, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let file = project.join("src/main.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "export const main = 1;\n").unwrap();
        std::fs::write(project.join("src/other.ts"), "export const other = 2;\n").unwrap();
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project.clone()),
                ..Default::default()
            },
        );
        (dir, ctx, project, file)
    }

    #[test]
    fn exact_file_glob_with_ready_index_matches_basename_without_widening_scope() {
        let (_dir, ctx, project, file) = exact_file_glob_fixture();
        let index = crate::search_index::SearchIndex::build(&project);
        assert!(index.is_ready());
        assert!(index.snapshot().has_file_in_scope(&file));
        *ctx.search_index().write().unwrap() = Some(index);

        let discovery = glob_root(&ctx, &project, &file, "main.ts", 100, false, None);
        assert_eq!(discovery.source, "index");
        assert!(!discovery.walk_truncated);
        assert_eq!(discovery.files, vec![file.clone()]);
        assert_eq!(
            glob_root(&ctx, &project, &file, "*.ts", 100, false, None).files,
            vec![file.clone()]
        );
        assert!(
            glob_root(&ctx, &project, &file, "other.ts", 100, false, None)
                .files
                .is_empty()
        );
    }

    #[test]
    fn exact_file_glob_fallback_matches_basename_without_widening_scope() {
        let (_dir, ctx, project, file) = exact_file_glob_fixture();
        assert!(ctx.search_index().read().unwrap().is_none());

        let discovery = glob_root(&ctx, &project, &file, "main.ts", 100, false, None);
        assert_eq!(discovery.source, "fallback");
        assert!(!discovery.walk_truncated);
        assert_eq!(discovery.files, vec![file.clone()]);
        assert_eq!(
            glob_root(&ctx, &project, &file, "*.ts", 100, false, None).files,
            vec![file.clone()]
        );
        assert!(
            glob_root(&ctx, &project, &file, "other.ts", 100, false, None)
                .files
                .is_empty()
        );
    }

    #[test]
    fn named_ignored_glob_walk_stops_at_cap_and_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(project.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(project.join("ignored")).unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            std::fs::write(project.join("ignored").join(name), "cost").unwrap();
        }
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "glob-named-ignored-bounds", "command": "glob", "pattern": "ignored/*.md"
        }))
        .unwrap();
        let capped = crate::grep_executor::with_walk_limit(1, || handle_glob(&request, &ctx));
        let body = serde_json::to_value(&capped).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["total"], 1, "{body}");
        assert_eq!(body["walk_truncated"], true, "{body}");
        assert_eq!(body["complete"], false, "{body}");
        assert_eq!(body["walk_entries_examined"], 3, "{body}");

        let timed = crate::grep_executor::with_fallback_walk_budget_for_test(
            Duration::from_millis(100),
            || {
                crate::grep_executor::with_fallback_walk_entry_delay_for_test(
                    Duration::from_millis(250),
                    || handle_glob(&request, &ctx),
                )
            },
        );
        let body = serde_json::to_value(&timed).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["total"], 1, "{body}");
        assert_eq!(body["complete"], false, "{body}");
        assert_eq!(body["walk_entries_examined"], 2, "{body}");
        assert_eq!(
            body["files_list_envelope"]["reason"], "time_limit",
            "{body}"
        );
    }

    #[test]
    fn indexed_glob_is_unchanged_by_a_zero_fallback_walk_budget() {
        let (_dir, ctx, project, _) = exact_file_glob_fixture();
        let index = crate::search_index::SearchIndex::build(&project);
        assert!(index.is_ready());
        *ctx.search_index().write().unwrap() = Some(index);
        let request = serde_json::from_value(serde_json::json!({
            "id": "glob-indexed-budget",
            "command": "glob",
            "pattern": "*.ts",
        }))
        .unwrap();

        let response =
            crate::grep_executor::with_fallback_walk_budget_for_test(Duration::ZERO, || {
                handle_glob(&request, &ctx)
            });
        let body = serde_json::to_value(response).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["complete"], true, "{body}");
        assert_eq!(body["fallback"], serde_json::Value::Null, "{body}");
        assert_eq!(body["files"].as_array().unwrap().len(), 2, "{body}");
        assert!(
            !body["text"].as_str().unwrap().contains("time limit"),
            "{body}"
        );
        assert!(body["walk_entries_examined"].is_null(), "{body}");
    }

    #[test]
    fn fallback_glob_returns_matches_when_its_walk_budget_expires() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(project.join("a.rs"), "a").unwrap();
        std::fs::write(project.join("b.rs"), "b").unwrap();
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project.clone()),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "glob-time-limit",
            "command": "glob",
            "pattern": "*.rs",
        }))
        .unwrap();

        let response = crate::grep_executor::with_fallback_walk_budget_for_test(
            Duration::from_millis(100),
            || {
                crate::grep_executor::with_fallback_walk_entry_delay_for_test(
                    Duration::from_millis(250),
                    || handle_glob(&request, &ctx),
                )
            },
        );
        let body = serde_json::to_value(&response).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["complete"], false, "{body}");
        assert_eq!(body["files"].as_array().unwrap().len(), 1, "{body}");
        assert!(
            body["walk_entries_examined"].as_u64().unwrap() > 0,
            "{body}"
        );
        assert_eq!(body["files_list_envelope"]["reason"], "time_limit");
        let trailer = "shown 1 of ≥1 files (time limit) · narrow: path";
        assert!(
            body["text"].as_str().unwrap().ends_with(trailer),
            "{}",
            body["text"]
        );
        let examined = body["walk_entries_examined"].as_u64().unwrap();
        assert!(examined > 0, "{body}");
        assert!(
            body["text"]
                .as_str()
                .unwrap()
                .contains(&format!("after examining {examined} entries")),
            "{}",
            body["text"]
        );
        let rendered = crate::subc_format::format_response("glob", &response, false);
        assert!(rendered.ends_with(trailer), "{rendered}");
        assert_eq!(rendered.matches(trailer).count(), 1, "{rendered}");
    }

    #[test]
    fn outside_project_glob_reports_when_its_walk_budget_expires() {
        let project_dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(project_dir.path()).unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = std::fs::canonicalize(outside_dir.path()).unwrap();
        std::fs::write(outside.join("a.rs"), "a").unwrap();
        std::fs::write(outside.join("b.rs"), "b").unwrap();
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "glob-outside-time-limit",
            "command": "glob",
            "pattern": "*.rs",
            "path": outside.to_string_lossy(),
        }))
        .unwrap();

        let response = crate::grep_executor::with_fallback_walk_budget_for_test(
            Duration::from_millis(100),
            || {
                crate::grep_executor::with_fallback_walk_entry_delay_for_test(
                    Duration::from_millis(250),
                    || handle_glob(&request, &ctx),
                )
            },
        );
        let body = serde_json::to_value(&response).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["complete"], false, "{body}");
        assert_eq!(body["files"].as_array().unwrap().len(), 1, "{body}");
        assert!(
            body["walk_entries_examined"]
                .as_u64()
                .is_some_and(|examined| examined > 0),
            "expected a positive outside-walk entry count: {body}"
        );
        assert_eq!(body["files_list_envelope"]["reason"], "time_limit");
    }

    #[test]
    fn timed_out_glob_keeps_partial_matches_when_post_walk_metadata_is_slow() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(project.join("a.rs"), "a").unwrap();
        std::fs::write(project.join("b.rs"), "b").unwrap();
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project.clone()),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "glob-slow-post-walk-sort",
            "command": "glob",
            "pattern": "*.rs",
        }))
        .unwrap();

        let response = crate::grep_executor::with_fallback_walk_budget_for_test(
            Duration::from_millis(50),
            || {
                crate::grep_executor::with_fallback_walk_entry_delay_for_test(
                    Duration::from_millis(80),
                    || {
                        crate::grep_executor::with_fallback_walk_sort_metadata_delay_for_test(
                            Duration::from_secs(6),
                            || handle_glob(&request, &ctx),
                        )
                    },
                )
            },
        );
        let body = serde_json::to_value(&response).unwrap();
        assert_eq!(body["success"], true, "{body}");
        assert_eq!(body["complete"], false, "{body}");
        assert_eq!(body["files"].as_array().unwrap().len(), 1, "{body}");
        assert!(
            body["walk_entries_examined"]
                .as_u64()
                .is_some_and(|examined| examined > 0),
            "expected a positive walk entry count: {body}"
        );
        assert!(
            body["text"].as_str().unwrap().contains("discovery order"),
            "{body}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fallback_glob_keeps_read_blocked_for_a_stalled_helper() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(project.join("a.rs"), "a").unwrap();
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(1))
            .with_access_budget(Duration::from_millis(300));

        let response = crate::bounded_io::with_budget(Some(budget), || {
            let stalled_helper = crate::bounded_io::stall_running_helper(&project);
            let discovery = fallback_glob(&project, &project, "*.rs", false);
            assert!(discovery.files.is_empty());
            assert!(!discovery.walk_time_limit_reached);
            let response = crate::bounded_io::finish_response(crate::protocol::Response::success(
                "glob-stalled-helper",
                serde_json::json!({}),
            ));
            drop(stalled_helper);
            response
        });

        assert_eq!(response.data["code"], "read_blocked", "{response:?}");
    }

    #[test]
    fn perf_audit_glob_only_touches_returned_page() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        for i in 0..1000 {
            let path = project.join(format!("file_{i:04}.rs"));
            std::fs::write(&path, format!("fn marker_{i}() {{}}\n")).unwrap();
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(1_700_000_000 + i, 0),
            )
            .unwrap();
        }
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project.clone()),
                ..Default::default()
            },
        );
        *ctx.search_index().write().unwrap() =
            Some(crate::search_index::SearchIndex::build(&project));
        let request = serde_json::from_value(serde_json::json!({
            "id": "audit", "command": "glob", "pattern": "*.rs"
        }))
        .unwrap();
        crate::search_index::audit_work_reset();
        let response = handle_glob(&request, &ctx);
        let body = serde_json::to_value(response).unwrap();
        assert_eq!(body["files"].as_array().unwrap().len(), 100);
        assert_eq!(body["total"], 1000);
        let expected = (900..1000)
            .rev()
            .map(|i| project.join(format!("file_{i:04}.rs")))
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_vec(&body["files"]).unwrap(),
            serde_json::to_vec(
                &expected
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
            )
            .unwrap()
        );
        assert_eq!(
            body["text"],
            format_glob_text(&expected, "*.rs", &project, true)
        );
        let work = crate::search_index::audit_work();
        assert_eq!(work.sort_stats, 0, "{work:?}");
        assert_eq!(work.sort_canonicalizes, 0, "{work:?}");
        assert_eq!(work.presence_checks, 100, "{work:?}");
    }

    fn files(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn root() -> PathBuf {
        PathBuf::from("/project")
    }

    #[test]
    fn glob_uses_flat_list_for_small_results() {
        let text = format_glob_text(&files(&["src/a.rs", "src/b.rs"]), "**/*.rs", &root(), false);

        assert_eq!(text, "2 files matching **/*.rs\n\nsrc/a.rs\nsrc/b.rs");
    }

    #[test]
    fn glob_groups_directories_and_summarizes_overflow() {
        let text = format_glob_text(
            &files(&[
                "dir1/a.rs",
                "dir1/b.rs",
                "dir1/c.rs",
                "dir1/d.rs",
                "dir1/e.rs",
                "dir1/f.rs",
                "dir1/g.rs",
                "dir1/h.rs",
                "dir2/a.rs",
                "dir2/b.rs",
                "dir3/a.rs",
                "dir3/b.rs",
                "dir4/a.rs",
                "dir4/b.rs",
                "dir5/a.rs",
                "dir5/b.rs",
                "dir6/a.rs",
                "dir6/b.rs",
                "dir7/a.rs",
                "dir7/b.rs",
                "dir8/a.rs",
                "dir8/b.rs",
                "dir9/a.rs",
            ]),
            "**/*.rs",
            &root(),
            false,
        );

        assert!(text.starts_with("23 files matching **/*.rs\n\n"));
        assert!(text.contains("dir1/ (8 files)\n  a.rs, b.rs, c.rs, d.rs, e.rs, ..."));
        assert!(text.contains("dir6/ (2 files)\n  a.rs, b.rs"));
        assert!(!text.contains("dir7/ (2 files)\n  a.rs, b.rs"));
        assert!(text.ends_with("... and 5 more files in 3 directories"));
    }

    #[test]
    fn glob_appends_truncation_message() {
        let text = format_glob_text(&files(&["src/a.rs"]), "**/*.rs", &root(), true);

        assert_eq!(
            text,
            "1 file matching **/*.rs\n\nsrc/a.rs\n\n(Results are truncated: showing first 100 results. Consider using a more specific path or pattern.)"
        );
    }
}

#[cfg(test)]
mod ordering_probe {
    #[test]
    fn glob_page_cap_keeps_newest_eligible_discovered_files() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..110 {
            let path = dir.path().join(format!("file-{index:03}.txt"));
            std::fs::write(&path, "eligible").unwrap();
            filetime::set_file_mtime(
                &path,
                filetime::FileTime::from_unix_time(1_600_000_000 + index, 0),
            )
            .unwrap();
        }
        let outcome = crate::grep_executor::ripgrep_glob(dir.path(), "*.txt", 100).unwrap();
        let actual: Vec<_> = outcome
            .files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let expected: Vec<_> = (10..110)
            .rev()
            .map(|index| format!("file-{index:03}.txt"))
            .collect();
        assert_eq!(actual, expected);
    }
}
