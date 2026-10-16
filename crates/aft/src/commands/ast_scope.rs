use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast_grep_lang::AstGrepLang;
use crate::context::{AppContext, WorkerSearchTool};
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

#[allow(clippy::too_many_arguments)]
// Searches supply a read session; replacement scopes retain write authorization.
pub(crate) fn collect_ast_files(
    req_id: &str,
    read_session: Option<&str>,
    command: &str,
    ctx: &AppContext,
    project_root: &Path,
    lang: &AstGrepLang,
    paths: &[String],
    globs: &[String],
) -> Result<AstScope, Response> {
    let search_tool = read_session.map(|_| WorkerSearchTool::AstGrepSearch);
    let roots = resolve_roots(req_id, read_session, ctx, project_root, paths, search_tool)?;

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
    read_session: Option<&str>,
    ctx: &AppContext,
    project_root: &Path,
    paths: &[String],
    search_tool: Option<WorkerSearchTool>,
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
            |candidate| match (read_session, search_tool) {
                (Some(session), Some(search_tool)) => {
                    ctx.validate_search_path(req_id, session, candidate, search_tool)
                }
                (Some(session), None) => ctx.validate_read_path(req_id, session, candidate),
                (None, _) => ctx.validate_path(req_id, candidate),
            },
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
        // Compile each glob once and test every path against it; compiling it
        // again per path made this check cost a glob build per walked file.
        let matcher = glob_matcher(include_glob);
        if !matcher.is_some_and(|matcher| {
            matched_relative_paths
                .iter()
                .any(|path| matcher.matched(path, false).is_whitelist())
        }) {
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

fn glob_matcher(glob: &str) -> Option<ignore::overrides::Override> {
    #[cfg(test)]
    work_counters::GLOB_BUILDS.with(|count| count.set(count.get() + 1));
    let mut builder = ignore::overrides::OverrideBuilder::new("");
    builder.add(glob).ok()?;
    builder.build().ok()
}

fn walk_roots(
    project_root: &Path,
    roots: &[SearchRoot],
    lang: &AstGrepLang,
    globs: &[String],
) -> Vec<PathBuf> {
    // One walk never yields a path twice (symlinks are not followed), so
    // canonicalizing every file to dedupe is only needed when several roots
    // can overlap. Canonicalizing is a filesystem call per file.
    if let [root] = roots {
        return walk_root(project_root, root, lang, globs);
    }
    let mut seen = HashSet::new();
    roots
        .iter()
        .flat_map(|root| walk_root(project_root, root, lang, globs))
        .filter(|file| {
            #[cfg(test)]
            work_counters::FILE_CANONICALIZATIONS.with(|count| count.set(count.get() + 1));
            seen.insert(canonical_key(file))
        })
        .collect()
}

/// Per-thread counts of the filesystem and glob work a scope walk does, so
/// tests can pin it without timing anything.
#[cfg(test)]
mod work_counters {
    use std::cell::Cell;

    thread_local! {
        pub(super) static GLOB_BUILDS: Cell<usize> = const { Cell::new(0) };
        pub(super) static FILE_CANONICALIZATIONS: Cell<usize> = const { Cell::new(0) };
    }
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
    // Prevent a disappearing child mount from making ReadDir::drop abort on ENXIO.
    builder.same_file_system(true).hidden(true);
    crate::context::apply_project_ignore_rules(&mut builder, &root.path).filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::parser::TreeSitterProvider;

    /// Scope collection must compile each include glob once (not once per
    /// walked file) and must not canonicalize every file of a single-root walk.
    #[test]
    fn single_root_scope_compiles_globs_once_and_skips_per_file_canonicalize() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for index in 0..50 {
            std::fs::write(root.join(format!("src/f{index:02}.ts")), "const x = 1;\n").unwrap();
        }
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), config);
        let lang = AstGrepLang::from_str("typescript").unwrap();
        let globs = vec!["src/**".to_string(), "missing/**".to_string()];

        let globs_before = work_counters::GLOB_BUILDS.with(|count| count.get());
        let canon_before = work_counters::FILE_CANONICALIZATIONS.with(|count| count.get());
        let scope = match collect_ast_files(
            "scope",
            Some("scope-session"),
            "ast_search",
            &ctx,
            &root,
            &lang,
            &[],
            &globs,
        ) {
            Ok(scope) => scope,
            Err(_) => panic!("scope collection failed"),
        };
        let glob_builds = work_counters::GLOB_BUILDS.with(|count| count.get()) - globs_before;
        let canonicalizations =
            work_counters::FILE_CANONICALIZATIONS.with(|count| count.get()) - canon_before;

        assert_eq!(scope.files.len(), 50);
        assert_eq!(
            scope.scope_warnings,
            vec!["missing/** → no files".to_string()]
        );
        assert_eq!(glob_builds, 2, "one glob build per include glob");
        assert_eq!(
            canonicalizations, 0,
            "a single root needs no per-file dedupe"
        );
    }

    /// Overlapping roots still dedupe files that both walks reach.
    #[test]
    fn overlapping_roots_still_dedupe_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join("src/inner")).unwrap();
        std::fs::write(root.join("src/a.ts"), "const a = 1;\n").unwrap();
        std::fs::write(root.join("src/inner/b.ts"), "const b = 1;\n").unwrap();
        let lang = AstGrepLang::from_str("typescript").unwrap();
        let roots = vec![
            SearchRoot {
                path: root.join("src"),
                label: Some("src".into()),
            },
            SearchRoot {
                path: root.join("src/inner"),
                label: Some("src/inner".into()),
            },
        ];
        let mut files = walk_roots(&root, &roots, &lang, &[]);
        files.sort();
        assert_eq!(
            files,
            vec![root.join("src/a.ts"), root.join("src/inner/b.ts")]
        );
    }
}
