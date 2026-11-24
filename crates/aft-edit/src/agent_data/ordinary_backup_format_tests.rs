//! Ordinary history must keep the format emitted and decoded by main. The
//! fixture was captured with main's real writer, disk reader and DB-row reader;
//! only paths, generated IDs and clock values are normalized here.

use super::*;
use serde_json::{json, Value};

const SESSION: &str = "compat-session";
const FIXTURE_PATH: &str = "/fixture/repo/file.bin";

fn entry_view(entry: &BackupEntry) -> Value {
    json!({"backup_id":"backup-fixture", "content_bytes":entry.content_bytes().unwrap().as_ref(),
        "timestamp":0,"order":"1","description":entry.description,"op_id":entry.op_id,
        "kind":entry.kind.as_str(),"mode":entry.mode,"link_target":entry.link_target,
        "created_dirs":entry.created_dirs,"post_state":entry.post_state.as_ref().map(PathFingerprint::to_meta_string),
        "external_change_before":entry.external_change_before,"external_change_checkpoint":entry.external_change_checkpoint,
        "link_to":entry.link_to,"hardlink_detached":entry.hardlink_detached})
}

fn fixture() -> Value {
    let captured: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/agent_data/ordinary-main-format.json"
    ))
    .unwrap();
    #[cfg(not(unix))]
    let captured = {
        let mut captured = captured;
        for path in [
            "/backup/stack_meta/entries/0/mode",
            "/backup/restore_meta/mode",
            "/backup/decoded_disk/mode",
            "/backup/decoded_db_row/mode",
        ] {
            *captured.pointer_mut(path).unwrap() = Value::Null;
        }
        captured
    };
    captured
}

#[test]
fn ordinary_undo_snapshot_matches_origin_main_format() {
    assert!(!crate::agent_data::CATALOG_ENABLED);
    let expected = fixture();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("repo/file.bin");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"fixture-\0\xff-before\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let storage = temp.path().join("store");
    create_private_dir_all(&storage).unwrap();
    assert!(crate::reader_floor::prepare(&storage).is_empty());
    let floor = crate::reader_floor::read(&storage).unwrap().unwrap();
    assert_eq!(
        floor.stores.get("backup_meta"),
        expected["backup"]["stack_meta"]["schema_version"]
            .as_u64()
            .as_ref()
    );
    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage, crate::harness::Harness::Pi, 0);
    store
        .snapshot_with_op(SESSION, &path, "ordinary snapshot", Some("compat-op"))
        .unwrap();
    std::fs::write(&path, b"fixture-after\n").unwrap();
    store.record_post_mutation_states();
    let key = canonicalize_key(&path);
    let session_dir = store.session_dir(SESSION).unwrap();
    let stack_dir = session_dir.join(BackupStore::path_hash(&key));
    let stack = store
        .read_stack_from_disk_unlocked(SESSION, &key)
        .unwrap()
        .unwrap();
    assert_eq!(stack.len(), 1);
    let entry = &stack[0];
    assert!(entry.imported_guard.is_none());
    let mut meta: Value =
        serde_json::from_slice(&std::fs::read(stack_dir.join("meta.json")).unwrap()).unwrap();
    meta["path"] = json!(FIXTURE_PATH);
    for item in meta["entries"].as_array_mut().unwrap() {
        assert!(item["timestamp"].as_u64().is_some());
        item["order"].as_str().unwrap().parse::<u128>().unwrap();
        assert!(!item["backup_id"].as_str().unwrap().is_empty());
        assert!(!item["content_path"].as_str().unwrap().contains('/'));
        item["backup_id"] = json!("backup-fixture");
        item["timestamp"] = json!(0);
        item["order"] = json!("1");
        item["content_path"] = json!("bak_1_backup-fixture.bak");
    }
    let mut marker: Value =
        serde_json::from_slice(&std::fs::read(session_dir.join("session.json")).unwrap()).unwrap();
    assert!(marker["last_accessed"].as_u64().is_some());
    marker["last_accessed"] = json!(0);
    let states: Value =
        serde_json::from_slice(&std::fs::read(stack_dir.join("post-state.json")).unwrap()).unwrap();
    assert_eq!(states.as_object().unwrap().len(), 1);
    let states = json!({"backup-fixture":states[&entry.backup_id]});
    let blob = match &entry.content {
        BackupContent::OnDisk { path, .. } => path.to_str().unwrap(),
        _ => panic!("expected durable content"),
    };
    let row = entry.to_backup_row(
        "pi",
        SESSION,
        "compat-project",
        path.to_str().unwrap(),
        &BackupStore::path_hash(&key),
        Some(blob),
    );
    let restore_meta: Value = serde_json::from_str(row.restore_meta.as_deref().unwrap()).unwrap();
    assert!(restore_metadata_from_json(row.restore_meta.as_deref().unwrap()).is_some());
    let decoded_row = BackupEntry::try_from(row).unwrap();
    assert!(decoded_row.imported_guard.is_none());
    let actual = json!({"stack_meta":meta,"session_meta":marker,"post_state":states,"restore_meta":restore_meta,
        "decoded_disk":entry_view(entry),"decoded_db_row":entry_view(&decoded_row)});
    assert_eq!(
        serde_json::to_string(&actual).unwrap(),
        serde_json::to_string(&expected["backup"]).unwrap()
    );
    assert_eq!(actual, expected["backup"]);
}

#[test]
fn ordinary_migration_and_marker_writes_keep_main_schema() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("meta.json");
    crate::private_storage::write(
        &path,
        br#"{"schema_version":2,"count":1,"path":"/fixture/repo/file.bin"}"#,
    )
    .unwrap();
    BackupStore::upgrade_meta_file(&path, SESSION);
    let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        value["schema_version"],
        fixture()["backup"]["stack_meta"]["schema_version"]
    );
    assert!(value["entries"][0].get("imported_guard").is_none());
    assert_eq!(
        crate::persisted_format::PersistedStore::BackupMeta.written(),
        5
    );
    assert_eq!(
        crate::persisted_format::PersistedStore::BackupMeta.supported(),
        6
    );
}

#[test]
fn imported_undo_alone_opts_into_guarded_format_and_floor() {
    use crate::agent_data::{ExpectedState, FileRecord, SnapshotGroup};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file.bin");
    std::fs::write(&path, b"after").unwrap();
    let storage = temp.path().join("store");
    create_private_dir_all(&storage).unwrap();
    assert!(crate::reader_floor::prepare(&storage).is_empty());
    assert_eq!(
        crate::reader_floor::read(&storage).unwrap().unwrap().stores["backup_meta"],
        5
    );
    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage.clone(), crate::harness::Harness::Pi, 0);
    let key = crate::agent_data::registry::PortablePath {
        project_id: "fixture-project".into(),
        root_key: cortexkit_project_identity::RootKey::new(
            cortexkit_project_identity::RootKeyKind::Label,
            "fixture".into(),
        ),
        relative_path: "file.bin".into(),
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
            expected: ExpectedState::capture(&path).unwrap(),
        }],
    };
    store
        .install_portable_undo(SESSION, &[(group, vec![path.clone()])])
        .unwrap();
    let dir = store
        .session_dir(SESSION)
        .unwrap()
        .join(BackupStore::path_hash(&canonicalize_key(&path)));
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["schema_version"], 6);
    assert!(meta["entries"][0]["imported_guard"].is_object());
    let entry = store.history(SESSION, &path).pop().unwrap();
    let restore: Value = serde_json::from_str(&restore_metadata_json(&entry)).unwrap();
    assert_eq!(restore["version"], 3);
    assert!(restore["imported_guard"].is_object());
    assert_eq!(
        crate::reader_floor::read(&storage).unwrap().unwrap().stores["backup_meta"],
        6
    );
}
