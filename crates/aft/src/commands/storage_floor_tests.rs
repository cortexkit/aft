use crate::config::Config;
use crate::context::AppContext;
use crate::language::StubProvider;
use crate::protocol::{RawRequest, Response};
use serde_json::json;
use std::path::{Path, PathBuf};

struct Fixture {
    _storage_env: crate::test_env::StorageOverrideGuard,
    _root: tempfile::TempDir,
    ctx: AppContext,
    store: PathBuf,
    neighbor: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let storage_env = crate::test_env::without_storage_override();
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("storage");
        std::fs::create_dir(&storage).unwrap();
        let store = storage.join("aft.db");
        let neighbor = root.path().join("neighbor.txt");
        std::fs::write(&store, "original\n").unwrap();
        std::fs::write(&neighbor, "original\n").unwrap();
        let ctx = AppContext::new(
            Box::new(StubProvider),
            Config {
                project_root: Some(root.path().to_path_buf()),
                storage_dir: Some(storage),
                restrict_to_project_root: false,
                ..Config::default()
            },
        );
        Self {
            _storage_env: storage_env,
            _root: root,
            ctx,
            store,
            neighbor,
        }
    }

    fn assert_refused(&self, call: impl FnOnce() -> Response) {
        let identity = crate::db::file_identity::identity_of(&self.store);
        let modified = std::fs::metadata(&self.store).unwrap().modified().unwrap();
        let response = call();
        assert_eq!(response.data["code"], "denied_floor", "{response:?}");
        assert_eq!(
            response.data["message"],
            format!(
                "path '{}' is denied by the access policy floor",
                self.store.display()
            )
        );
        assert_eq!(crate::db::file_identity::identity_of(&self.store), identity);
        assert_eq!(
            std::fs::metadata(&self.store).unwrap().modified().unwrap(),
            modified
        );
        assert!(self
            .ctx
            .backup()
            .lock()
            .tracked_files("storage-floor")
            .is_empty());
    }
}

fn request(command: &str, params: serde_json::Value) -> RawRequest {
    RawRequest {
        id: "storage-floor".into(),
        command: command.into(),
        session_id: Some("storage-floor".into()),
        lsp_hints: None,
        params,
    }
}

#[test]
fn read_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::read::handle_read(&request("read", json!({"file": f.store})), &f.ctx)
    });
}

#[test]
fn write_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::write::handle_write(
            &request("write", json!({"file": f.store, "content": "changed"})),
            &f.ctx,
        )
    });
}

#[test]
fn edit_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::edit_match::handle_edit_match(
            &request(
                "edit_match",
                json!({"file": f.store, "match": "original", "replacement": "changed"}),
            ),
            &f.ctx,
        )
    });
}

fn patch(path: &Path) -> String {
    format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-original\n+changed\n*** End Patch",
        path.display()
    )
}

#[test]
fn apply_patch_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::apply_patch::handle_apply_patch(
            &request("apply_patch", json!({"patch_text": patch(&f.store)})),
            &f.ctx,
        )
    });
}

#[test]
fn delete_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::delete_file::handle_delete_file(
            &request("delete_file", json!({"files": [f.store]})),
            &f.ctx,
        )
    });
}

#[test]
fn move_refuses_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::move_file::handle_move_file(
            &request(
                "move_file",
                json!({"file": f.store, "destination": f.neighbor.with_extension("moved")}),
            ),
            &f.ctx,
        )
    });
    f.assert_refused(|| {
        super::move_file::handle_move_file(
            &request(
                "move_file",
                json!({"file": f.neighbor, "destination": f.store}),
            ),
            &f.ctx,
        )
    });
}

#[test]
fn normal_neighbor_of_storage_floor_still_works() {
    let f = Fixture::new();
    let read = super::read::handle_read(&request("read", json!({"file": f.neighbor})), &f.ctx);
    assert!(read.success, "{read:?}");
    let write = super::write::handle_write(
        &request("write", json!({"file": f.neighbor, "content": "normal"})),
        &f.ctx,
    );
    assert!(write.success, "{write:?}");
    assert_eq!(std::fs::read_to_string(&f.neighbor).unwrap(), "normal");
    let edit = super::edit_match::handle_edit_match(
        &request(
            "edit_match",
            json!({"file": f.neighbor, "match": "normal", "replacement": "original\n"}),
        ),
        &f.ctx,
    );
    assert!(edit.success, "{edit:?}");
    let patch = super::apply_patch::handle_apply_patch(
        &request("apply_patch", json!({"patch_text": patch(&f.neighbor)})),
        &f.ctx,
    );
    assert!(patch.success, "{patch:?}");
    let destination = f.neighbor.with_extension("moved");
    let moved = super::move_file::handle_move_file(
        &request(
            "move_file",
            json!({"file": f.neighbor, "destination": destination}),
        ),
        &f.ctx,
    );
    assert!(moved.success, "{moved:?}");
    let deleted = super::delete_file::handle_delete_file(
        &request("delete_file", json!({"files": [destination]})),
        &f.ctx,
    );
    assert!(deleted.success, "{deleted:?}");
    assert!(!destination.exists());
}

#[test]
fn grep_and_glob_explicit_paths_refuse_storage_floor() {
    let f = Fixture::new();
    f.assert_refused(|| {
        super::grep::handle_grep(
            &request("grep", json!({"path": f.store, "pattern": "original"})),
            &f.ctx,
        )
    });
    f.assert_refused(|| {
        super::glob::handle_glob(
            &request("glob", json!({"path": f.store, "pattern": "*"})),
            &f.ctx,
        )
    });
}

#[test]
fn recursive_delete_refuses_ancestor_of_storage_floor() {
    let f = Fixture::new();
    let response = super::delete_file::handle_delete_file(
        &request(
            "delete_file",
            json!({"files": [f._root.path()], "recursive": true}),
        ),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "denied_floor", "{response:?}");
    assert!(f.store.exists());
    assert!(f.neighbor.exists());
}

struct SqliteAliases {
    _connection: crate::db::file_identity::IdentityConnection,
    fixture: Fixture,
    aliases: Vec<PathBuf>,
    history: crate::backup::BackupStore,
}

impl SqliteAliases {
    fn new() -> Self {
        let fixture = Fixture::new();
        std::fs::remove_file(&fixture.store).unwrap();
        // Seed old history before SQLite opens, then replace the project files
        // with aliases. The fixture itself must not read a live SQLite file.
        let mut history = crate::backup::BackupStore::new();
        let aliases = ["", "-wal", "-shm"]
            .into_iter()
            .enumerate()
            .map(|(i, _)| {
                let alias = fixture._root.path().join(format!("alias-{i}.txt"));
                std::fs::write(&alias, "original\n").unwrap();
                history
                    .snapshot_with_op("alias", &alias, "before registration", Some("alias-op"))
                    .unwrap();
                alias
            })
            .collect::<Vec<_>>();
        let connection = rusqlite::Connection::open(&fixture.store).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE source(value TEXT);")
            .unwrap();
        for (alias, suffix) in aliases.iter().zip(["", "-wal", "-shm"]) {
            std::fs::remove_file(alias).unwrap();
            std::fs::hard_link(
                PathBuf::from(format!("{}{suffix}", fixture.store.display())),
                alias,
            )
            .unwrap();
        }
        let connection =
            crate::db::file_identity::IdentityConnection::new(connection, "storage alias fixture");
        Self {
            _connection: connection,
            fixture,
            aliases,
            history,
        }
    }

    fn assert_refused(&self, path: &Path, call: impl FnOnce() -> Response) {
        let before = crate::file_identity::metadata(path).unwrap();
        let response = call();
        assert_eq!(
            response.data["code"], "denied_floor",
            "{path:?}: {response:?}"
        );
        let after = crate::file_identity::metadata(path).unwrap();
        assert_eq!(before.1, after.1);
        assert_eq!(before.0.modified().unwrap(), after.0.modified().unwrap());
    }
}

#[test]
fn storage_floor_live_sqlite_aliases_refuse_read_write_edit() {
    let f = SqliteAliases::new();
    for path in &f.aliases {
        f.assert_refused(path, || {
            super::read::handle_read(&request("read", json!({"file": path})), &f.fixture.ctx)
        });
        f.assert_refused(path, || {
            super::write::handle_write(
                &request("write", json!({"file": path, "content": "changed"})),
                &f.fixture.ctx,
            )
        });
        f.assert_refused(path, || {
            super::edit_match::handle_edit_match(
                &request(
                    "edit_match",
                    json!({"file": path, "match": "original", "replacement": "changed"}),
                ),
                &f.fixture.ctx,
            )
        });
    }
    assert_eq!(
        f._connection
            .query_row("SELECT count(*) FROM source", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn storage_floor_live_sqlite_aliases_refuse_backup_capture_and_undo() {
    let mut f = SqliteAliases::new();
    for path in &f.aliases {
        let before = crate::file_identity::metadata(path).unwrap();
        let mut snapshots = crate::backup::BackupStore::new();
        assert_eq!(
            snapshots
                .snapshot("alias", path, "refused")
                .unwrap_err()
                .code(),
            "denied_floor"
        );
        assert!(crate::backup::CapturedRegularFile::read(path).is_err());
        assert_eq!(
            crate::backup::PathFingerprint::of_path(path),
            crate::backup::PathFingerprint::Other
        );
        assert_eq!(
            f.history.restore_latest("alias", path).unwrap_err().code(),
            "denied_floor"
        );
        let after = crate::file_identity::metadata(path).unwrap();
        assert_eq!(before.1, after.1);
        assert_eq!(before.0.modified().unwrap(), after.0.modified().unwrap());
    }
    assert_eq!(
        f.history
            .restore_last_operation("alias")
            .unwrap_err()
            .code(),
        "denied_floor"
    );
    assert_eq!(
        f._connection
            .query_row("SELECT count(*) FROM source", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn storage_floor_sidecars_created_after_registration_refuse_project_aliases() {
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.store).unwrap();
    let connection =
        crate::db::file_identity::IdentityConnection::open(&fixture.store, "late sidecar fixture")
            .unwrap();
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE source(value TEXT);")
        .unwrap();
    for suffix in ["-wal", "-shm"] {
        let source = PathBuf::from(format!("{}{suffix}", fixture.store.display()));
        let alias = fixture._root.path().join(format!("late{suffix}.txt"));
        std::fs::hard_link(source, &alias).unwrap();
        let response =
            super::read::handle_read(&request("read", json!({"file": alias})), &fixture.ctx);
        assert_eq!(
            response.data["code"], "denied_floor",
            "{suffix}: {response:?}"
        );
    }
}

#[test]
fn storage_floor_ordinary_project_hard_links_still_work() {
    let f = Fixture::new();
    let alias = f._root.path().join("ordinary-alias.txt");
    std::fs::hard_link(&f.neighbor, &alias).unwrap();
    let read = super::read::handle_read(&request("read", json!({"file": alias})), &f.ctx);
    assert!(read.success, "{read:?}");
    let write = super::write::handle_write(
        &request("write", json!({"file": alias, "content": "normal"})),
        &f.ctx,
    );
    assert!(write.success, "{write:?}");
    std::fs::remove_file(&alias).unwrap();
    std::fs::write(&f.neighbor, "normal").unwrap();
    std::fs::hard_link(&f.neighbor, &alias).unwrap();
    let edit = super::edit_match::handle_edit_match(
        &request(
            "edit_match",
            json!({"file": alias, "match": "normal", "replacement": "changed"}),
        ),
        &f.ctx,
    );
    assert!(edit.success, "{edit:?}");
    std::fs::remove_file(&alias).unwrap();
    std::fs::write(&f.neighbor, "changed").unwrap();
    std::fs::hard_link(&f.neighbor, &alias).unwrap();
    let mut snapshots = crate::backup::BackupStore::new();
    assert!(snapshots
        .snapshot("ordinary", &alias, "normal")
        .unwrap()
        .is_some());
    std::fs::write(&alias, "later").unwrap();
    snapshots.restore_latest("ordinary", &alias).unwrap();
    assert_eq!(std::fs::read_to_string(&alias).unwrap(), "changed");
}

#[test]
fn storage_floor_identity_check_stats_ordinary_file_once() {
    let f = Fixture::new();
    let stats = crate::db::file_identity::stat_probe::count(|| {
        assert!(!crate::db::file_identity::is_live_sqlite_file(&f.neighbor));
    });
    assert_eq!(stats, std::collections::BTreeMap::from([(f.neighbor, 1)]));
}
