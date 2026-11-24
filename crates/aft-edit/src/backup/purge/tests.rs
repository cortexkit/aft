use super::*;
use crate::db::backups::BackupStackSummary;
use crate::harness::Harness;
use std::fs;

const S1: &str = "purge-session-one";
const S2: &str = "purge-session-two";

struct Fixture {
    project: tempfile::TempDir,
    storage: tempfile::TempDir,
    db: Arc<Mutex<TrackedConnection>>,
}

impl Fixture {
    fn new() -> Self {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let db = Arc::new(Mutex::new(
            crate::db::open(&storage.path().join("aft.db")).unwrap(),
        ));
        Self {
            project,
            storage,
            db,
        }
    }

    /// A store configured the way a bound daemon root configures its own.
    fn store(&self) -> BackupStore {
        let mut store = BackupStore::new();
        store.set_storage_dir_for_harness(self.storage.path().to_path_buf(), Harness::Opencode, 72);
        store.set_db_harness(Harness::Opencode);
        store.set_db_project_key("purge-test-project".to_string());
        store.set_db_pool(Arc::clone(&self.db));
        store
    }

    fn file(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.project.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn request(&self, path: Option<&Path>, session: Option<&str>, dry_run: bool) -> PurgeRequest {
        PurgeRequest {
            storage_dir: self.storage.path().to_path_buf(),
            path: path.map(Path::to_path_buf),
            session: session.map(str::to_string),
            harness: None,
            dry_run,
        }
    }

    fn db_stacks(&self) -> Vec<BackupStackSummary> {
        crate::db::backups::list_backup_stacks(&self.db.lock().unwrap(), None, None).unwrap()
    }

    fn stack_dir(&self, session: &str, path: &Path) -> PathBuf {
        self.storage
            .path()
            .join("opencode")
            .join("backups")
            .join(hash_session(session))
            .join(BackupStore::path_hash(&backup_key_for_path(path)))
    }
}

/// Snapshot `path`, then change it so the next snapshot captures new content.
fn edit(store: &mut BackupStore, session: &str, path: &Path, next: &str) {
    store.snapshot(session, path, "edit").unwrap();
    fs::write(path, next).unwrap();
}

fn key(path: &Path) -> String {
    backup_key_for_path(path).display().to_string()
}

#[cfg(unix)]
#[test]
fn final_directory_symlink_purge_selects_only_the_link_entry() {
    let fx = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let target_file = outside.path().join("secret.txt");
    fs::write(&target_file, "secret").unwrap();
    let link = fx.project.path().join("link");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let live = parking_lot::Mutex::new(fx.store());
    live.lock().snapshot(S1, &target_file, "edit").unwrap();
    live.lock().snapshot(S1, &link, "delete").unwrap();

    let selected = session_path_keys(&live, S1, &link, |_| Some(Arc::clone(&fx.db)));
    assert_eq!(selected, vec![backup_key_for_path(&link)]);
    let target = PurgeTarget::new(&link);
    assert_eq!(
        target.classify(&link.join("secret.txt"), false),
        SavedPathMatch::Unrelated
    );
    assert_eq!(
        target.classify(&target_file, false),
        SavedPathMatch::Unrelated
    );

    let report = purge_backups(
        &fx.request(Some(&link), Some(S1), false),
        Some(Arc::clone(&fx.db)),
        &[&live],
    );
    assert!(report.is_complete(), "{:?}", report.failures);
    assert_eq!(report.removed.stacks, 1);
    assert_eq!(live.lock().history(S1, &target_file).len(), 1);
    assert!(live.lock().history(S1, &link).is_empty());
}

#[test]
fn dry_run_reports_counts_and_removes_nothing() {
    let fx = Fixture::new();
    let file = fx.file("a/b/x.txt", "v0");
    let live = parking_lot::Mutex::new(fx.store());
    edit(&mut live.lock(), S1, &file, "v1");
    edit(&mut live.lock(), S1, &file, "v2");

    let dir = fx.project.path().join("a");
    let report = purge_backups(
        &fx.request(Some(&dir), None, true),
        Some(Arc::clone(&fx.db)),
        &[&live],
    );

    assert!(report.dry_run);
    assert_eq!(report.matched.stacks, 1);
    assert_eq!(report.matched.entries, 2);
    // Two content files, the stack's meta.json, and its post-state.json
    // sidecar (unsynced post-edit fingerprints, purged with the stack).
    assert_eq!(report.matched.files, 4);
    assert!(report.matched.bytes > 0);
    assert_eq!(report.matched.db_rows, 2);
    assert_eq!(report.matched.sessions, vec![S1.to_string()]);
    assert_eq!(report.samples, vec![key(&file)]);
    assert_eq!(report.removed, PurgeTotals::default());
    assert_eq!(report.examined.namespaces, vec!["opencode".to_string()]);
    assert_eq!(report.examined.disk_stacks, 1);
    assert_eq!(report.examined.db_stacks, 1);
    assert!(report.is_complete());

    assert_eq!(live.lock().history(S1, &file).len(), 2);
    assert!(fx.stack_dir(S1, &file).join("meta.json").is_file());
    assert_eq!(fx.db_stacks().len(), 1);
}

#[test]
fn directory_path_purges_exactly_the_entries_under_it_in_every_session() {
    let fx = Fixture::new();
    let inside = fx.file("a/b/x.txt", "x0");
    let nested = fx.file("a/b/deep/z.txt", "z0");
    // Shares the string prefix `a/b` but is a different directory.
    let sibling = fx.file("a/bc/y.txt", "y0");
    let live = parking_lot::Mutex::new(fx.store());
    edit(&mut live.lock(), S1, &inside, "x1");
    edit(&mut live.lock(), S2, &inside, "x2");
    edit(&mut live.lock(), S1, &nested, "z1");
    edit(&mut live.lock(), S1, &sibling, "y1");

    let dir = fx.project.path().join("a/b");
    let report = purge_backups(
        &fx.request(Some(&dir), None, false),
        Some(Arc::clone(&fx.db)),
        &[&live],
    );

    assert!(report.is_complete(), "{:?}", report.failures);
    assert_eq!(report.removed.stacks, 3);
    assert_eq!(
        report.removed.sessions,
        vec![S1.to_string(), S2.to_string()]
    );
    let mut store = live.lock();
    assert!(store.history(S1, &inside).is_empty());
    assert!(store.history(S2, &inside).is_empty());
    assert!(store.history(S1, &nested).is_empty());
    assert_eq!(store.history(S1, &sibling).len(), 1);
    assert_eq!(
        store.restore_latest(S2, &inside).unwrap_err().code(),
        "no_undo_history"
    );
    let remaining = fx.db_stacks();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].file_path, key(&sibling));
    assert!(fx.stack_dir(S1, &sibling).join("meta.json").is_file());
}

#[test]
fn session_narrows_a_path_purge() {
    let fx = Fixture::new();
    let inside = fx.file("a/b/x.txt", "x0");
    let live = parking_lot::Mutex::new(fx.store());
    edit(&mut live.lock(), S1, &inside, "x1");
    edit(&mut live.lock(), S2, &inside, "x2");

    let dir = fx.project.path().join("a/b");
    let report = purge_backups(
        &fx.request(Some(&dir), Some(S1), false),
        Some(Arc::clone(&fx.db)),
        &[&live],
    );

    assert_eq!(report.removed.stacks, 1);
    assert_eq!(report.removed.sessions, vec![S1.to_string()]);
    let store = live.lock();
    assert!(store.history(S1, &inside).is_empty());
    assert_eq!(store.history(S2, &inside).len(), 1);
    let remaining = fx.db_stacks();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session_id, S2);
}

#[test]
fn session_filter_purges_only_that_session() {
    let fx = Fixture::new();
    let first = fx.file("one.txt", "1");
    let second = fx.file("elsewhere/two.txt", "2");
    let live = parking_lot::Mutex::new(fx.store());
    edit(&mut live.lock(), S1, &first, "1a");
    edit(&mut live.lock(), S1, &second, "2a");
    edit(&mut live.lock(), S2, &first, "1b");

    let report = purge_backups(
        &fx.request(None, Some(S1), false),
        Some(Arc::clone(&fx.db)),
        &[&live],
    );

    assert_eq!(report.removed.stacks, 2);
    assert_eq!(report.removed.sessions, vec![S1.to_string()]);
    let store = live.lock();
    assert!(store.history(S1, &first).is_empty());
    assert!(store.history(S1, &second).is_empty());
    assert_eq!(store.history(S2, &first).len(), 1);
    assert!(fx.db_stacks().iter().all(|stack| stack.session_id == S2));
}

#[test]
fn purge_leaves_no_files_rows_or_cached_history() {
    let fx = Fixture::new();
    let file = fx.file("gone/x.txt", "v0");
    let live = parking_lot::Mutex::new(fx.store());
    {
        let mut store = live.lock();
        store
            .snapshot_with_op(S1, &file, "edit", Some("op-1"))
            .unwrap();
        fs::write(&file, "v1").unwrap();
        // The stack is cached in memory, which is what the purge must clear.
        assert_eq!(
            store.in_memory_entry_count(S1, &backup_key_for_path(&file)),
            1
        );
    }

    let report = purge_backups(
        &fx.request(Some(&file), None, false),
        Some(Arc::clone(&fx.db)),
        &[&live],
    );
    assert_eq!(report.removed.stacks, 1);
    assert_eq!(report.live_stores, 1);

    let mut store = live.lock();
    assert_eq!(
        store.in_memory_entry_count(S1, &backup_key_for_path(&file)),
        0
    );
    assert!(store.in_memory_stack_keys().is_empty());
    assert!(store.history(S1, &file).is_empty());
    assert_eq!(
        store.preview_latest_path(S1, &file).unwrap_err().code(),
        "no_undo_history"
    );
    assert_eq!(
        store.restore_latest(S1, &file).unwrap_err().code(),
        "no_undo_history"
    );
    assert_eq!(
        store.restore_last_operation(S1).unwrap_err().code(),
        "no_undo_history"
    );
    assert!(!fx.stack_dir(S1, &file).exists());
    assert!(fx.db_stacks().is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
}

#[test]
fn rows_left_by_a_hand_deleted_stack_are_purged() {
    let fx = Fixture::new();
    let file = fx.file("hand/x.txt", "v0");
    let mut writer = fx.store();
    edit(&mut writer, S1, &file, "v1");
    fs::remove_dir_all(fx.stack_dir(S1, &file)).unwrap();

    // The stack's rows outlive its hand-deleted files.
    assert_eq!(fx.db_stacks().len(), 1);

    let report = purge_backups(
        &fx.request(Some(&file), None, false),
        Some(Arc::clone(&fx.db)),
        &[],
    );
    assert_eq!(report.removed.stacks, 1);
    assert_eq!(report.removed.db_rows, 1);
    assert!(fx.db_stacks().is_empty());

    let mut after = fx.store();
    assert_eq!(
        after.restore_latest(S1, &file).unwrap_err().code(),
        "no_undo_history"
    );
}

#[test]
fn another_process_undo_reports_no_history_after_an_offline_purge() {
    let fx = Fixture::new();
    let file = fx.file("other/x.txt", "v0");
    // Stands in for a process the purge cannot reach: it keeps its cache.
    let mut other = fx.store();
    edit(&mut other, S1, &file, "v1");

    let report = purge_backups(
        &fx.request(Some(&file), None, false),
        Some(Arc::clone(&fx.db)),
        &[],
    );
    assert_eq!(report.removed.stacks, 1);

    assert_eq!(
        other.restore_latest(S1, &file).unwrap_err().code(),
        "no_undo_history"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "v1");
}

#[test]
fn owner_refuses_storage_it_does_not_use() {
    let fx = Fixture::new();
    let live = parking_lot::Mutex::new(fx.store());
    let elsewhere = tempfile::tempdir().unwrap();
    let params = serde_json::json!({
        "storage_dir": elsewhere.path(),
        "session": S1,
        "dry_run": true,
    });
    let refusal = purge_as_owner(&params, &[&live], |_| None).unwrap_err();
    assert_eq!(refusal.code, "storage_not_owned");

    let params = serde_json::json!({
        "storage_dir": fx.storage.path(),
        "session": S1,
    });
    let report = purge_as_owner(&params, &[&live], |_| Some(Arc::clone(&fx.db))).unwrap();
    assert!(report.dry_run, "a request without dry_run must only report");
}

#[test]
fn requests_without_a_filter_or_with_relative_paths_are_rejected() {
    let storage = tempfile::tempdir().unwrap();
    let missing_filter = serde_json::json!({ "storage_dir": storage.path() });
    assert!(PurgeRequest::from_params(&missing_filter).is_err());
    let relative = serde_json::json!({ "storage_dir": storage.path(), "path": "src" });
    assert!(PurgeRequest::from_params(&relative).is_err());
    let bad_harness =
        serde_json::json!({ "storage_dir": storage.path(), "session": "s", "harness": "../x" });
    assert!(PurgeRequest::from_params(&bad_harness).is_err());
}

#[test]
fn concurrent_append_during_purge_keeps_the_stack_consistent() {
    let fx = Fixture::new();
    let file = fx.file("race/x.txt", "c0");
    let live = parking_lot::Mutex::new(fx.store());
    edit(&mut live.lock(), S1, &file, "c1");

    let writer_file = file.clone();
    let mut writer = fx.store();
    let appender = std::thread::spawn(move || {
        for round in 0..25 {
            writer.snapshot(S1, &writer_file, "race edit").unwrap();
            fs::write(&writer_file, format!("c{}", round + 2)).unwrap();
        }
        writer
    });
    for _ in 0..8 {
        let report = purge_backups(
            &fx.request(Some(&file), None, false),
            Some(Arc::clone(&fx.db)),
            &[&live],
        );
        assert!(report.is_complete(), "{:?}", report.failures);
    }
    let mut writer = appender.join().unwrap();

    // Whatever interleaving happened, disk and rows describe the same stack
    // and every entry's content file exists.
    let fresh = fx.store();
    let key = backup_key_for_path(&file);
    let disk = fresh
        .read_stack_from_disk_unlocked(S1, &key)
        .expect("stack readable")
        .unwrap_or_default();
    let rows = crate::db::backups::count_backups_for_path(
        &fx.db.lock().unwrap(),
        "opencode",
        S1,
        &BackupStore::path_hash(&key),
    )
    .unwrap();
    assert_eq!(disk.len(), rows);
    for store in [&mut writer, &mut *live.lock()] {
        match store.restore_latest(S1, &file) {
            Ok(_) => {}
            Err(error) => assert_eq!(error.code(), "no_undo_history", "{error}"),
        }
    }
}

#[test]
fn session_path_purge_removes_only_the_calling_sessions_stacks() {
    let fx = Fixture::new();
    let secret = fx.file("conf/secret.env", "token=0");
    let nearby = fx.file("conf/other.txt", "o0");
    let live = parking_lot::Mutex::new(fx.store());
    edit(&mut live.lock(), S1, &secret, "token=1");
    edit(&mut live.lock(), S1, &secret, "token=2");
    edit(&mut live.lock(), S1, &nearby, "o1");
    edit(&mut live.lock(), S2, &secret, "token=3");

    let report = purge_session_path(&live, S1, &secret, |_| None).unwrap();

    assert!(report.is_complete(), "{:?}", report.failures);
    assert_eq!(report.removed.stacks, 1);
    assert_eq!(report.removed.entries, 2);
    assert_eq!(report.removed.sessions, vec![S1.to_string()]);
    let mut store = live.lock();
    assert!(store.history(S1, &secret).is_empty());
    assert_eq!(
        store.restore_latest(S1, &secret).unwrap_err().code(),
        "no_undo_history"
    );
    assert_eq!(store.history(S1, &nearby).len(), 1);
    assert_eq!(store.history(S2, &secret).len(), 1);
    assert!(!fx.stack_dir(S1, &secret).exists());
    assert!(fx.stack_dir(S2, &secret).join("meta.json").is_file());
    let mut remaining = fx
        .db_stacks()
        .into_iter()
        .map(|stack| (stack.session_id, stack.file_path))
        .collect::<Vec<_>>();
    remaining.sort();
    assert_eq!(
        remaining,
        vec![
            (S1.to_string(), key(&nearby)),
            (S2.to_string(), key(&secret)),
        ]
    );
}

#[test]
fn session_path_purge_of_a_store_without_storage_forgets_memory_stacks() {
    let fx = Fixture::new();
    let inside = fx.file("a/x.txt", "x0");
    let outside = fx.file("b/y.txt", "y0");
    let live = parking_lot::Mutex::new(BackupStore::new());
    edit(&mut live.lock(), S1, &inside, "x1");
    edit(&mut live.lock(), S1, &outside, "y1");
    edit(&mut live.lock(), S2, &inside, "x2");

    let report = purge_session_path(&live, S1, &fx.project.path().join("a"), |_| None).unwrap();

    assert_eq!(report.removed.stacks, 1);
    assert_eq!(report.removed.entries, 1);
    let store = live.lock();
    assert!(store.history(S1, &inside).is_empty());
    assert_eq!(store.history(S1, &outside).len(), 1);
    assert_eq!(store.history(S2, &inside).len(), 1);
}
