//! Golden ordinary output and reader values captured from the pinned main
//! revision, including the distinction between absent and empty guard fields.

use super::*;
use serde_json::{json, Value};

fn fixture() -> Value {
    let captured: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/agent_data/ordinary-main-format.json"
    ))
    .unwrap();
    #[cfg(not(unix))]
    let captured = {
        let mut captured = captured;
        captured["checkpoint"]["meta"]["files"][0]["mode"] = Value::Null;
        captured["checkpoint"]["decoded"]["files"][0]["mode"] = Value::Null;
        captured
    };
    captured
}

#[test]
fn ordinary_checkpoint_matches_origin_main_format() {
    assert!(!crate::agent_data::CATALOG_ENABLED);
    let expected = fixture();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo/file.bin");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"fixture-\0\xff-before\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    }
    let mut store = CheckpointStore::new();
    store.set_storage_dir_for_harness(temp.path().join("store"), crate::harness::Harness::Pi);
    store
        .create_for_files("compat-session", "ordinary", vec![path.clone()])
        .unwrap();
    let dir = store
        .durable_checkpoint_dir("compat-session", "ordinary")
        .unwrap();
    let mut meta: Value =
        serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert!(meta["created_at"].as_u64().is_some());
    assert!(meta["created_order"].as_u64().is_some());
    meta["created_at"] = json!(0);
    meta["created_order"] = json!(1);
    for file in meta["files"].as_array_mut().unwrap() {
        assert!(!file["blob"].as_str().unwrap().contains('/'));
        file["original_path"] = json!("/fixture/repo/file.bin");
        file["blob"] = json!("file_1_0_0.blob");
    }
    let decoded = read_checkpoint_from_disk(&store, &dir, "compat-session", "ordinary").unwrap();
    assert!(decoded.imported_guards.is_empty());
    let file = decoded.file_contents.get(&path).unwrap();
    let kind = match file.kind {
        CheckpointFileKind::Regular { .. } => "regular",
        _ => panic!("expected regular file"),
    };
    let actual = json!({"meta":meta,"decoded":{"name":decoded.name,"created_at":0,"created_order":1,
        "pending_purge_count":decoded.pending_purge.len(),"files":[{"path":"/fixture/repo/file.bin","kind":kind,
        "mode":file.mode,"content_bytes":checkpoint_file_bytes(file).as_ref()}]}});
    assert_eq!(
        serde_json::to_string(&actual).unwrap(),
        serde_json::to_string(&expected["checkpoint"]).unwrap()
    );
    assert_eq!(actual, expected["checkpoint"]);
}

#[test]
fn imported_checkpoint_alone_uses_guarded_schema() {
    use crate::agent_data::{ExpectedState, FileRecord, SnapshotGroup};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.bin");
    fs::write(&path, b"after").unwrap();
    let key = crate::agent_data::registry::PortablePath {
        project_id: "fixture-project".into(),
        root_key: cortexkit_project_identity::RootKey::new(
            cortexkit_project_identity::RootKeyKind::Label,
            "fixture".into(),
        ),
        relative_path: "file.bin".into(),
    };
    let group = SnapshotGroup::Checkpoint {
        name: "imported".into(),
        created_at: current_timestamp(),
        created_order: current_timestamp_nanos(),
        files: vec![FileRecord {
            path: key,
            id: "imported-entry".into(),
            order: 1,
            timestamp: 1,
            mode: None,
            before: Some(b"before".to_vec()),
            expected: ExpectedState::capture(&path).unwrap(),
        }],
    };
    let mut store = CheckpointStore::new();
    store.set_storage_dir_for_harness(temp.path().join("store"), crate::harness::Harness::Pi);
    store
        .install_portable_checkpoints("compat-session", &[(group, vec![path])])
        .unwrap();
    let dir = store
        .durable_checkpoint_dir("compat-session", "imported")
        .unwrap();
    let meta: Value = serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["schema_version"], 2);
    assert_eq!(meta["imported_guards"].as_object().unwrap().len(), 1);
}
