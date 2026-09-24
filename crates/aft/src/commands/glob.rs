use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::grep_executor::bounded_fallback_walk_files;
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, has_any_project_file_from, resolve_search_scope, sort_paths_by_mtime_desc,
};

use super::multi_path::{canonical_key, resolve_path_or_multi, SearchPathResolution};

#[derive(Debug)]
struct GlobDiscovery {
    files: Vec<PathBuf>,
    walk_truncated: bool,
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

    if let Err(error) = build_path_filters(&[pattern.to_string()], &[]) {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("glob: invalid pattern: {}", error),
        );
    }

    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
    let search_roots = match req.params.get("path").and_then(|value| value.as_str()) {
        Some(path) => match resolve_path_or_multi(
            path,
            &project_root,
            |candidate| ctx.validate_path(&req.id, candidate),
            &req.id,
        ) {
            Ok(SearchPathResolution::Single(root)) => vec![root],
            Ok(SearchPathResolution::Multi(roots)) => roots,
            Err(resp) => return resp,
        },
        None => vec![resolve_search_scope(&project_root, None).root],
    };

    // Return clear error if the search path doesn't exist
    if let Some(missing_root) = search_roots.iter().find(|root| !root.exists()) {
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
    let (
        mut files,
        walk_truncated,
        source,
        entries_visited,
        walk_time,
        scope_has_files,
        scope_probe,
        skipped_foreign_mounts,
    ) = if search_roots.len() == 1 {
        let discovery = glob_root(
            ctx,
            &project_root,
            &search_roots[0],
            pattern,
            DEFAULT_MAX_RESULTS + 1,
        );
        (
            discovery.files,
            discovery.walk_truncated,
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
            .map(|root| glob_root(ctx, &project_root, root, pattern, DEFAULT_MAX_RESULTS + 1))
            .collect();
        let walk_truncated = discoveries.iter().any(|d| d.walk_truncated);
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
        let files = merge_glob_files(discoveries.into_iter().flat_map(|d| d.files).collect());
        (
            files,
            walk_truncated,
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
    // Glob's public contract is newest-first. Sort before truncating so the cap
    // keeps the most recently modified matches instead of the lexically first.
    sort_paths_by_mtime_desc(&mut files, &project_root);
    let total = files.len();
    let result_truncated = total > DEFAULT_MAX_RESULTS;
    if result_truncated {
        files.truncate(DEFAULT_MAX_RESULTS);
    }

    let mut body = serde_json::json!({
        "text": format_glob_text(&files, pattern, &project_root, result_truncated),
        "complete": !walk_truncated && skipped_foreign_mounts == 0,
        "no_files_matched_scope": !scope_has_files,
        "skipped_foreign_mounts": skipped_foreign_mounts,
        "files": files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "total": total,
        "truncated": result_truncated,
    });
    if walk_truncated {
        body["walk_truncated"] = serde_json::Value::Bool(true);
    }
    // Disclose the trigram index state and whether any root was answered by a
    // filesystem walk rather than the index.
    body["index"] = crate::commands::grep::trigram_index_disclosure(ctx);
    body["fallback"] = if source == "index" {
        serde_json::Value::Null
    } else {
        serde_json::Value::String("filesystem".to_string())
    };

    let rendered_count =
        crate::subc_format::report_rendered_row_count("glob", &body).unwrap_or(files.len());

    let envelope = crate::list_surfaces::glob::build_glob_envelope(
        rendered_count,
        total,
        result_truncated,
        walk_truncated,
        skipped_foreign_mounts,
    );

    if let Some(env) = envelope {
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

fn scope_has_files(project_root: &Path, search_root: &Path) -> bool {
    let catch_all = build_path_filters(&["**/*".to_string()], &[]).expect("valid catch-all glob");
    has_any_project_file_from(project_root, search_root, &catch_all)
}

fn glob_root(
    ctx: &AppContext,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    max_results: usize,
) -> GlobDiscovery {
    let search_root_text = search_root.to_string_lossy();
    let search_scope = resolve_search_scope(project_root, Some(search_root_text.as_ref()));
    let indexed_snapshot = {
        let search_index = ctx
            .search_index()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match search_index.as_ref() {
            Some(index) if index.ready && search_scope.use_index => Some(index.snapshot()),
            _ => None,
        }
    };
    let indexed = indexed_snapshot.map(|snapshot| {
        let (files, scope_has_files, entries_visited) =
            snapshot.glob_profiled(pattern, &search_scope.root, false);
        GlobDiscovery {
            entries_visited,
            files,
            walk_truncated: false,
            source: "index",
            walk_time: Duration::ZERO,
            scope_has_files,
            scope_probe: Duration::ZERO,
            skipped_foreign_mounts: 0,
        }
    });

    match indexed {
        Some(discovery) => discovery,
        None => {
            if search_scope.use_index {
                super::configure::trigger_search_index_reload_if_evicted(ctx);
            }
            if !search_scope.use_index {
                let walk_started = Instant::now();
                if let Some(outcome) =
                    super::grep::ripgrep_glob(&search_scope.root, pattern, max_results)
                {
                    let walk_time = walk_started.elapsed();
                    let scope_started = Instant::now();
                    let scope_has_files = scope_has_files(project_root, &search_scope.root);
                    return GlobDiscovery {
                        files: outcome.files,
                        walk_truncated: outcome.walk_truncated,
                        source: "fallback",
                        entries_visited: outcome.entries_visited,
                        walk_time,
                        scope_has_files,
                        scope_probe: scope_started.elapsed(),
                        skipped_foreign_mounts: 0,
                    };
                }
            }
            fallback_glob(project_root, &search_scope.root, pattern)
        }
    }
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
    project_root: &std::path::Path,
    search_root: &std::path::Path,
    pattern: &str,
) -> GlobDiscovery {
    let filters = build_path_filters(&[pattern.to_string()], &[]).unwrap_or_default();
    // Match the glob pattern relative to the search root so that a bare
    // filename like `a.rs` matches a file directly under `path` (e.g.
    // `src/a.rs` when `path` is `src`). Using `project_root` as the filter
    // root would make the relative path `src/a.rs`, which `a.rs` does not
    // match. The `ripgrep_glob` path already passes `search_root` as both
    // arguments, so this keeps the two fallback routes in agreement.
    let walk_started = Instant::now();
    let outcome = bounded_fallback_walk_files(search_root, search_root, &filters);
    let walk_time = walk_started.elapsed();
    let scope_started = Instant::now();
    let scope_has_files = scope_has_files(project_root, search_root);
    GlobDiscovery {
        files: outcome.files,
        walk_truncated: outcome.walk_truncated,
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
