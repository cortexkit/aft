use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::context::AppContext;
use crate::grep_executor::{
    bounded_fallback_walk_files_for_target, diagnose_scope_counts, ScopeFileCounts,
};
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, normalized_display_sort_key, presence_stat_budget, resolve_search_scope,
    sort_paths_by_cached_mtime_desc, IndexedGlobMatcher, PresentOnDisk,
};

use super::multi_path::{canonical_key, resolve_path_or_multi, SearchPathResolution};

#[derive(Debug)]
struct GlobDiscovery {
    files: Vec<PathBuf>,
    indexed_mtimes: HashMap<PathBuf, SystemTime>,
    unretained_candidates: usize,
    walk_failed: bool,
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
const MAX_DIRECTORY_SECTIONS: usize = MAX_DISPLAY_DIRECTORIES;
const MAX_DISPLAY_DIRECTORIES: usize = 6;

pub fn handle_glob(req: &RawRequest, ctx: &AppContext) -> Response {
    let config = ctx.config();
    let _pin = false.then(|| ctx.pin_config_to(config));
    ctx.with_read_policy_snapshot(&req.id, || {
        crate::bounded_io::with_skip_disclosure(|| {
            crate::context::confinement::with_floor_disclosure(|| {
                crate::bounded_io_command::command(req, ctx, handle_glob_inner)
            })
        })
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
        crate::parent_search::glob_fan_out(ctx, &search_roots, pattern)
    };
    // A parent folder's ready children are answered from their indexes even
    // when another child is walked, so their skipped folders still count.
    let answered_by_parent = parent_answer.is_some();
    let (
        mut files,
        unretained_candidates,
        walk_failed,
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
        indexed_mtimes = answer.indexed_mtimes;
        (
            answer.files,
            0,
            false,
            answer.walk_truncated,
            answer.walk_time_limit_reached,
            if answer.walked { "fallback" } else { "index" },
            answer.entries_visited,
            Duration::ZERO,
            answer.scope_has_files,
            Duration::ZERO,
            answer.skipped_foreign_mounts,
        )
    } else if search_roots.len() == 1 {
        let discovery = match discover_glob_root(
            named_ignored_roots[0].as_deref(),
            ctx,
            &project_root,
            &search_roots[0],
            pattern,
            DEFAULT_MAX_RESULTS + 1,
            Some(presence_stat_budget(DEFAULT_MAX_RESULTS)),
            ignored_targets[0],
            req.params
                .get("path")
                .and_then(|value| value.as_str())
                .map(|_| req.session()),
        ) {
            Ok(discovery) => discovery,
            // A failed view load fails the call, like grep: no fallback
            // walk may answer for a checkout view. The error text is the
            // view's own, naming the cause.
            Err(error) => return crate::context::view_load_error_response(&req.id, error),
        };
        indexed_mtimes = discovery.indexed_mtimes;
        (
            discovery.files,
            discovery.unretained_candidates,
            discovery.walk_failed,
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
        let mut discoveries = Vec::with_capacity(search_roots.len());
        for ((root, ignored), named) in search_roots
            .iter()
            .zip(&ignored_targets)
            .zip(&named_ignored_roots)
        {
            match discover_glob_root(
                named.as_deref(),
                ctx,
                &project_root,
                root,
                pattern,
                DEFAULT_MAX_RESULTS + 1,
                None,
                *ignored,
                req.params
                    .get("path")
                    .and_then(|value| value.as_str())
                    .map(|_| req.session()),
            ) {
                Ok(discovery) => discoveries.push(discovery),
                // A failed view load fails the call, like grep: no fallback
                // walk may answer for a checkout view.
                Err(error) => return crate::context::view_load_error_response(&req.id, error),
            }
        }
        let walk_failed = discoveries.iter().any(|d| d.walk_failed);
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
            0,
            walk_failed,
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
    let candidate_count = files.len().saturating_add(unretained_candidates);
    let (mut presence, _) = match crate::search_index::take_present_on_disk_with_admission(
        files,
        DEFAULT_MAX_RESULTS,
        |path| path.as_path(),
        "glob",
        |path| match ctx.validate_read_path(&req.id, req.session(), path) {
            Ok(_) => Ok(true),
            Err(response) if response.data["code"] == "denied_floor" => {
                crate::context::confinement::capture().record_matched_floor(path);
                Ok(false)
            }
            Err(response) => Err(response),
        },
    ) {
        Ok(presence) => presence,
        Err(response) => return response,
    };
    account_for_unretained_glob_candidates(&mut presence, unretained_candidates);
    let missing_on_disk = presence.dropped;
    let presence_budget_exhausted = presence.budget_exhausted;
    let files = presence.present;
    let admitted_count = files.len();
    // Unchecked candidates may be stale or denied and do not establish a lower bound.
    let total = admitted_count;
    let all_candidates_checked = presence.unexamined == 0;
    let result_truncated = presence.unexamined > 0 || presence_budget_exhausted;

    let (text, rendered_files) =
        format_glob_output(&files, pattern, &project_root, result_truncated);
    let mut body = serde_json::json!({
        "text": text,
        "rendered_files": rendered_files,
        "complete": all_candidates_checked && !walk_failed && !walk_truncated && skipped_foreign_mounts == 0 && !presence_budget_exhausted,
        "no_files_matched_scope": !walk_failed && !scope_has_files,
        "skipped_foreign_mounts": skipped_foreign_mounts,
        "files": files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "total": all_candidates_checked.then_some(total),
        "truncated": result_truncated,
    });
    if !all_candidates_checked {
        body["total_lower_bound"] = serde_json::json!(total);
        body["admitted_count"] = serde_json::json!(admitted_count);
        body["candidate_count"] = serde_json::json!(candidate_count);
        body["unexamined_files"] = serde_json::json!(presence.unexamined);
    }
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
    if !walk_failed && !scope_has_files && has_named_ignored_root {
        // The prefix walk already examined this scope. Probing the original
        // root would count the overridden ignore rule instead of those entries.
        body["text"] = serde_json::Value::String(format!(
            "{}\n\n(0 files searched: no files matched the pattern in the named folder scope; nothing was searched.)",
            body["text"].as_str().unwrap_or_default(),
        ));
    } else if !walk_failed && !scope_has_files {
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
    crate::parent_search::attach_search_gaps(&mut body, parent_gaps);
    crate::context::confinement::disclose_denied(&mut body);

    let rendered_count =
        crate::subc_format::report_rendered_row_count("glob", &body).unwrap_or(files.len());

    let envelope = crate::list_surfaces::glob::build_glob_envelope_with_time_limit(
        rendered_count,
        total,
        result_truncated,
        walk_failed || walk_truncated,
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

    let mut response = Response::success(&req.id, body);
    if source == "index" || answered_by_parent {
        crate::data_layer::surfaces::disclose_index_gaps(ctx, &search_roots, &mut response);
    }
    response
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
    indexed_window: Option<usize>,
    ignored_target: bool,
    explicit_session: Option<&str>,
) -> Result<GlobDiscovery, String> {
    if let Some(named_root) = named_ignored_root {
        let filters = build_path_filters(&[pattern.to_owned()], &[]).unwrap_or_default();
        let started = Instant::now();
        let outcome = crate::grep_executor::bounded_glob_walk_in_named_ignored_folder(
            search_root,
            named_root,
            &filters,
        );
        return Ok(GlobDiscovery {
            scope_has_files: !outcome.files.is_empty()
                || outcome.walk_truncated
                || outcome.skipped_foreign_mounts > 0,
            files: outcome.files,
            indexed_mtimes: HashMap::new(),
            unretained_candidates: 0,
            walk_failed: outcome.error.is_some(),
            walk_truncated: outcome.walk_truncated,
            walk_time_limit_reached: outcome.walk_time_limit_reached,
            source: "fallback",
            entries_visited: outcome.entries_visited,
            walk_time: started.elapsed(),
            scope_probe: Duration::ZERO,
            skipped_foreign_mounts: outcome.skipped_foreign_mounts,
        });
    }
    glob_root(
        ctx,
        project_root,
        search_root,
        pattern,
        max_results,
        indexed_window,
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
    indexed_window: Option<usize>,
    ignored_target: bool,
    explicit_session: Option<&str>,
) -> Result<GlobDiscovery, String> {
    if crate::context::confinement::active() {
        return Ok(fallback_glob(
            project_root,
            search_root,
            pattern,
            ignored_target,
        ));
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
        return Ok(GlobDiscovery {
            scope_has_files: !files.is_empty(),
            files,
            indexed_mtimes: HashMap::new(),
            unretained_candidates: 0,
            walk_failed: false,
            walk_truncated: false,
            walk_time_limit_reached: false,
            source: "task-output",
            entries_visited,
            walk_time: Duration::ZERO,
            scope_probe: Duration::ZERO,
            skipped_foreign_mounts: 0,
        });
    }
    let search_root_text = search_root.to_string_lossy();
    let search_scope = resolve_search_scope(project_root, Some(search_root_text.as_ref()));
    if search_scope.use_index && !ignored_target {
        if let Some(view) = crate::data_layer::text_scope::ready_view(ctx) {
            if let Some(discovery) = glob_from_view(
                view,
                project_root,
                &search_scope.root,
                pattern,
                indexed_window,
            ) {
                return Ok(discovery);
            }
        }
    }
    if !search_scope.use_index && !ignored_target {
        let walk_started = Instant::now();
        if let Some(outcome) = super::grep::ripgrep_glob(&search_scope.root, pattern, max_results) {
            let walk_time = walk_started.elapsed();
            let scope_started = Instant::now();
            let scope_has_files = !outcome.files.is_empty() || outcome.walk_truncated;
            return Ok(GlobDiscovery {
                files: outcome.files,
                indexed_mtimes: HashMap::new(),
                unretained_candidates: 0,
                walk_failed: outcome.error.is_some(),
                walk_truncated: outcome.walk_truncated,
                walk_time_limit_reached: outcome.walk_time_limit_reached,
                source: "fallback",
                entries_visited: outcome.entries_visited,
                walk_time,
                scope_has_files,
                scope_probe: scope_started.elapsed(),
                skipped_foreign_mounts: 0,
            });
        }
    }
    Ok(fallback_glob(
        project_root,
        &search_scope.root,
        pattern,
        ignored_target,
    ))
}

/// [`glob_root`] answered from the data layer's view of the checkout: the
/// view lists the files and their modification times, and today's indexed
/// glob matches them. `None` when the root is outside the view's checkout,
/// so today's path answers instead.
fn glob_from_view(
    view: crate::data_layer::checkout::CheckoutView,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    indexed_window: Option<usize>,
) -> Option<GlobDiscovery> {
    let scope = crate::data_layer::text_scope::ViewScope::of(view, search_root)?;
    if !scope.is_whole_checkout() && !scope.has_files() {
        // The checkout holds no file under this explicitly named directory,
        // so walk it, as today's path does when its index lacks the directory.
        return Some(fallback_glob(project_root, search_root, pattern, false));
    }
    if let Some(window) = indexed_window {
        return Some(bounded_indexed_glob(
            &scope,
            project_root,
            search_root,
            pattern,
            window,
        ));
    }
    let snapshot = scope.listing();
    let (files, scope_has_files, entries_visited) =
        snapshot.glob_profiled(pattern, search_root, false);
    let indexed_mtimes = files
        .iter()
        .filter_map(|path| {
            snapshot
                .file_modified(path)
                .map(|time| (path.clone(), time))
        })
        .collect();
    Some(GlobDiscovery {
        entries_visited,
        files,
        indexed_mtimes,
        unretained_candidates: 0,
        walk_failed: false,
        walk_truncated: false,
        walk_time_limit_reached: false,
        source: "index",
        walk_time: Duration::ZERO,
        scope_has_files,
        scope_probe: Duration::ZERO,
        skipped_foreign_mounts: 0,
    })
}

/// Keep only the newest rows the presence check could examine. Use this only
/// for a single root: if any filesystem root times out in a multi-root glob,
/// the whole reply keeps discovery order instead of sorting.
fn bounded_indexed_glob(
    scope: &crate::data_layer::text_scope::ViewScope,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    window: usize,
) -> GlobDiscovery {
    let matcher = IndexedGlobMatcher::new(pattern, search_root);
    let stable_root = crate::path_identity::canonicalize_normalized(project_root);
    let mut newest = BinaryHeap::new();
    let mut entries_visited = 0;
    let mut matched = 0usize;
    if let Some(matcher) = matcher {
        for (path, modified) in scope.listing_rows() {
            crate::run_tool_call::progress::report();
            entries_visited += 1;
            if !matcher.matches(&path) {
                continue;
            }
            matched += 1;
            // The public order is newest first, then normalized display bytes,
            // then the original path. The heap puts the worst kept row on top.
            let display_key = normalized_display_sort_key(
                Some(&stable_root),
                &crate::path_identity::normalize_path(&path),
            );
            let row = (Reverse(modified), display_key, path);
            if newest.len() < window {
                newest.push(row);
            } else if newest.peek().is_some_and(|worst| &row < worst) {
                *newest.peek_mut().unwrap() = row;
            }
        }
    }
    let unretained_candidates = matched - newest.len();
    let mut files = Vec::with_capacity(newest.len());
    let mut indexed_mtimes = HashMap::with_capacity(newest.len());
    for (Reverse(modified), _, path) in newest.into_sorted_vec() {
        indexed_mtimes.insert(path.clone(), modified);
        files.push(path);
    }
    GlobDiscovery {
        files,
        indexed_mtimes,
        unretained_candidates,
        entries_visited,
        walk_failed: false,
        walk_truncated: false,
        walk_time_limit_reached: false,
        source: "index",
        walk_time: Duration::ZERO,
        scope_has_files: matched > 0,
        scope_probe: Duration::ZERO,
        skipped_foreign_mounts: 0,
    }
}

fn account_for_unretained_glob_candidates(
    presence: &mut PresentOnDisk<PathBuf>,
    unretained: usize,
) {
    presence.unexamined = presence.unexamined.saturating_add(unretained);
    // The heap kept as many rows as the presence check can stat. If the
    // page is still short and more candidates exist, the checks hit that limit.
    presence.budget_exhausted |= unretained > 0 && presence.present.len() < DEFAULT_MAX_RESULTS;
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
        Some(presence_stat_budget(DEFAULT_MAX_RESULTS)),
        false,
        explicit_session,
    )
    .expect("test glob discovery without a view failure");
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
    let outcome = bounded_fallback_walk_files_for_target(
        search_root,
        search_root,
        &filters,
        ignored_target,
        "glob",
    );
    let walk_time = walk_started.elapsed();
    let scope_started = Instant::now();
    // Reuse the filtered walk; an incomplete walk cannot prove an empty scope.
    let scope_has_files =
        !outcome.files.is_empty() || outcome.walk_truncated || outcome.skipped_foreign_mounts > 0;
    GlobDiscovery {
        files: outcome.files,
        indexed_mtimes: HashMap::new(),
        unretained_candidates: 0,
        walk_failed: outcome.error.is_some(),
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

#[cfg(test)]
fn format_glob_text(
    files: &[PathBuf],
    pattern: &str,
    project_root: &Path,
    truncated: bool,
) -> String {
    format_glob_output(files, pattern, project_root, truncated).0
}

fn format_glob_output(
    files: &[PathBuf],
    pattern: &str,
    project_root: &Path,
    truncated: bool,
) -> (String, usize) {
    // The existing file-prefix selector must fit long paths too, not only a
    // fixed number of names. The envelope counts only filenames actually printed.
    let mut end = files.len();
    loop {
        let output =
            format_glob_selected(&files[..end], files.len(), pattern, project_root, truncated);
        if output.0.len() <= crate::tool_output::LIST_BODY_BYTES || end == 0 {
            return output;
        }
        end -= 1;
    }
}

fn format_glob_selected(
    files: &[PathBuf],
    reported_files: usize,
    pattern: &str,
    project_root: &Path,
    truncated: bool,
) -> (String, usize) {
    // Convert to relative paths within project
    let relative_files: Vec<PathBuf> = files
        .iter()
        .map(|p| p.strip_prefix(project_root).unwrap_or(p).to_path_buf())
        .collect();

    let header = format!(
        "{} {} matching {}",
        reported_files,
        if reported_files == 1 { "file" } else { "files" },
        pattern
    );

    let mut shown = 0;
    let text = if relative_files.is_empty() {
        header
    } else if relative_files.len() <= MAX_FLAT_FILES {
        shown = relative_files.len();
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
            shown += if names.len() > MAX_FILES_PER_DIRECTORY {
                MAX_DISPLAY_FILES_PER_DIRECTORY
            } else {
                names.len()
            };
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
        (format!("{}\n\n{}", text, GLOB_TRUNCATED_MESSAGE), shown)
    } else {
        (text, shown)
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

    #[test]
    fn list_body_budget_glob_counts_the_long_path_prefix_it_renders() {
        let root = Path::new("/project");
        let files: Vec<_> = (0..20)
            .map(|i| root.join("deep/".repeat(600)).join(format!("file_{i}.rs")))
            .collect();
        let (text, shown) = format_glob_output(&files, "**/*.rs", root, false);
        assert!(shown > 0 && shown < files.len());
        assert!(text.len() <= crate::tool_output::LIST_BODY_BYTES);
        assert_eq!(
            shown,
            text.lines()
                .filter(|line| line.ends_with(".rs") && !line.contains(" matching "))
                .count()
        );
        let data = serde_json::json!({"files":files,"text":text,"rendered_files":shown});
        assert_eq!(crate::subc_format::rendered_glob_file_count(&data), shown);
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
    fn review_search_glob_lower_bound_only_counts_checked_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        for index in 0..=DEFAULT_MAX_RESULTS {
            std::fs::write(project.join(format!("file-{index:03}.txt")), "text").unwrap();
        }
        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(project),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "glob-lower-bound",
            "command": "glob",
            "pattern": "*.txt",
        }))
        .unwrap();
        let response = handle_glob(&request, &ctx);
        let body = &response.data;
        assert_eq!(body["complete"], false, "{body}");
        assert_eq!(body["candidate_count"], DEFAULT_MAX_RESULTS + 1, "{body}");
        assert_eq!(body["unexamined_files"], 1, "{body}");
        assert_eq!(body["total_lower_bound"], DEFAULT_MAX_RESULTS, "{body}");
        let envelope: crate::list_envelope::ListEnvelope =
            serde_json::from_value(body["files_list_envelope"].clone()).unwrap();
        assert_eq!(
            envelope.total,
            crate::list_envelope::Total::AtLeast(DEFAULT_MAX_RESULTS)
        );
    }

    #[test]
    fn review_search_indexed_glob_retains_only_the_page_and_refill_window() {
        use crate::data_layer::checkout::{Checkout, DataLayer};
        use aft_store::ids::{Digest128, FamilyId};

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("checkout");
        std::fs::create_dir(&root).unwrap();
        let window = DEFAULT_MAX_RESULTS + crate::search_index::MISSING_ON_DISK_REFILL_BUDGET;
        let count = window * 2;
        for index in 0..count {
            let path = root.join(format!("file-{index:04}.txt"));
            std::fs::write(&path, "text").unwrap();
            let mtime = filetime::FileTime::from_unix_time(1_700_000_000 + index as i64 / 3, 0);
            filetime::set_file_mtime(path, mtime).unwrap();
        }
        let layer = std::sync::Arc::new(DataLayer::new(8 << 20, 2));
        let family = FamilyId::from_digest(Digest128::of(b"glob-refill-test"));
        let checkout = Checkout::bind(layer, &dir.path().join("store"), family, &root).unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        let mut expected = glob_from_view(view.clone(), &root, &root, "*.txt", None).unwrap();
        sort_paths_by_cached_mtime_desc(&mut expected.files, &root, &expected.indexed_mtimes);
        assert_eq!(expected.files.len(), count);
        for path in expected.files[..12]
            .iter()
            .chain(&expected.files[window..window + 20])
        {
            std::fs::remove_file(path).unwrap();
        }
        let actual = glob_from_view(view, &root, &root, "*.txt", Some(window)).unwrap();
        assert_eq!(actual.entries_visited, count);
        assert_eq!(actual.files.len(), window);
        assert_eq!(actual.files, expected.files[..window]);
        let present = |paths| {
            crate::search_index::take_present_on_disk_with_admission(
                paths,
                DEFAULT_MAX_RESULTS,
                |path: &PathBuf| path.as_path(),
                "glob",
                |_| Ok::<_, ()>(true),
            )
            .unwrap()
            .0
        };
        assert_eq!(actual.unretained_candidates, count - window);
        let compare_refill = |stale: usize, exhausted: bool| {
            let reference = present(expected.files.clone());
            let mut selected = present(actual.files.clone());
            account_for_unretained_glob_candidates(&mut selected, actual.unretained_candidates);
            assert_eq!(selected.present, reference.present);
            assert_eq!(selected.dropped, stale);
            assert_eq!(selected.dropped, reference.dropped);
            assert_eq!(selected.unexamined, reference.unexamined);
            assert_eq!(selected.budget_exhausted, exhausted);
            assert_eq!(selected.budget_exhausted, reference.budget_exhausted);
        };
        compare_refill(12, false);
        let exhausted = crate::search_index::MISSING_ON_DISK_REFILL_BUDGET + 1;
        for path in &expected.files[12..exhausted] {
            std::fs::remove_file(path).unwrap();
        }
        compare_refill(exhausted, true);
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
