pub use aft_db::standing_roots::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, IndexConfig, IndexKind, IndexRootConfig};
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use tempfile::tempdir;

    fn config(storage: &Path, roots: Vec<IndexRootConfig>) -> Config {
        Config {
            storage_dir: Some(storage.to_path_buf()),
            index: IndexConfig { roots },
            ..Config::default()
        }
    }

    fn root(path: &Path, indexes: Vec<IndexKind>) -> IndexRootConfig {
        IndexRootConfig {
            path: path.to_string_lossy().into_owned(),
            indexes,
        }
    }

    #[test]
    fn publication_is_a_noop_after_bind_epoch_revocation() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let mut report = roots.reconcile(&cfg).unwrap();
        let entry = report.active_entries.remove(0);
        let build = roots.admit_build(&entry.literal_path).unwrap();
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        let cache_dir = storage.path().join("index").join(&entry.artifact_key);
        let lease = crate::root_cache::WriterLease::acquire_shared(
            crate::root_cache::RootCacheDomain::Index,
            &cache_dir,
            &entry.artifact_key,
            &entry.resolved_target,
        )
        .unwrap()
        .unwrap();
        roots.begin_case_a_bind(&entry.literal_path).unwrap();
        let published = std::sync::atomic::AtomicBool::new(false);
        let result = roots
            .publish_if_current(
                &entry.literal_path,
                build.publication,
                lease.as_ref(),
                || true,
                || true,
                || published.store(true, Ordering::SeqCst),
            )
            .unwrap();
        assert!(result.is_none());
        assert!(!published.load(Ordering::SeqCst));
    }

    #[test]
    fn publication_fence_holds_epoch_mutex_through_final_rename() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let mut report = roots.reconcile(&cfg).unwrap();
        let entry = report.active_entries.remove(0);
        let admission = roots.admit_build(&entry.literal_path).unwrap();
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        let cache_dir = storage.path().join("index").join(&entry.artifact_key);
        let lease = crate::root_cache::WriterLease::acquire_shared(
            crate::root_cache::RootCacheDomain::Index,
            &cache_dir,
            &entry.artifact_key,
            &entry.resolved_target,
        )
        .unwrap()
        .unwrap();
        let (rename_entered_tx, rename_entered_rx) = std::sync::mpsc::channel();
        let (release_rename_tx, release_rename_rx) = std::sync::mpsc::channel();
        let publish_roots = roots.clone();
        let publish_literal = entry.literal_path.clone();
        let publisher = std::thread::spawn(move || {
            publish_roots
                .publish_if_current(
                    &publish_literal,
                    admission.publication,
                    lease.as_ref(),
                    || true,
                    || true,
                    || {
                        rename_entered_tx.send(()).unwrap();
                        release_rename_rx.recv().unwrap();
                    },
                )
                .unwrap()
        });
        rename_entered_rx.recv().unwrap();
        let (bind_done_tx, bind_done_rx) = std::sync::mpsc::channel();
        let bind_roots = roots.clone();
        let bind_literal = entry.literal_path.clone();
        let binder = std::thread::spawn(move || {
            bind_done_tx
                .send(bind_roots.begin_case_a_bind(&bind_literal))
                .unwrap();
        });
        assert!(bind_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err());
        release_rename_tx.send(()).unwrap();
        assert!(publisher.join().unwrap().is_some());
        // Only the 100 ms check above is about ordering: the bind must not
        // finish while the rename holds the fence. Once released, the bind does
        // real filesystem work, so this receive is just a hang guard, sized
        // for a loaded Windows runner.
        bind_done_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap()
            .unwrap();
        binder.join().unwrap();
    }

    #[test]
    fn superseded_snapshot_publication_is_a_noop() {
        let storage = tempdir().unwrap();
        let root_dir = tempdir().unwrap();
        let roots = StandingRoots::default();
        let mut cfg = config(
            storage.path(),
            vec![root(root_dir.path(), vec![IndexKind::Search])],
        );
        let mut initial = roots.reconcile(&cfg).unwrap();
        let entry = initial.active_entries.remove(0);
        let build = roots.admit_build(&entry.literal_path).unwrap();
        crate::root_cache::configure_artifact_access(
            &entry.resolved_target,
            &entry.artifact_key,
            false,
        );
        cfg.index.roots[0].indexes = vec![IndexKind::Semantic];
        let report = roots.reconcile(&cfg).unwrap();
        assert_eq!(report.replaced, vec![entry.literal_path.clone()]);
        let cache_dir = storage.path().join("index").join(&entry.artifact_key);
        let lease = crate::root_cache::WriterLease::acquire_shared(
            crate::root_cache::RootCacheDomain::Index,
            &cache_dir,
            &entry.artifact_key,
            &entry.resolved_target,
        )
        .unwrap()
        .unwrap();
        assert!(roots
            .publish_if_current(
                &entry.literal_path,
                build.publication,
                lease.as_ref(),
                || true,
                || true,
                || (),
            )
            .unwrap()
            .is_none());
    }
}
