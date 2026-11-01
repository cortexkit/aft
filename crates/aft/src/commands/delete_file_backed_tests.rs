use super::*;
#[cfg(unix)]
use crate::durability::EventKind;
use crate::durability::{take, take_full_syncs};
use std::time::Duration;

struct Fixture {
    _project: tempfile::TempDir,
    storage: tempfile::TempDir,
    tree: tempfile::TempDir,
    ctx: AppContext,
}

impl Fixture {
    fn new(files: usize) -> Self {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        for i in 0..files {
            std::fs::write(
                tree.path().join(format!("leaf-{i}")),
                format!("original-{i}"),
            )
            .unwrap();
        }
        let mut config = crate::config::Config::default();
        config.project_root = Some(project.path().to_path_buf());
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.backup()
            .lock()
            .set_storage_dir(storage.path().into(), 24);
        Self {
            _project: project,
            storage,
            tree,
            ctx,
        }
    }

    fn delete(&self) -> Response {
        let req: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "backed-delete", "command": "delete_file",
            "file": self.tree.path(), "recursive": true,
        }))
        .unwrap();
        handle_delete_file(&req, &self.ctx)
    }
}

fn clear_syncs() {
    take();
    take_full_syncs();
}

#[test]
#[cfg(unix)]
fn backed_delete_commits_one_full_sync_before_unlink_and_restores_after_restart() {
    let fixture = Fixture::new(64);
    clear_syncs();
    let _seam = install_unlink_seam_for_test(Box::new(|stage| {
        if stage == UnlinkStage::BeforeUnlink {
            let events = take();
            assert_eq!(
                take_full_syncs().len(),
                1,
                "one device barrier before the first unlink"
            );
            assert!(events.iter().any(|(kind, _)| *kind == EventKind::FileSync));
        }
    }));
    let response = fixture.delete();
    assert!(response.success, "{:?}", response.data);
    assert!(!fixture.tree.path().exists());
    let mut reopened = crate::backup::BackupStore::new();
    reopened.set_storage_dir(fixture.storage.path().into(), 24);
    reopened
        .restore_last_operation(crate::protocol::DEFAULT_SESSION_ID)
        .unwrap();
    for i in 0..64 {
        assert_eq!(
            std::fs::read_to_string(fixture.tree.path().join(format!("leaf-{i}"))).unwrap(),
            format!("original-{i}")
        );
    }
}

struct ResetClock;
impl Drop for ResetClock {
    fn drop(&mut self) {
        CAPTURE_CLOCK.with(|clock| *clock.borrow_mut() = None);
    }
}

#[test]
fn backed_delete_deadline_stops_before_the_next_snapshot_without_unlink() {
    assert_capture_expires(32);
}

#[test]
fn backed_delete_deadline_after_last_snapshot_still_never_commits_or_unlinks() {
    assert_capture_expires(4);
}

fn assert_capture_expires(files: usize) {
    let fixture = Fixture::new(files);
    let budget = crate::bounded_io::Budget::after(Duration::from_secs(3600));
    let deadline = budget.deadline;
    let _budget = crate::bounded_io::enter(Some(budget));
    let mut checks = 0;
    CAPTURE_CLOCK.with(|clock| {
        *clock.borrow_mut() = Some(Box::new(move || {
            checks += 1;
            if checks <= 5 {
                deadline - Duration::from_secs(1)
            } else {
                deadline
            }
        }));
    });
    let _clock = ResetClock;
    DELETE_OPERATIONS.with(|ops| *ops.borrow_mut() = DeleteOperations::default());
    clear_syncs();
    let response = fixture.delete();
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(response.data["code"], "read_deadline_exceeded");
    drop(_budget);
    DELETE_OPERATIONS.with(|ops| {
        assert_eq!(ops.borrow().snapshots, 5);
        assert_eq!(ops.borrow().unlinks, 0);
    });
    assert_eq!(
        std::fs::read_dir(fixture.tree.path()).unwrap().count(),
        files
    );
    #[cfg(unix)]
    assert_eq!(
        take_full_syncs().len(),
        0,
        "abandoned captures and discard need no device flush"
    );
    assert!(fixture
        .ctx
        .backup()
        .lock()
        .history(
            crate::protocol::DEFAULT_SESSION_ID,
            &fixture.tree.path().join("leaf-0")
        )
        .is_empty());
    let mut reopened = crate::backup::BackupStore::new();
    reopened.set_storage_dir(fixture.storage.path().into(), 24);
    assert!(reopened
        .history(crate::protocol::DEFAULT_SESSION_ID, fixture.tree.path())
        .is_empty());
}

#[test]
fn backed_delete_expired_capture_starts_no_snapshots() {
    let fixture = Fixture::new(8);
    let budget = crate::bounded_io::Budget::after(Duration::from_secs(3600));
    let deadline = budget.deadline;
    let _budget = crate::bounded_io::enter(Some(budget));
    CAPTURE_CLOCK.with(|clock| *clock.borrow_mut() = Some(Box::new(move || deadline)));
    let _clock = ResetClock;
    DELETE_OPERATIONS.with(|ops| *ops.borrow_mut() = DeleteOperations::default());
    let response = fixture.delete();
    assert_eq!(response.data["code"], "read_deadline_exceeded");
    DELETE_OPERATIONS.with(|ops| assert_eq!(ops.borrow().snapshots, 0));
    assert_eq!(std::fs::read_dir(fixture.tree.path()).unwrap().count(), 8);
}

#[test]
fn backed_delete_discard_does_not_full_sync_each_removed_snapshot() {
    let fixture = Fixture::new(16);
    let paths = (0..16)
        .map(|i| fixture.tree.path().join(format!("leaf-{i}")))
        .collect::<Vec<_>>();
    for path in &paths {
        fixture
            .ctx
            .backup()
            .lock()
            .snapshot_with_op(
                crate::protocol::DEFAULT_SESSION_ID,
                path,
                "pre-delete",
                Some("discard"),
            )
            .unwrap();
    }
    clear_syncs();
    discard_delete_backups(
        &fixture.ctx,
        crate::protocol::DEFAULT_SESSION_ID,
        "discard",
        &paths,
    );
    assert_eq!(take_full_syncs().len(), 0);
    for path in paths {
        assert!(path.exists());
        assert!(fixture
            .ctx
            .backup()
            .lock()
            .history(crate::protocol::DEFAULT_SESSION_ID, &path)
            .is_empty());
    }
}

#[test]
#[cfg(unix)]
fn backed_delete_failed_group_commit_never_unlinks_and_restores_sync_policy() {
    let fixture = Fixture::new(8);
    DELETE_OPERATIONS.with(|ops| *ops.borrow_mut() = DeleteOperations::default());
    clear_syncs();
    crate::durability::FAIL_GROUP_SYNC.with(|fail| fail.set(true));
    let response = fixture.delete();
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(response.data["code"], "io_error");
    DELETE_OPERATIONS.with(|ops| assert_eq!(ops.borrow().unlinks, 0));
    assert_eq!(std::fs::read_dir(fixture.tree.path()).unwrap().count(), 8);
    assert_eq!(take_full_syncs().len(), 0);
    fixture
        .ctx
        .backup()
        .lock()
        .snapshot(
            "after",
            &fixture.tree.path().join("leaf-0"),
            "unrelated backup",
        )
        .unwrap();
    assert!(
        !take_full_syncs().is_empty(),
        "ordinary snapshots retain full durability"
    );
}
