pub use aft_base::cache_location::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::pinned_dir::{PinnedDir, RepairBudget};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::{self, File};
#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, Mutex, OnceLock, Weak};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;

#[cfg(test)]
#[path = "cache_location_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "gh_shim_speech/serving_tests.rs"]
mod speech_serving_tests;

#[cfg(test)]
mod prune_legacy_tests {
    use super::prune_legacy::{ASIDE_PREFIX, RENAME_REFUSED, STOP_AFTER_MOVE};
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    /// Every legacy entry the fixture plants, relative to the storage root.
    const LEGACY: [&str; 12] = [
        "views/view-1/derived.sqlite",
        "blobs/ab/cdef",
        "callgraph/key-1/build-breaker.sqlite",
        "index/key-1/cache.bin",
        "semantic/key-1/semantic.bin",
        "semantic/key-2/semantic.bin",
        "symbols/key-1/symbols.sqlite",
        "artifact-owners/owner.json",
        "migration/state.json",
        "semantic-batch-leases-v1/lease.json",
        "opencode/callgraph/key-1/graph.sqlite",
        "pi/callgraph/key-2/graph.sqlite",
    ];
    /// The folders a run must find for `LEGACY`, written out by hand so the
    /// expectation does not reuse the code's own list.
    const LEGACY_FOLDERS: [&str; 12] = [
        "artifact-owners",
        "blobs",
        "callgraph",
        "index",
        "migration",
        "opencode/callgraph",
        "pi/callgraph",
        "semantic-batch-leases-v1",
        "semantic/key-1",
        "semantic/key-2",
        "symbols",
        "views",
    ];
    /// What a run must leave byte-identical.
    const PROTECTED: [&str; 15] = [
        "aft.db",
        "aft.db-wal",
        "cache-keys.json",
        "data/store/main.sqlite",
        "inspect/key-1/inspect.sqlite",
        "retention/daemon-use.lock",
        "root-identities/key-1.json",
        "backups/session/stack",
        "opencode/inspect/key-1/inspect.sqlite",
        "opencode/sessions/one.json",
        "pi/settings.json",
        "semantic/models/models--Qdrant--all-MiniLM-L6-v2-onnx/model.onnx",
        "semantic/notes.txt",
        "unknown-folder/views/keep",
        "views.txt",
    ];

    struct Fixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        storage: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        // Canonical, so test hooks keyed by path match the pinned spelling
        // (macOS reaches temp folders through /var -> /private/var).
        let base = fs::canonicalize(temp.path()).unwrap();
        // On Windows canonical paths carry the verbatim `\\?\` prefix, and
        // the command reports and keys paths in their plain spelling.
        #[cfg(windows)]
        let base = crate::windows_path::normalize_windows_path(&base);
        let storage = base.join("storage");
        for relative in LEGACY.iter().chain(&PROTECTED) {
            let path = storage.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("contents of {relative}")).unwrap();
        }
        Fixture {
            _temp: temp,
            base,
            storage,
        }
    }

    /// Every file and folder under `root` with its bytes (folders map to
    /// nothing), read without following links.
    fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let kind = fs::symlink_metadata(&path).unwrap().file_type();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if kind.is_dir() {
                    pending.push(path);
                    found.insert(relative, None);
                } else if kind.is_symlink() {
                    found.insert(relative, Some(b"<link>".to_vec()));
                } else {
                    found.insert(relative, Some(fs::read(&path).unwrap()));
                }
            }
        }
        found
    }

    fn protected_bytes(storage: &Path) -> Vec<(String, Vec<u8>)> {
        PROTECTED
            .iter()
            .map(|relative| {
                (
                    relative.to_string(),
                    fs::read(storage.join(relative)).unwrap(),
                )
            })
            .collect()
    }

    fn legacy_folders(storage: &Path) -> Vec<PathBuf> {
        let mut paths = LEGACY_FOLDERS
            .iter()
            .map(|relative| storage.join(relative))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn run(storage: &Path, yes: bool) -> PruneLegacyReport {
        run_with_tiers(&[], storage, yes)
    }

    fn run_with_tiers(
        tiers: &[crate::config_resolve::ConfigTier],
        storage: &Path,
        yes: bool,
    ) -> PruneLegacyReport {
        match prune_legacy_stores(tiers, storage, yes, &mut |_| {}) {
            Ok(report) => report,
            Err(PruneLegacyError::Refused(reason)) => panic!("refused: {reason}"),
            Err(PruneLegacyError::Failed(error)) => panic!("failed: {error}"),
        }
    }

    fn found_paths(stores: &[LegacyStore]) -> Vec<PathBuf> {
        let mut paths = stores
            .iter()
            .map(|store| store.path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn asides(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(ASIDE_PREFIX))
            })
            .collect()
    }

    #[test]
    fn prune_legacy_dry_run_lists_exactly_the_legacy_stores_and_changes_nothing() {
        let fixture = fixture();
        let before = tree(&fixture.base);
        let report = run(&fixture.storage, false);
        assert_eq!(tree(&fixture.base), before, "a dry run changed the disk");
        assert_eq!(
            found_paths(&report.stores),
            legacy_folders(&fixture.storage)
        );
        assert!(report
            .stores
            .iter()
            .all(|store| store.complete && !store.leftover));
        let views = report
            .stores
            .iter()
            .find(|store| store.path.ends_with("views"))
            .unwrap();
        assert_eq!(
            views.bytes,
            "contents of views/view-1/derived.sqlite".len() as u64
        );
        assert!(report.removed.is_empty());
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert_eq!(report.roots, vec![fixture.storage.clone()]);
    }

    #[test]
    fn prune_legacy_yes_removes_the_stores_and_keeps_everything_else_byte_identical() {
        let fixture = fixture();
        let kept = protected_bytes(&fixture.storage);
        let mut events = Vec::new();
        let report = prune_legacy_stores(&[], &fixture.storage, true, &mut |event| {
            events.push(format!("{event:?}"));
        })
        .unwrap_or_else(|error| panic!("{error:?}"));
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(
            found_paths(&report.removed),
            legacy_folders(&fixture.storage)
        );
        for path in legacy_folders(&fixture.storage) {
            assert!(
                fs::symlink_metadata(&path).is_err(),
                "{} survived",
                path.display()
            );
        }
        assert_eq!(protected_bytes(&fixture.storage), kept);
        assert!(
            asides(&fixture.storage).is_empty(),
            "the aside folder stayed"
        );
        // The run lock is the only new file.
        assert!(fixture
            .storage
            .join("retention/prune-legacy.lock")
            .is_file());
        assert_eq!(
            events
                .iter()
                .filter(|event| event.starts_with("Removed"))
                .count(),
            legacy_folders(&fixture.storage).len()
        );
        // Harness folders keep everything except their call graph store.
        assert!(fixture.storage.join("opencode/inspect").is_dir());
        assert!(fixture.storage.join("unknown-folder/views/keep").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn prune_legacy_refuses_a_symlinked_store_and_never_follows_it() {
        let fixture = fixture();
        let outside = fixture.base.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), "keep me").unwrap();
        fs::remove_dir_all(fixture.storage.join("views")).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.storage.join("views")).unwrap();
        fs::remove_dir_all(fixture.storage.join("opencode/callgraph")).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.storage.join("opencode/callgraph")).unwrap();
        for yes in [false, true] {
            let report = run(&fixture.storage, yes);
            let skipped = report
                .skipped
                .iter()
                .map(|skip| (skip.path.clone(), skip.reason.clone()))
                .collect::<Vec<_>>();
            assert_eq!(skipped.len(), 2, "{skipped:?}");
            for (path, reason) in &skipped {
                assert!(
                    path.ends_with("views") || path.ends_with("opencode/callgraph"),
                    "{skipped:?}"
                );
                assert!(reason.contains("symbolic link"), "{reason}");
            }
            assert!(!found_paths(&report.stores).contains(&fixture.storage.join("views")));
        }
        assert_eq!(
            fs::read_to_string(outside.join("precious")).unwrap(),
            "keep me"
        );
        assert!(fs::symlink_metadata(fixture.storage.join("views"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn prune_legacy_refuses_a_store_on_another_device() {
        use crate::pinned_dir::REPAIR_IDENTITIES;
        let fixture = fixture();
        let blobs = fixture.storage.join("blobs");
        let (device, inode) = PinnedDir::open(&blobs)
            .unwrap()
            .directory_identity()
            .unwrap();
        REPAIR_IDENTITIES.with(|ids| {
            ids.borrow_mut()
                .insert(blobs.clone(), (device.wrapping_add(1), inode));
        });
        let dry = run(&fixture.storage, false);
        let report = run(&fixture.storage, true);
        REPAIR_IDENTITIES.with(|ids| ids.borrow_mut().clear());
        for report in [&dry, &report] {
            assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
            assert_eq!(report.skipped[0].path, blobs);
            assert!(
                report.skipped[0].reason.contains("another disk"),
                "{:?}",
                report.skipped
            );
        }
        assert_eq!(
            fs::read_to_string(blobs.join("ab/cdef")).unwrap(),
            "contents of blobs/ab/cdef"
        );
        assert!(!fixture.storage.join("views").exists());
    }

    #[test]
    fn prune_legacy_skips_and_reports_a_store_that_cannot_be_moved_aside() {
        let fixture = fixture();
        let kept = fs::read(fixture.storage.join("index/key-1/cache.bin")).unwrap();
        RENAME_REFUSED.with(|names| names.borrow_mut().push(OsString::from("index")));
        let mut skipped_events = 0;
        let report = prune_legacy_stores(&[], &fixture.storage, true, &mut |event| {
            if matches!(event, PruneLegacyEvent::Skipped(_)) {
                skipped_events += 1;
            }
        });
        RENAME_REFUSED.with(|names| names.borrow_mut().clear());
        let report = report.unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(skipped_events, 1);
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert_eq!(report.skipped[0].path, fixture.storage.join("index"));
        assert!(
            report.skipped[0]
                .reason
                .contains("could not be moved aside"),
            "{}",
            report.skipped[0].reason
        );
        assert_eq!(
            fs::read(fixture.storage.join("index/key-1/cache.bin")).unwrap(),
            kept
        );
        assert_eq!(
            report.removed.len(),
            legacy_folders(&fixture.storage).len() - 1
        );
        assert!(!fixture.storage.join("views").exists());
        assert!(asides(&fixture.storage).is_empty());
    }

    #[test]
    fn prune_legacy_rerun_removes_what_an_interrupted_run_moved_aside() {
        let fixture = fixture();
        let kept = protected_bytes(&fixture.storage);
        STOP_AFTER_MOVE.with(|stop| stop.set(true));
        let interrupted = prune_legacy_stores(&[], &fixture.storage, true, &mut |_| {});
        STOP_AFTER_MOVE.with(|stop| stop.set(false));
        interrupted.unwrap_or_else(|error| panic!("{error:?}"));
        let left = asides(&fixture.storage);
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left[0].join("0-artifact-owners/owner.json").is_file());
        for path in legacy_folders(&fixture.storage) {
            assert!(fs::symlink_metadata(&path).is_err(), "{}", path.display());
        }

        let dry = run(&fixture.storage, false);
        assert_eq!(found_paths(&dry.stores), left);
        assert!(dry.stores[0].leftover);
        assert!(dry.stores[0].bytes > 0);

        let report = run(&fixture.storage, true);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(found_paths(&report.removed), left);
        assert!(asides(&fixture.storage).is_empty());
        assert_eq!(protected_bytes(&fixture.storage), kept);
    }

    #[test]
    fn prune_legacy_refuses_a_folder_that_is_not_aft_storage() {
        let fixture = fixture();
        fs::remove_file(fixture.storage.join("aft.db")).unwrap();
        let before = tree(&fixture.base);
        for yes in [false, true] {
            match prune_legacy_stores(&[], &fixture.storage, yes, &mut |_| {}) {
                Err(PruneLegacyError::Refused(reason)) => {
                    assert!(reason.contains("does not look like"), "{reason}")
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
        assert_eq!(tree(&fixture.base), before);
    }

    #[test]
    fn prune_legacy_reports_a_missing_storage_folder_as_nothing_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let report = run(&temp.path().join("absent"), true);
        assert!(report.roots.is_empty());
        assert!(!temp.path().join("absent").exists());
    }

    #[test]
    fn prune_legacy_refuses_while_another_run_holds_the_lock() {
        let fixture = fixture();
        let retention = fixture.storage.join("retention");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(retention.join("prune-legacy.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        match prune_legacy_stores(&[], &fixture.storage, true, &mut |_| {}) {
            Err(PruneLegacyError::Refused(reason)) => {
                assert!(reason.contains("Another cleanup"), "{reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(fixture
            .storage
            .join("views/view-1/derived.sqlite")
            .is_file());
    }

    #[test]
    fn prune_legacy_also_cleans_the_enrolled_external_cache_root() {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        // Plain spelling on Windows, as the command reports it.
        #[cfg(windows)]
        let base = crate::windows_path::normalize_windows_path(&base);
        let storage = base.join("storage");
        let parent = base.join("cache-volume");
        fs::create_dir(&parent).unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.clone()),
            cache_dir: Some(parent.clone()),
            ..Default::default()
        };
        let warning = crate::reader_floor::prepare_cache_root_for_test(&mut config);
        assert!(warning.is_none(), "{warning:?}");
        drop(config);
        fs::write(storage.join("aft.db"), "db").unwrap();
        let cache = parent.join("aft");
        for (relative, contents) in [
            ("views/a", "old"),
            ("index/key-1/cache.bin", "old"),
            ("semantic/key-1/semantic.bin", "old"),
            ("semantic/models/model.onnx", "model"),
            ("data/keep", "new"),
        ] {
            let path = cache.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "test".into(),
            doc: serde_json::json!({"storage": {"cache_dir": parent}}).to_string(),
        }];
        let dry = run_with_tiers(&tiers, &storage, false);
        assert_eq!(dry.roots, vec![storage.clone(), cache.clone()]);
        // Only the cache root's part: preparing the cache root above may
        // leave its own folders under the storage root.
        let in_cache = |stores: &[LegacyStore]| {
            found_paths(stores)
                .into_iter()
                .filter(|path| path.starts_with(&cache))
                .collect::<Vec<_>>()
        };
        let expected = ["index", "semantic/key-1", "views"].map(|name| cache.join(name));
        assert_eq!(in_cache(&dry.stores), expected);
        let report = run_with_tiers(&tiers, &storage, true);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(in_cache(&report.removed), expected);
        for path in expected {
            assert!(!path.exists(), "{}", path.display());
        }
        assert_eq!(fs::read_to_string(cache.join("data/keep")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(cache.join("semantic/models/model.onnx")).unwrap(),
            "model"
        );
        assert!(cache.join(OWNER).is_file());
        assert!(asides(&cache).is_empty());
    }

    #[test]
    fn prune_legacy_skips_an_external_cache_root_it_cannot_prove() {
        let fixture = fixture();
        let parent = fixture.base.join("cache-volume");
        fs::create_dir_all(parent.join("aft/views")).unwrap();
        fs::write(parent.join("aft/views/a"), "someone else's").unwrap();
        let tiers = [crate::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "test".into(),
            doc: serde_json::json!({"storage": {"cache_dir": parent}}).to_string(),
        }];
        let report = run_with_tiers(&tiers, &fixture.storage, true);
        assert_eq!(report.roots, vec![fixture.storage.clone()]);
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert_eq!(report.skipped[0].path, parent.join("aft"));
        assert!(
            report.skipped[0].reason.contains("not proven"),
            "{}",
            report.skipped[0].reason
        );
        assert!(parent.join("aft/views/a").is_file());
    }
}

#[cfg(test)]
mod markers;
