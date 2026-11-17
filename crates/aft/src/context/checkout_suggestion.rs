use super::{normalize_path, relative_components_under_root};
use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub(super) fn checkout_counterpart(
    original: &Path,
    checkout: &Path,
    indexed: Option<&crate::data_layer::runtime::DataCheckout>,
) -> Option<PathBuf> {
    let relative = relative_components_under_root(original)?;
    if relative.as_os_str().is_empty() {
        return None;
    }

    // Normalize before probing so a spelling such as /../secret never stats
    // the unnormalized path supplied by the caller.
    let normalized_original = normalize_path(original);
    match std::fs::symlink_metadata(&normalized_original) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => return None,
    }

    let checkout = std::fs::canonicalize(checkout).ok()?;
    let candidate = normalize_path(&checkout.join(relative));
    if !candidate.starts_with(&checkout) {
        return None;
    }

    if let Ok(canonical_candidate) = std::fs::canonicalize(&candidate) {
        return canonical_candidate
            .starts_with(&checkout)
            .then_some(canonical_candidate);
    }

    // Suffix recovery must not turn a traversal or an escaping symlink into
    // a suggestion. Only an already-published inventory of this checkout is
    // consulted; a denied read must not start a filesystem scan.
    if original
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return None;
    }
    let indexed = indexed?;
    if crate::inspect::job::normalize_path(indexed.root())
        != crate::inspect::job::normalize_path(&checkout)
    {
        return None;
    }
    let view = indexed.view()?;
    let relative = unique_indexed_checkout_suffix(original, view.current_state().paths())?;
    let candidate = std::fs::canonicalize(checkout.join(relative)).ok()?;
    (candidate.starts_with(&checkout) && candidate.is_file()).then_some(candidate)
}

fn unique_indexed_checkout_suffix(
    original: &Path,
    paths: &[aft_store::checkout::PathRecord],
) -> Option<PathBuf> {
    // Refuse recovery outside these work budgets rather than claim uniqueness
    // from an incomplete inventory. Without a suggestion the path takes the
    // normal out-of-scope route and asks for permission as before.
    const MAX_PATHS: usize = 100_000;
    const MAX_COMPONENTS: usize = 64;
    if paths.len() > MAX_PATHS {
        return None;
    }
    let mut components = Vec::new();
    for component in original.components() {
        if let Component::Normal(component) = component {
            if components.len() == MAX_COMPONENTS {
                return None;
            }
            components.push(component.to_str()?);
        }
    }
    for start in 0..components.len().saturating_sub(1) {
        let suffix = components[start..].join("/");
        let mut found = None;
        for record in paths {
            if record.path.ends_with(suffix.as_bytes())
                && (record.path.len() == suffix.len()
                    || record.path[record.path.len() - suffix.len() - 1] == b'/')
            {
                if found.is_some() {
                    return None;
                }
                found = Some(record.path.as_slice());
            }
        }
        if let Some(path) = found {
            return Some(PathBuf::from(std::str::from_utf8(path).ok()?));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    #[test]
    fn checkout_suffix_matches_verbatim_checkout_to_ordinary_index_root() {
        use crate::data_layer::runtime::{DataCheckout, Fresh};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        let file = root.join("crates/project/src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "pub fn marker() {}\n").unwrap();
        let verbatim = std::fs::canonicalize(&root).unwrap();
        let ordinary = crate::windows_path::normalize_windows_path(&verbatim);
        assert_ne!(ordinary, verbatim, "exercise both Windows path spellings");
        let indexed = DataCheckout::open_planes(
            &ordinary,
            &dir.path().join("index"),
            None,
            crate::data_layer::checkout::Planes::ALL,
        )
        .unwrap();
        assert!(matches!(
            indexed.fresh_view(std::time::Duration::from_secs(120)),
            Fresh::Ready(_)
        ));
        assert_eq!(indexed.root(), ordinary);
        let missing = dir.path().join("project/src/lib.rs");
        assert_eq!(
            super::checkout_counterpart(&missing, &verbatim, Some(&indexed)),
            Some(std::fs::canonicalize(&file).unwrap())
        );
        assert_eq!(
            indexed.root(),
            ordinary,
            "comparison must not re-key the store"
        );
    }
}
