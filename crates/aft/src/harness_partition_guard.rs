//! Refuses writes into the legacy per-harness partitions
//! `<storage>/<harness>/(callgraph|inspect)`: a write meant for today's storage
//! layout must never land inside one.

use std::io;
use std::path::{Component, Path};

/// Cheap structural predicate: true when `candidate` points anywhere under
/// `<storage>/<harness>/(callgraph|inspect)`.
pub fn is_legacy_harness_partition_path(storage_root: &Path, candidate: &Path) -> bool {
    let storage_root = crate::path_identity::lexical_normalize(storage_root);
    let candidate = if candidate.is_absolute() {
        crate::path_identity::lexical_normalize(candidate)
    } else {
        crate::path_identity::lexical_normalize(&storage_root.join(candidate))
    };

    let Ok(relative) = candidate.strip_prefix(&storage_root) else {
        return false;
    };

    let mut components = relative.components();
    let Some(Component::Normal(_harness)) = components.next() else {
        return false;
    };
    let Some(Component::Normal(domain)) = components.next() else {
        return false;
    };

    matches!(domain.to_str(), Some("callgraph" | "inspect"))
}

#[track_caller]
pub fn debug_assert_not_legacy_harness_partition_path(storage_root: &Path, candidate: &Path) {
    debug_assert!(
        !is_legacy_harness_partition_path(storage_root, candidate),
        "new-layout write path must not point into a legacy harness partition: {}",
        candidate.display()
    );
}

pub fn refuse_legacy_partition_write(
    storage_root: &Path,
    candidate: &Path,
    operation: &str,
) -> io::Result<()> {
    if is_legacy_harness_partition_path(storage_root, candidate) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing {operation} into legacy harness partition {}",
                candidate.display()
            ),
        ));
    }
    Ok(())
}

#[track_caller]
pub fn guard_new_layout_write_path(
    storage_root: &Path,
    candidate: &Path,
    operation: &str,
) -> io::Result<()> {
    debug_assert_not_legacy_harness_partition_path(storage_root, candidate);
    refuse_legacy_partition_write(storage_root, candidate, operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;
    use std::path::PathBuf;

    #[test]
    fn legacy_partition_guard_matches_exact_domains() {
        let storage_root = PathBuf::from("/tmp/aft-storage");
        assert!(is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("opencode/callgraph/0123456789abcdef.current")
        ));
        assert!(is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("pi/inspect/0123456789abcdef.sqlite")
        ));
        assert!(!is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("opencode/callgraph-old/0123456789abcdef.sqlite")
        ));
        assert!(!is_legacy_harness_partition_path(
            &storage_root,
            &storage_root.join("index/0123456789abcdef.sqlite")
        ));
        assert!(!is_legacy_harness_partition_path(
            &storage_root,
            Path::new("/elsewhere/opencode/callgraph/0123456789abcdef.sqlite")
        ));
    }

    #[test]
    fn debug_assert_and_refusal_cover_legacy_write_paths() {
        let storage_root = PathBuf::from("/tmp/aft-storage");
        let legacy_target = storage_root.join("opencode/callgraph/0123456789abcdef.sqlite");
        let panic = catch_unwind(|| {
            debug_assert_not_legacy_harness_partition_path(&storage_root, &legacy_target);
        });
        assert!(panic.is_err());

        let error = refuse_legacy_partition_write(&storage_root, &legacy_target, "publish")
            .expect_err("legacy write must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error
            .to_string()
            .contains("refusing publish into legacy harness partition"));
    }
}
