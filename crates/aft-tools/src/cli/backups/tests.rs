use super::*;
use aft::backup::purge::purge_as_owner;
use aft::backup::BackupStore;
use aft::db::TrackedConnection;
use aft::harness::Harness;
use std::fs;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};

const SESSION: &str = "cli-purge-session";

/// Tempdir fixtures live under the system temp directory, which production
/// builds of the backup store refuse to snapshot. Debug builds honor this
/// opt-in; the library's own tests enable it through a test-only switch that a
/// binary test cannot reach.
fn allow_temp_backups() {
    std::env::set_var("AFT_TEST_ALLOW_TEMP_BACKUPS", "1");
}

fn store_for(storage: &Path, db: &Arc<Mutex<TrackedConnection>>) -> BackupStore {
    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage.to_path_buf(), Harness::Opencode, 72);
    store.set_db_harness(Harness::Opencode);
    store.set_db_project_key("cli-purge-project".to_string());
    store.set_db_pool(Arc::clone(db));
    store
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

struct NoOwner;

impl StoreOwner for NoOwner {
    fn purge(&self, _request: &PurgeRequest) -> OwnerReply {
        OwnerReply::NotOwned("no owner in this test".to_string())
    }
}

struct FailingOwner;

impl StoreOwner for FailingOwner {
    fn purge(&self, _request: &PurgeRequest) -> OwnerReply {
        OwnerReply::Failed("simulated transport failure".to_string())
    }
}

type OwnerCall = (serde_json::Value, mpsc::Sender<OwnerReply>);

/// A backup store owned by another thread, answering purge requests the way
/// the daemon's management handler does: through `purge_as_owner` with its
/// own store and its own database handle.
struct ThreadOwner {
    requests: mpsc::Sender<OwnerCall>,
}

impl StoreOwner for ThreadOwner {
    fn purge(&self, request: &PurgeRequest) -> OwnerReply {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.requests
            .send((serde_json::to_value(request).unwrap(), reply_tx))
            .unwrap();
        reply_rx.recv().unwrap()
    }
}

fn spawn_owner(
    store: Arc<parking_lot::Mutex<BackupStore>>,
    db: Arc<Mutex<TrackedConnection>>,
) -> (ThreadOwner, std::thread::JoinHandle<()>) {
    let (requests, incoming) = mpsc::channel::<OwnerCall>();
    let handle = std::thread::spawn(move || {
        for (params, reply) in incoming {
            let result = purge_as_owner(&params, &[&*store], |_| Some(Arc::clone(&db)));
            let _ = reply.send(match result {
                Ok(report) => OwnerReply::Report(Box::new(report)),
                Err(refusal) => {
                    OwnerReply::Failed(format!("{}: {}", refusal.code, refusal.message))
                }
            });
        }
    });
    (ThreadOwner { requests }, handle)
}

#[test]
fn purge_goes_through_the_live_owner_and_its_next_undo_finds_no_history() {
    allow_temp_backups();
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let db = Arc::new(Mutex::new(
        aft::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    let file = project.path().join("tree/data.txt");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "v0").unwrap();

    let store = Arc::new(parking_lot::Mutex::new(store_for(storage.path(), &db)));
    {
        let mut owned = store.lock();
        assert!(owned
            .snapshot(SESSION, &file, "edit one")
            .unwrap()
            .is_some());
        fs::write(&file, "v1").unwrap();
        owned.snapshot(SESSION, &file, "edit two").unwrap();
        fs::write(&file, "v2").unwrap();
        assert_eq!(owned.history(SESSION, &file).len(), 2);
    }
    let (owner, handle) = spawn_owner(Arc::clone(&store), Arc::clone(&db));

    let mut output = Vec::new();
    let tree = project.path().join("tree");
    run_with(
        args(&["purge", "--path", tree.to_str().unwrap(), "--yes"]),
        storage.path().to_path_buf(),
        project.path().to_path_buf(),
        &owner,
        &mut output,
    )
    .unwrap();
    drop(owner);
    handle.join().unwrap();
    let output = String::from_utf8(output).unwrap();

    // The owner kept the stack cached; after the purge its history must be
    // empty and undo must report no history rather than touch missing files.
    let mut owned = store.lock();
    assert!(
        owned.history(SESSION, &file).is_empty(),
        "owner still lists purged history; purge output:\n{output}"
    );
    assert_eq!(
        owned
            .preview_latest_path(SESSION, &file)
            .unwrap_err()
            .code(),
        "no_undo_history"
    );
    assert_eq!(
        owned.restore_latest(SESSION, &file).unwrap_err().code(),
        "no_undo_history"
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "v2");
    let rows = aft::db::backups::list_backup_stacks(&db.lock().unwrap(), None, None).unwrap();
    assert!(rows.is_empty(), "{rows:?}");
    assert!(output.contains("ran by: daemon"), "{output}");
    assert!(
        output.contains("removed: 1 stack(s), 2 entries"),
        "{output}"
    );
}

#[test]
fn dry_run_without_owner_reports_and_removes_nothing() {
    allow_temp_backups();
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let db = Arc::new(Mutex::new(
        aft::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    let file = project.path().join("keep.txt");
    fs::write(&file, "v0").unwrap();
    let mut store = store_for(storage.path(), &db);
    store.snapshot(SESSION, &file, "edit").unwrap();
    drop(store);
    drop(db);

    let mut output = Vec::new();
    run_with(
        args(&["purge", "--session", SESSION, "--json"]),
        storage.path().to_path_buf(),
        project.path().to_path_buf(),
        &NoOwner,
        &mut output,
    )
    .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(body["report"]["dry_run"], true);
    assert_eq!(body["report"]["matched"]["stacks"], 1);
    assert_eq!(body["report"]["matched"]["entries"], 1);
    assert_eq!(body["report"]["removed"]["stacks"], 0);
    assert!(body["ran_by"].as_str().unwrap().starts_with("this process"));

    let db = Arc::new(Mutex::new(
        aft::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    let store = store_for(storage.path(), &db);
    assert_eq!(store.history(SESSION, &file).len(), 1);
}

#[test]
fn a_failed_owner_is_not_bypassed() {
    allow_temp_backups();
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let db = Arc::new(Mutex::new(
        aft::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    let file = project.path().join("owned.txt");
    fs::write(&file, "v0").unwrap();
    let mut store = store_for(storage.path(), &db);
    store.snapshot(SESSION, &file, "edit").unwrap();

    let error = run_with(
        args(&["purge", "--session", SESSION, "--yes"]),
        storage.path().to_path_buf(),
        project.path().to_path_buf(),
        &FailingOwner,
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 1);
    assert!(error.to_string().contains("simulated transport failure"));
    assert_eq!(store.history(SESSION, &file).len(), 1);
}

#[test]
fn arguments_are_validated() {
    let storage = tempfile::tempdir().unwrap();
    let cwd = storage.path().to_path_buf();
    let run = |items: &[&str]| {
        run_with(
            args(items),
            storage.path().to_path_buf(),
            cwd.clone(),
            &NoOwner,
            &mut Vec::new(),
        )
    };
    assert_eq!(run(&["purge"]).unwrap_err().exit_code(), 2);
    assert_eq!(run(&["prune"]).unwrap_err().exit_code(), 2);
    assert_eq!(run(&["purge", "--path"]).unwrap_err().exit_code(), 2);
    assert_eq!(
        run(&["purge", "--session", "a", "--session", "b"])
            .unwrap_err()
            .exit_code(),
        2
    );
    assert_eq!(run(&["purge", "--bogus"]).unwrap_err().exit_code(), 2);
    assert!(run(&["--help"]).is_ok());

    let parsed = parse_args(args(&["purge", "--path=rel/dir", "--harness", "pi", "-y"])).unwrap();
    assert_eq!(parsed.path, Some(PathBuf::from("rel/dir")));
    assert_eq!(parsed.harness.as_deref(), Some("pi"));
    assert!(parsed.yes);
}

#[test]
fn owner_envelopes_map_to_replies() {
    let not_owned = serde_json::json!({
        "op": BACKUPS_PURGE_OPERATION,
        "status": "error",
        "data": {"code": "storage_not_owned", "message": "elsewhere"},
    });
    assert!(matches!(
        owner_reply_from_envelope(&serde_json::to_vec(&not_owned).unwrap()),
        OwnerReply::NotOwned(_)
    ));
    let failed = serde_json::json!({
        "op": BACKUPS_PURGE_OPERATION,
        "status": "error",
        "data": {"code": "internal_error", "message": "boom"},
    });
    assert!(matches!(
        owner_reply_from_envelope(&serde_json::to_vec(&failed).unwrap()),
        OwnerReply::Failed(message) if message.contains("boom")
    ));
    let ok = serde_json::json!({
        "op": BACKUPS_PURGE_OPERATION,
        "status": "ok",
        "data": serde_json::to_value(PurgeReport::default()).unwrap(),
    });
    assert!(matches!(
        owner_reply_from_envelope(&serde_json::to_vec(&ok).unwrap()),
        OwnerReply::Report(_)
    ));
}
