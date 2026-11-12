//! A status push's tracked-file count must equal `tracked_files().len()` in
//! every state the store passes through, while re-reading only `meta.json`
//! files that changed since the previous push.

use super::*;
use std::fs;

const SESSION: &str = "status-inventory";

fn store_with_files(count: usize) -> (BackupStore, tempfile::TempDir, Vec<PathBuf>) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir(&project).unwrap();
    let paths = (0..count)
        .map(|index| {
            let path = project.join(format!("file_{index}.txt"));
            fs::write(&path, format!("original {index}")).unwrap();
            path
        })
        .collect();
    let mut store = BackupStore::new();
    store.set_storage_dir(temp.path().join("storage"), 72);
    (store, temp, paths)
}

fn meta_path(store: &BackupStore, session: &str, path: &Path) -> PathBuf {
    store
        .session_dir(session)
        .unwrap()
        .join(BackupStore::path_hash(&canonicalize_key(path)))
        .join("meta.json")
}

/// Assert that the push count equals today's count both with the store's
/// resident stacks and after they are released, and return the count.
fn assert_matches_tracked_files(store: &mut BackupStore, session: &str, step: &str) -> usize {
    let expected = store.tracked_files(session).len();
    let pushed = store.status_inventory(session).tracked_file_count();
    assert_eq!(pushed, expected, "resident count differs after {step}");
    // A second push serves unchanged metadata from the cache.
    let pushed = store.status_inventory(session).tracked_file_count();
    assert_eq!(pushed, expected, "cached count differs after {step}");
    store.release_session_memory(session);
    let released = store.tracked_files(session).len();
    let pushed = store.status_inventory(session).tracked_file_count();
    assert_eq!(pushed, released, "released count differs after {step}");
    expected
}

#[test]
fn status_inventory_count_matches_tracked_files_through_store_lifecycle() {
    let (mut store, _temp, paths) = store_with_files(5);

    for path in &paths[..3] {
        store.snapshot(SESSION, path, "create").unwrap();
    }
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "create"),
        3
    );

    store.restore_latest(SESSION, &paths[0]).unwrap();
    assert_eq!(assert_matches_tracked_files(&mut store, SESSION, "undo"), 2);
    // The cache keeps only stacks the latest pass reached.
    assert_eq!(store.status_meta_caches.cached_stacks(SESSION), 2);

    store
        .snapshot_with_op(SESSION, &paths[3], "rolled back", Some("bad"))
        .unwrap();
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "operation snapshot"),
        3
    );
    store.discard_operation_entries(SESSION, "bad");
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "discard"),
        2
    );

    // Two entries on one stack, then pruning to depth one rewrites its
    // metadata; pruning to depth zero leaves empty metadata behind, which the
    // directory pass excludes while the store still lists the key it indexed.
    store.snapshot(SESSION, &paths[1], "second entry").unwrap();
    store.set_policy(BackupPolicy {
        max_depth: 1,
        ..store.policy()
    });
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "policy pruning"),
        2
    );
    store
        .snapshot(SESSION, &paths[4], "before empty pruning")
        .unwrap();
    store.set_policy(BackupPolicy {
        max_depth: 0,
        ..store.policy()
    });
    assert_matches_tracked_files(&mut store, SESSION, "pruning to depth zero");
    store.set_policy(BackupPolicy {
        max_depth: DEFAULT_MAX_UNDO_DEPTH,
        ..store.policy()
    });

    for path in &paths {
        store.snapshot(SESSION, path, "refill").unwrap();
    }
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "refill"),
        5
    );

    let shared = parking_lot::Mutex::new(store);
    purge::purge_session_path(&shared, SESSION, &paths[2], |_| None).unwrap();
    let mut store = shared.into_inner();
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "purge"),
        4
    );

    // A corrupt metadata file fails discovery, and both counts fall back to the
    // resident keys.
    fs::write(meta_path(&store, SESSION, &paths[1]), b"{ not json").unwrap();
    store.snapshot(SESSION, &paths[0], "resident").unwrap();
    assert_eq!(store.tracked_files(SESSION).len(), 1);
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "corrupt metadata"),
        1
    );
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 0);
}

#[test]
fn status_inventory_count_matches_tracked_files_after_import() {
    use crate::agent_data::{ExpectedState, FileRecord, SnapshotGroup};
    let temp = tempfile::tempdir().unwrap();
    let paths = ["native.txt", "imported.txt"].map(|name| {
        let path = temp.path().join(name);
        fs::write(&path, name).unwrap();
        path
    });
    // Imported history raises the storage root's reader floor, which needs a
    // prepared root, as in the portable-import format tests.
    let storage = temp.path().join("store");
    create_private_dir_all(&storage).unwrap();
    assert!(crate::reader_floor::prepare(&storage).is_empty());
    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage, crate::harness::Harness::Pi, 0);
    store.snapshot(SESSION, &paths[0], "native").unwrap();
    let key = crate::agent_data::registry::PortablePath {
        project_id: "fixture-project".into(),
        root_key: cortexkit_project_identity::RootKey::new(
            cortexkit_project_identity::RootKeyKind::Label,
            "fixture".into(),
        ),
        relative_path: "imported.txt".into(),
    };
    let group = SnapshotGroup::Undo {
        id: "imported-operation".into(),
        files: vec![FileRecord {
            path: key,
            id: "imported-entry".into(),
            order: 1,
            timestamp: 1,
            mode: None,
            before: Some(b"before".to_vec()),
            expected: ExpectedState::capture(&paths[1]).unwrap(),
        }],
    };
    store
        .install_portable_undo(SESSION, &[(group, vec![paths[1].clone()])])
        .unwrap();
    // Import replaces the resident map with the imported stacks, while the
    // native stack stays on disk.
    assert_eq!(
        assert_matches_tracked_files(&mut store, SESSION, "import"),
        2
    );
}

#[test]
fn status_inventory_rereads_metadata_replaced_with_same_size_and_mtime() {
    let (mut store, _temp, paths) = store_with_files(1);
    store.snapshot(SESSION, &paths[0], "create").unwrap();
    store.release_session_memory(SESSION);
    let parses = store.status_meta_parses_for_test();
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 1);
    assert_eq!(store.status_meta_parses_for_test(), parses + 1);
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 1);
    assert_eq!(
        store.status_meta_parses_for_test(),
        parses + 1,
        "unchanged metadata must come from the cache"
    );

    // Publish metadata naming a different path of the same length, exactly as
    // writers do (a temporary file renamed over the old one), with the old
    // size and modification time. The new path does not hash to the stack's
    // directory name, so the stack no longer counts.
    let meta = meta_path(&store, SESSION, &paths[0]);
    let before = fs::metadata(&meta).unwrap();
    let original = paths[0].display().to_string();
    let renamed = format!("{}x", &original[..original.len() - 1]);
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&meta).unwrap()).unwrap();
    assert_eq!(value["path"], original.as_str());
    value["path"] = renamed.into();
    let content = serde_json::to_string_pretty(&value).unwrap();
    assert_eq!(content.len() as u64, before.len());
    let temp = meta.with_file_name("meta.json.tmp");
    fs::write(&temp, &content).unwrap();
    filetime::set_file_mtime(
        &temp,
        filetime::FileTime::from_last_modification_time(&before),
    )
    .unwrap();
    fs::rename(&temp, &meta).unwrap();
    let after = fs::metadata(&meta).unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());

    assert_eq!(store.tracked_files(SESSION).len(), 0);
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 0);
    assert_eq!(store.status_meta_parses_for_test(), parses + 2);
}

#[test]
fn status_inventory_regates_cached_metadata_after_a_reader_floor_refusal() {
    use crate::persisted_format::{self, PersistedStore, UnsupportedPersistedFormat};
    let (mut store, temp, paths) = store_with_files(2);
    for path in &paths {
        store.snapshot(SESSION, path, "create").unwrap();
    }
    store.release_session_memory(SESSION);
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 2);
    let parses = store.status_meta_parses_for_test();

    // A newer build raised this storage root's reader floor for backup
    // metadata. Discovery now fails, and both counts fall back to the
    // (released, so empty) resident keys even though no file changed.
    let storage = temp.path().join("storage");
    let refusal = UnsupportedPersistedFormat::floor(
        PersistedStore::BackupMeta,
        storage.join("reader-floor.json"),
        999,
    );
    persisted_format::record(&refusal, &storage);
    let refused = (
        store.tracked_files(SESSION).len(),
        store.status_inventory(SESSION).tracked_file_count(),
    );
    persisted_format::replace_floor_refusals(&storage, &[]);
    assert_eq!(refused, (0, 0));
    assert_eq!(store.status_meta_parses_for_test(), parses);
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 2);
}

#[test]
fn status_inventory_cache_is_freed_with_session_memory() {
    let (mut store, _temp, paths) = store_with_files(2);
    for path in &paths {
        store.snapshot(SESSION, path, "create").unwrap();
    }
    assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 2);
    assert_eq!(store.status_meta_caches.cached_stacks(SESSION), 2);

    // A pass that started before the release must not put its cache back.
    let inventory = store.status_inventory(SESSION);
    store.release_session_memory(SESSION);
    assert_eq!(store.status_meta_caches.cached_stacks(SESSION), 0);
    let caches = Arc::clone(&store.status_meta_caches);
    let (epoch, cache) = caches.take(SESSION, inventory.session_dir.as_deref().unwrap());
    store.release_memory();
    caches.put_back(SESSION, epoch, cache);
    assert_eq!(caches.cached_stacks(SESSION), 0);
    assert_eq!(inventory.tracked_file_count(), 2);
}

/// Splits the warm status-inventory pass into its per-stack checks, to show
/// which one dominates the push cost.
#[test]
#[ignore = "manual status inventory component cost probe"]
fn status_inventory_component_cost_probe() {
    use crate::run_tool_call::ThreadCpuSample;
    let (mut store, _temp, paths) = store_with_files(256);
    for path in &paths {
        store.snapshot(SESSION, path, "probe").unwrap();
    }
    store.release_session_memory(SESSION);
    let metas = paths
        .iter()
        .map(|path| meta_path(&store, SESSION, path))
        .collect::<Vec<_>>();
    let version = Some(u64::from(ORDINARY_SCHEMA_VERSION));
    let session_dir = store.session_dir(SESSION).unwrap();
    let measure = |label: &str, f: &dyn Fn()| {
        let start = ThreadCpuSample::now();
        for _ in 0..50 {
            f();
        }
        let cpu = ThreadCpuSample::elapsed(start, ThreadCpuSample::now()).unwrap();
        eprintln!("{label}: 256 stacks, CPU/pass={:?}", cpu / 50);
    };
    measure("read_dir only", &|| {
        std::hint::black_box(fs::read_dir(&session_dir).unwrap().count());
    });
    measure("file_identity::metadata", &|| {
        for meta in &metas {
            std::hint::black_box(crate::file_identity::metadata(meta).unwrap());
        }
    });
    measure("refuse_live_history_file", &|| {
        for meta in &metas {
            refuse_live_history_file(meta).unwrap();
        }
    });
    measure("check_backup_meta_version", &|| {
        for meta in &metas {
            check_backup_meta_version(meta, version).unwrap();
        }
    });
    measure("is_loadable_backup_path", &|| {
        for (path, meta) in paths.iter().zip(&metas) {
            assert!(is_loadable_backup_path(
                &canonicalize_key(path),
                meta.parent().unwrap()
            ));
        }
    });
    store.status_inventory(SESSION).tracked_file_count();
    measure("whole warm pass", &|| {
        assert_eq!(store.status_inventory(SESSION).tracked_file_count(), 256);
    });
}
