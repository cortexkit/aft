use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::context::AppContext;

/// Spells the files a read manifest names so a host's read rules match the
/// file whose content was returned.
///
/// Each path is resolved to its real location first, so a request that goes
/// through a symlink cannot hide where a returned file lives. A real location
/// under the project root is then spelled with the configured project root,
/// because host rules are written relative to the root the host configured,
/// and that root may itself be an alias of its canonical form (macOS
/// `/var` for `/private/var`, Windows `C:\` for `\\?\C:\`). Any other path,
/// including every path when no project root is configured, is reported at its
/// canonical location. The requested file's own spelling is never used to
/// derive a manifest path: hosts authorize that spelling before dispatch, and
/// deriving from it would let a symlink at a different directory depth make a
/// manifest path name a different file.
pub(crate) struct ReadManifestPaths {
    root: Option<ManifestRoot>,
}

struct ManifestRoot {
    canonical: PathBuf,
    spelling: PathBuf,
}

impl ReadManifestPaths {
    pub(crate) fn for_context(ctx: &AppContext) -> Self {
        let root = ctx.config().project_root.clone().and_then(|root| {
            Some(ManifestRoot {
                canonical: canonical_path(&root)?,
                spelling: normalize_path(&root),
            })
        });
        Self { root }
    }

    pub(crate) fn map(&self, path: &Path) -> PathBuf {
        let Some(real) = real_location(path) else {
            return normalize_path(path);
        };
        let Some(root) = &self.root else {
            return real;
        };
        match real.strip_prefix(&root.canonical) {
            Ok(relative) => normalize_path(&root.spelling.join(relative)),
            Err(_) => real,
        }
    }

    pub(crate) fn map_set<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a PathBuf>,
    ) -> BTreeSet<PathBuf> {
        paths.into_iter().map(|path| self.map(path)).collect()
    }
}

/// Rewrites a response's `content_files` into read-manifest spelling. The
/// manifest is a set: every producer reports it sorted (in `Path` component
/// order) and without duplicates, so two spellings of one file collapse to a
/// single entry and the order never depends on how the files were displayed.
pub(crate) fn normalize_response_content_files(ctx: &AppContext, data: &mut serde_json::Value) {
    let Some(paths) = data
        .get("content_files")
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };
    let paths: BTreeSet<_> = paths
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(|path| ctx.resolve_relative_path(Path::new(path)))
        .collect();
    let mapper = ReadManifestPaths::for_context(ctx);
    data["content_files"] = serde_json::json!(mapper.map_set(paths.iter()));
}

fn canonical_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .ok()
        .map(|path| normalize_path(&path))
}

/// Canonicalizes `path`, or for a path that does not exist (a file read at a
/// git revision, for one) its nearest existing ancestor followed by the
/// missing components, so symlinks above a missing file are still resolved.
fn real_location(path: &Path) -> Option<PathBuf> {
    if let Some(canonical) = canonical_path(path) {
        return Some(canonical);
    }
    let mut missing = Vec::new();
    let mut current = path;
    while let Some(parent) = current.parent() {
        missing.push(current.file_name()?.to_os_string());
        current = parent;
        if let Some(canonical) = canonical_path(current) {
            let mut real = canonical;
            real.extend(missing.iter().rev());
            return Some(normalize_path(&real));
        }
    }
    None
}

fn normalize_path(path: &Path) -> PathBuf {
    crate::path_identity::normalize_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::parser::TreeSitterProvider;

    fn ctx_with_root(root: Option<PathBuf>) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: root,
                ..Config::default()
            },
        )
    }

    /// `fs::canonicalize` returns a verbatim `\\?\C:\` path on Windows and a
    /// `/private/var` path for a macOS temp directory; neither may leak into a
    /// manifest whose project root was configured in the plain spelling.
    #[test]
    fn manifest_uses_configured_root_spelling_for_canonical_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
        let mapper = ReadManifestPaths::for_context(&ctx_with_root(Some(root.clone())));
        let canonical = std::fs::canonicalize(root.join("src/a.ts")).unwrap();

        let expected = normalize_path(&root.join("src/a.ts"));
        assert_eq!(mapper.map(&canonical), expected);
        assert_eq!(mapper.map(&root.join("src/a.ts")), expected);
        // A file read at a revision need not exist in the working tree.
        assert_eq!(
            mapper.map(&root.join("src/gone/b.ts")),
            normalize_path(&root.join("src/gone/b.ts"))
        );
    }

    #[test]
    fn manifest_names_unconfigured_and_outside_sources_canonically() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("a.ts"), "export const a = 1;\n").unwrap();
        let canonical = crate::path_identity::canonicalize_normalized(&outside.join("a.ts"));
        for root in [None, Some(project)] {
            let mapper = ReadManifestPaths::for_context(&ctx_with_root(root.clone()));
            assert_eq!(mapper.map(&outside.join("a.ts")), canonical, "{root:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn manifest_resolves_symlinks_at_any_directory_depth() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::write(root.join("src/secret.ts"), "export const s = 1;\n").unwrap();
        symlink(root.join("src"), root.join("a/link")).unwrap();
        symlink(&root, root.join("src/up")).unwrap();
        let mapper = ReadManifestPaths::for_context(&ctx_with_root(Some(root.clone())));

        let secret = normalize_path(&root.join("src/secret.ts"));
        for spelling in [
            "a/link/secret.ts",
            "src/up/src/secret.ts",
            "src/up/a/link/secret.ts",
        ] {
            assert_eq!(mapper.map(&root.join(spelling)), secret, "{spelling}");
        }
        // A missing file below a symlink is named under the symlink's target.
        assert_eq!(
            mapper.map(&root.join("a/link/gone.ts")),
            normalize_path(&root.join("src/gone.ts"))
        );
    }
}
