use std::collections::{BTreeMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::search_index::{
    build_path_filters, resolve_search_scope, sort_paths_by_mtime_desc, walk_project_files_from,
};

use super::multi_path::{canonical_key, resolve_path_or_multi, SearchPathResolution};

const MAX_GLOB_RESULTS: usize = 100;
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
    let scope_has_files = search_roots
        .iter()
        .any(|root| scope_has_files(&project_root, root));

    let mut files = if search_roots.len() == 1 {
        glob_root(
            ctx,
            &project_root,
            &search_roots[0],
            pattern,
            MAX_GLOB_RESULTS + 1,
        )
    } else {
        merge_glob_files(
            search_roots
                .iter()
                .flat_map(|root| glob_root(ctx, &project_root, root, pattern, MAX_GLOB_RESULTS + 1))
                .collect(),
        )
    };
    let total = files.len();
    let truncated = total > MAX_GLOB_RESULTS;
    if truncated {
        files.truncate(MAX_GLOB_RESULTS);
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "text": format_glob_text(&files, pattern, &project_root, truncated),
            "complete": true,
            "no_files_matched_scope": !scope_has_files,
            "files": files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            "total": total,
            "truncated": truncated,
        }),
    )
}

fn scope_has_files(project_root: &Path, search_root: &Path) -> bool {
    walk_project_files_from(
        project_root,
        search_root,
        &build_path_filters(&["**/*".to_string()], &[]).expect("valid catch-all glob"),
    )
    .into_iter()
    .next()
    .is_some()
}

fn glob_root(
    ctx: &AppContext,
    project_root: &Path,
    search_root: &Path,
    pattern: &str,
    max_results: usize,
) -> Vec<PathBuf> {
    let search_root_text = search_root.to_string_lossy();
    let search_scope = resolve_search_scope(project_root, Some(search_root_text.as_ref()));
    let search_index = ctx.search_index().borrow();
    match search_index.as_ref() {
        Some(index) if index.ready && search_scope.use_index => {
            index.glob(pattern, &search_scope.root)
        }
        _ => {
            if !search_scope.use_index {
                if let Some(rg_files) =
                    super::grep::ripgrep_glob(&search_scope.root, pattern, max_results)
                {
                    return rg_files;
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
    sort_paths_by_mtime_desc(&mut deduped);
    deduped
}

fn fallback_glob(
    project_root: &std::path::Path,
    search_root: &std::path::Path,
    pattern: &str,
) -> Vec<std::path::PathBuf> {
    let filters = build_path_filters(&[pattern.to_string()], &[]).unwrap_or_default();
    let filter_root = if search_root.starts_with(project_root) {
        project_root
    } else {
        search_root
    };
    let mut files = walk_project_files_from(filter_root, search_root, &filters);
    sort_paths_by_mtime_desc(&mut files);
    files
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
