use std::fs;

use aft::harness::Harness;

#[test]
fn cleanup_staging_dirs_removes_orphans() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("opencode/staging-bash-tasks-aaa/nested")).unwrap();
    fs::create_dir_all(root.join("opencode/staging-backups-bbb")).unwrap();

    let removed = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();

    assert_eq!(removed, 2);
    assert!(!root.join("opencode/staging-bash-tasks-aaa").exists());
    assert!(!root.join("opencode/staging-backups-bbb").exists());
}

#[test]
fn cleanup_staging_dirs_leaves_non_staging_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("opencode/staging-x")).unwrap();
    fs::create_dir_all(root.join("opencode/regular-dir")).unwrap();

    let removed = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();

    assert_eq!(removed, 1);
    assert!(!root.join("opencode/staging-x").exists());
    assert!(root.join("opencode/regular-dir").exists());
}

#[test]
fn cleanup_staging_dirs_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join("opencode/staging-x")).unwrap();

    let first = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();
    let second = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();

    assert_eq!(first, 1);
    assert_eq!(second, 0);
}

#[test]
fn cleanup_staging_dirs_handles_missing_harness_dir() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing-root");

    let removed = aft::migrate_storage::cleanup_staging_dirs(&root, Harness::Opencode).unwrap();

    assert_eq!(removed, 0);
}
