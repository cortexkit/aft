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
    let validated = validate(Path::new(raw))?;
    let single_root = search_root(project_root, &validated);
    if single_root.exists() || !raw.chars().any(char::is_whitespace) {
        return Ok(SearchPathResolution::Single(single_root));
    }

    let fragments = raw.split_whitespace().collect::<Vec<_>>();
    if fragments.len() < 2 {
        return Ok(SearchPathResolution::Single(single_root));
    }

    let mut roots = Vec::with_capacity(fragments.len());
    for fragment in &fragments {
        let validated = validate(Path::new(fragment))?;
        let root = search_root(project_root, &validated);
        if !root.exists() {
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
        if keyed.iter().any(|(_, existing_key)| existing_key == &key) {
            continue;
        }
        keyed.push((path, key));
    }

    let mut deduped = Vec::new();
    'outer: for (index, (path, key)) in keyed.iter().enumerate() {
        for (other_index, (_, other_key)) in keyed.iter().enumerate() {
            if index != other_index && key.starts_with(other_key) {
                continue 'outer;
            }
        }
        deduped.push(path.clone());
    }
    deduped
}

pub(crate) fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn search_root(project_root: &Path, validated: &Path) -> PathBuf {
    let path = validated.to_string_lossy();
    resolve_search_scope(project_root, Some(path.as_ref())).root
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
