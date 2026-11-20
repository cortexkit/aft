use std::path::{Path, PathBuf};

use crate::protocol::Response;
use crate::search_index::resolve_search_scope;

#[derive(Debug)]
pub(crate) enum SearchPathResolution {
    Single(PathBuf),
    Multi(Vec<PathBuf>),
}

pub(crate) fn resolve_path_or_multi<F>(
    raw: &str,
    project_root: &Path,
    validate: F,
    req_id: &str,
) -> Result<SearchPathResolution, Response>
where
    F: Fn(&Path) -> Result<PathBuf, Response>,
{
    resolve_path_or_multi_for_platform(raw, project_root, validate, req_id, cfg!(windows))
}

fn resolve_path_or_multi_for_platform<F>(
    raw: &str,
    project_root: &Path,
    validate: F,
    req_id: &str,
    windows: bool,
) -> Result<SearchPathResolution, Response>
where
    F: Fn(&Path) -> Result<PathBuf, Response>,
{
    let single_root = match validate(Path::new(raw)) {
        Ok(validated) => Some(search_root(project_root, &validated)),
        Err(error)
            if windows
                && raw.chars().any(char::is_whitespace)
                && error.data["code"] == "path_access_denied"
                && error.data["os_error"] == 123 =>
        {
            // Windows cannot stat two drive paths as one literal name. This is
            // not an access denial: validate each fragment before probing it.
            None
        }
        Err(error) => return Err(error),
    };
    if let Some(single_root) = &single_root {
        if crate::bounded_io::metadata(single_root).is_ok() || !raw.chars().any(char::is_whitespace)
        {
            return Ok(SearchPathResolution::Single(single_root.clone()));
        }
    }

    let fragments = raw.split_whitespace().collect::<Vec<_>>();
    if fragments.len() < 2 {
        return Ok(SearchPathResolution::Single(
            single_root.expect("a single path was validated"),
        ));
    }

    let mut roots = Vec::with_capacity(fragments.len());
    for fragment in &fragments {
        let validated = validate(Path::new(fragment))?;
        let root = search_root(project_root, &validated);
        if crate::bounded_io::metadata(&root).is_err() {
            return Err(Response::error(
                req_id,
                "path_not_found",
                format!(
                    "path does not exist: {} (interpreted as one of {} space-separated paths; \
                     if this was meant as a single path containing a space, quote it)",
                    root.display(),
                    fragments.len(),
                ),
            ));
        }
        roots.push(root);
    }

    let roots = dedupe_nested_paths(roots);
    if roots.len() == 1 {
        Ok(SearchPathResolution::Single(
            roots.into_iter().next().expect("one root"),
        ))
    } else {
        Ok(SearchPathResolution::Multi(roots))
    }
}

pub(crate) fn dedupe_nested_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut keyed = Vec::new();
    for path in paths {
        let key = canonical_key(&path);
        if keyed
            .iter()
            .any(|(_, existing_key, _)| existing_key == &key)
        {
            continue;
        }
        let is_dir = crate::bounded_io::metadata(&path).is_ok_and(|metadata| metadata.is_dir());
        keyed.push((path, key, is_dir));
    }

    let mut deduped = Vec::new();
    'outer: for (index, (path, key, is_dir)) in keyed.iter().enumerate() {
        for (other_index, (_, other_key, other_is_dir)) in keyed.iter().enumerate() {
            if index != other_index
                && *is_dir
                && *other_is_dir
                && path_is_nested_under(key, other_key)
            {
                continue 'outer;
            }
        }
        deduped.push(path.clone());
    }
    deduped
}

pub(crate) fn canonical_key(path: &Path) -> PathBuf {
    crate::bounded_io::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_is_nested_under(path: &Path, ancestor: &Path) -> bool {
    let mut path_components = path.components();
    for ancestor_component in ancestor.components() {
        match path_components.next() {
            Some(path_component) if path_component == ancestor_component => {}
            _ => return false,
        }
    }
    path_components.next().is_some()
}

fn search_root(project_root: &Path, validated: &Path) -> PathBuf {
    let path = validated.to_string_lossy();
    resolve_search_scope(project_root, Some(path.as_ref())).root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_compound_literal_error_does_not_hide_outside_fragment_denial() {
        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let inside = project.path().join("a");
        std::fs::create_dir(&inside).unwrap();
        let raw = format!("{} {}", inside.display(), outside.path().display());
        let error = resolve_path_or_multi_for_platform(
            &raw,
            project.path(),
            |path| {
                if path == Path::new(&raw) {
                    Err(Response::error_with_data(
                        "test",
                        "path_access_denied",
                        "invalid literal name",
                        serde_json::json!({"os_error":123}),
                    ))
                } else if path == outside.path() {
                    Err(Response::error(
                        "test",
                        "path_outside_root",
                        "outside fragment",
                    ))
                } else {
                    Ok(path.to_path_buf())
                }
            },
            "test",
            true,
        )
        .unwrap_err();
        assert_eq!(error.data["code"], "path_outside_root");
    }

    #[test]
    fn multi_path_reports_missing_fragment() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().to_path_buf();
        std::fs::create_dir(project_root.join("src")).expect("create src");
        assert!(!project_root.join("does-not-exist").exists());

        let result = resolve_path_or_multi(
            "src does-not-exist",
            &project_root,
            |candidate| Ok(candidate.to_path_buf()),
            "test-id",
        );

        let err = result.expect_err("missing fragment should return an error");
        let body = serde_json::to_value(err).expect("serialize response");
        assert!(body.to_string().contains("path_not_found"));
        assert!(body.to_string().contains("does-not-exist"));
    }

    #[test]
    fn dedupe_nested_paths_keeps_sibling_dirs_with_shared_prefix() {
        let temp = tempfile::tempdir().expect("tempdir");
        let app = temp.path().join("packages/app");
        let app_old = temp.path().join("packages/app-old");
        std::fs::create_dir_all(&app).expect("create app");
        std::fs::create_dir_all(&app_old).expect("create app-old");

        let deduped = dedupe_nested_paths(vec![app.clone(), app_old.clone()]);

        assert_eq!(deduped.len(), 2);
        assert!(deduped.contains(&app));
        assert!(deduped.contains(&app_old));
    }
}
