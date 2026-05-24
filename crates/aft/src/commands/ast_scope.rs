use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast_grep_lang::AstGrepLang;
use crate::context::AppContext;
use crate::protocol::Response;

use super::multi_path::{canonical_key, resolve_path_or_multi, SearchPathResolution};

pub(crate) struct AstScope {
    pub files: Vec<PathBuf>,
    pub scope_warnings: Vec<String>,
    pub no_files_matched_scope: bool,
}

struct SearchRoot {
    path: PathBuf,
    label: Option<String>,
}

pub(crate) fn collect_ast_files(
    req_id: &str,
    command: &str,
    ctx: &AppContext,
    project_root: &Path,
    lang: &AstGrepLang,
    paths: &[String],
    globs: &[String],
) -> Result<AstScope, Response> {
    let roots = resolve_roots(req_id, ctx, project_root, paths)?;

    for root in &roots {
        if !root.path.exists() {
            return Err(Response::error(
                req_id,
                "path_not_found",
                format!(
                    "{}: search path does not exist: {}",
                    command,
                    root.path.display()
                ),
            ));
        }
    }

    let files = walk_roots(project_root, &roots, lang, globs);
    let scope_warnings = scope_warnings(project_root, &roots, lang, globs, &files);
    let no_files_matched_scope = files.is_empty();

    Ok(AstScope {
        files,
        scope_warnings,
        no_files_matched_scope,
    })
}

fn resolve_roots(
    req_id: &str,
    ctx: &AppContext,
    project_root: &Path,
    paths: &[String],
) -> Result<Vec<SearchRoot>, Response> {
    if paths.is_empty() {
        return Ok(vec![SearchRoot {
            path: project_root.to_path_buf(),
            label: None,
        }]);
    }

    let mut roots = Vec::new();
    for path in paths {
        match resolve_path_or_multi(
            path,
            project_root,
            |candidate| ctx.validate_path(req_id, candidate),
            req_id,
        )? {
            SearchPathResolution::Single(root) => roots.push(SearchRoot {
                path: root,
                label: Some(path.clone()),
            }),
            SearchPathResolution::Multi(expanded) => {
                roots.extend(expanded.into_iter().map(|root| SearchRoot {
                    label: Some(root.display().to_string()),
                    path: root,
                }));
            }
        }
    }

    Ok(dedupe_search_roots(roots))
}

fn dedupe_search_roots(roots: Vec<SearchRoot>) -> Vec<SearchRoot> {
    let mut deduped = Vec::new();
    for root in roots {
        let key = canonical_key(&root.path);
        if deduped
            .iter()
            .any(|existing: &SearchRoot| canonical_key(&existing.path) == key)
        {
            continue;
        }
        deduped.push(root);
    }
    deduped
}

fn scope_warnings(
    project_root: &Path,
    roots: &[SearchRoot],
    lang: &AstGrepLang,
    globs: &[String],
    files: &[PathBuf],
) -> Vec<String> {
    let mut warnings = Vec::new();
    let has_include_globs = globs.iter().any(|glob| !glob.starts_with('!'));

    for root in roots.iter().filter(|root| root.label.is_some()) {
        let has_files = if files.iter().any(|file| file.starts_with(&root.path)) {
            true
        } else if has_include_globs {
            // The already-collected list may be empty for this root only because
            // include globs filtered every language file out. In that ambiguous
            // case, do one unfiltered walk for the explicit root so we preserve
            // the existing diagnostic distinction: path has no files vs. glob
            // matched no files.
            !walk_root(project_root, root, lang, &[]).is_empty()
        } else {
            false
        };

        if !has_files {
            warnings.push(format!(
                "{} → no files",
                root.label.as_deref().expect("explicit root label")
            ));
        }
    }

    let matched_relative_paths: HashSet<String> = files
        .iter()
        .map(|file| relative_path_for_globs(project_root, roots, file))
        .collect();

    for include_glob in globs.iter().filter(|glob| !glob.starts_with('!')) {
        if !matched_relative_paths
            .iter()
            .any(|path| glob_matches(include_glob, path))
        {
            warnings.push(format!("{} → no files", include_glob));
        }
    }

    warnings.sort();
    warnings.dedup();
    warnings
}

fn relative_path_for_globs(project_root: &Path, roots: &[SearchRoot], file: &Path) -> String {
    let filter_root = roots
        .iter()
        .find(|root| file.starts_with(&root.path))
        .map(|root| {
            if root.path.starts_with(project_root) {
                project_root
            } else {
                root.path.as_path()
            }
        })
        .unwrap_or(project_root);

    file.strip_prefix(filter_root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn glob_matches(glob: &str, relative_path: &str) -> bool {
    let mut builder = ignore::overrides::OverrideBuilder::new("");
    if builder.add(glob).is_err() {
        return false;
    }

    builder
        .build()
        .map(|overrides| overrides.matched(relative_path, false).is_whitelist())
        .unwrap_or(false)
}

fn walk_roots(
    project_root: &Path,
    roots: &[SearchRoot],
    lang: &AstGrepLang,
    globs: &[String],
) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    roots
        .iter()
        .flat_map(|root| walk_root(project_root, root, lang, globs))
        .filter(|file| seen.insert(canonical_key(file)))
        .collect()
}

fn walk_root(
    project_root: &Path,
    root: &SearchRoot,
    lang: &AstGrepLang,
    globs: &[String],
) -> Vec<PathBuf> {
    use ignore::WalkBuilder;

    let filter_root = if root.path.starts_with(project_root) {
        project_root
    } else {
        root.path.as_path()
    };
    let overrides = build_overrides(filter_root, globs);

    let mut builder = WalkBuilder::new(&root.path);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                return !matches!(
                    name.as_ref(),
                    "node_modules"
                        | "target"
                        | "venv"
                        | ".venv"
                        | ".git"
                        | "__pycache__"
                        | ".tox"
                        | "dist"
                        | "build"
                );
            }
            true
        });

    if let Some(overrides) = overrides {
        builder.overrides(overrides);
    }

    builder
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map_or(false, |ft| ft.is_file()))
        .map(|entry| entry.into_path())
        .filter(|path| lang.matches_path(path))
        .collect()
}

fn build_overrides(root: &Path, globs: &[String]) -> Option<ignore::overrides::Override> {
    if globs.is_empty() {
        return None;
    }

    let mut override_builder = ignore::overrides::OverrideBuilder::new(root);
    for glob in globs {
        if let Some(exclude) = glob.strip_prefix('!') {
            let _ = override_builder.add(&format!("!{}", exclude));
        } else {
            let _ = override_builder.add(glob);
        }
    }

    override_builder.build().ok()
}
