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
