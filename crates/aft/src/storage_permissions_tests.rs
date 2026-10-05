//! Exercise actual store writers rather than reproducing their creation logic.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn storage_creation_walk_is_owner_only() {
    let scratch = tempfile::tempdir().unwrap();
    let storage = scratch.path().join("storage");
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let source = project.join("main.rs");
    fs::write(&source, "fn main() { leaf(); }\nfn leaf() {}\n").unwrap();
    let key = crate::search_index::artifact_cache_key(&project);
    crate::root_cache::configure_artifact_access(&project, &key, false);

    let db = crate::db::open(&storage.join("aft.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE private_test(value TEXT); INSERT INTO private_test VALUES ('secret');",
    )
    .unwrap();
    let harness = storage.join("opencode");
    let task =
        crate::bash_background::persistence::allocate_task_layout(&harness, "session").unwrap();
    task.dirs
        .io
        .open_new_file(std::ffi::OsStr::new("stdout"))
        .unwrap();

    let mut backups = crate::backup::BackupStore::new();
    backups.set_storage_dir(harness.clone(), 72);
    assert!(backups
        .snapshot("session", &source, "snapshot")
        .unwrap()
        .is_some());
    let mut checkpoints = crate::checkpoint::CheckpointStore::new();
    checkpoints.set_storage_dir_for_harness(storage.clone(), crate::harness::Harness::Opencode);
    checkpoints
        .create_for_files("session", "snapshot", vec![source.clone()])
        .unwrap();

    let (callgraph, _) = crate::callgraph_store::CallGraphStore::cold_build_with_lease(
        storage.join("callgraph").join(&key),
        project.clone(),
        std::slice::from_ref(&source),
    )
    .unwrap();
    let mut search = crate::search_index::SearchIndex::build(&project);
    assert!(search.write_to_disk(&storage.join("index").join(&key), None));
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, &key));
    crate::gh_shim::write_storage_permission_fixture(&storage.join("state"));
    crate::logging::write_storage_permission_fixture(&storage.join("logs"));

    let mut paths = vec![storage];
    let mut checked = 0;
    while let Some(path) = paths.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            metadata.permissions().mode() & 0o077,
            0,
            "{} has group/world bits: {:o}",
            path.display(),
            metadata.permissions().mode() & 0o777
        );
        checked += 1;
        if metadata.is_dir() {
            paths.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
    assert!(checked > 25, "walk must reach all stores: {checked}");
    drop((callgraph, db));
}

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
    let project = scratch.path().join("project");
    fs::create_dir(&project).unwrap();
    let semantic = crate::semantic_index::SemanticIndex::new(project, 3);
    assert!(semantic.write_to_disk(&storage, "root-key"));
    assert_mode(&cache, 0o700);
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
}

fn assert_mode(path: &Path, mode: u32) {
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        mode,
        "{}",
        path.display()
    );
}
