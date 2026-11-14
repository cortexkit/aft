//! Exercise actual store writers rather than reproducing their creation logic.

use std::fs;

#[allow(unused_imports)]
pub(crate) use aft_base::storage_permissions_tests::*;

#[test]
fn storage_open_tightens_existing_root_without_changing_parent() {
    let scratch = tempfile::tempdir().unwrap();
    let parent = scratch.path().join("public-parent");
    let storage = parent.join("storage");
    let cache = storage.join("semantic").join("root-key");
    fs::create_dir_all(&cache).unwrap();
    for path in [&parent, &storage, &storage.join("semantic"), &cache] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old = cache.join("old.bin");
    fs::write(&old, "old secret").unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, "root-key"));
    assert_mode(&cache, 0o700);
    assert_mode(&storage, 0o700);
    assert_mode(&storage.join("semantic"), 0o700);
    assert_mode(&old, 0o644);
    assert_mode(&parent, 0o755);
}

#[test]
fn storage_open_does_not_chmod_symlink_targets() {
    let scratch = tempfile::tempdir().unwrap();
    let target = scratch.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let storage = scratch.path().join("storage");
    fs::create_dir_all(storage.join("semantic")).unwrap();
    std::os::unix::fs::symlink(&target, storage.join("semantic/root-key")).unwrap();
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, "root-key"));
    assert_mode(&target, 0o755);
    let nested = target.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&target, storage.join("alias")).unwrap();
    crate::private_storage::open_dir(&storage, &storage.join("alias/nested")).unwrap();
    assert_mode(&nested, 0o755);
}

#[test]
fn storage_existing_database_new_sidecars_are_private() {
    let scratch = tempfile::tempdir().unwrap();
    let storage = scratch.path().canonicalize().unwrap().join("storage");
    let path = storage.join("aft.db");
    drop(crate::db::open(&path).unwrap());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let db = crate::db::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE private_test(value TEXT); INSERT INTO private_test VALUES ('secret');",
    )
    .unwrap();
    for path in [
        &path,
        &storage.join("aft.db-wal"),
        &storage.join("aft.db-shm"),
    ] {
        assert_mode(path, 0o600);
    }
}
