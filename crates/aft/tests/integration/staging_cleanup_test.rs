use std::fs;

use aft::harness::Harness;

#[test]
fn cleanup_staging_dirs_removes_orphans() {
    let temp = crate::test_helpers::tempdir();
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
    let temp = crate::test_helpers::tempdir();
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
    let temp = crate::test_helpers::tempdir();
    let root = temp.path();
    fs::create_dir_all(root.join("opencode/staging-x")).unwrap();

    let first = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();
    let second = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();

    assert_eq!(first, 1);
    assert_eq!(second, 0);
}

#[test]
fn cleanup_staging_dirs_removes_root_target_and_child_union_orphans() {
    let temp = crate::test_helpers::tempdir();
    let root = temp.path();
    fs::create_dir_all(root.join("index/staging-index-project-a/nested")).unwrap();
    fs::create_dir_all(root.join("semantic/staging-semantic-project-a")).unwrap();
    fs::create_dir_all(root.join("opencode/filters/staging-filters-custom")).unwrap();

    let removed = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();

    assert_eq!(removed, 3);
    assert!(!root.join("index/staging-index-project-a").exists());
    assert!(!root.join("semantic/staging-semantic-project-a").exists());
    assert!(!root
        .join("opencode/filters/staging-filters-custom")
        .exists());
}

#[test]
fn cleanup_staging_dirs_removes_file_target_staging_files() {
    let temp = crate::test_helpers::tempdir();
    let root = temp.path();
    fs::create_dir_all(root.join("opencode")).unwrap();
    fs::write(
        root.join("opencode/staging-last_announced_version-last_announced_version-1-2"),
        "0.30.0",
    )
    .unwrap();
    fs::write(
        root.join("opencode/staging-last-update-check.json-last-update-check.json-1-2"),
        "{}",
    )
    .unwrap();

    let removed = aft::migrate_storage::cleanup_staging_dirs(root, Harness::Opencode).unwrap();

    assert_eq!(removed, 2);
    assert!(!root
        .join("opencode/staging-last_announced_version-last_announced_version-1-2")
        .exists());
    assert!(!root
        .join("opencode/staging-last-update-check.json-last-update-check.json-1-2")
        .exists());
}

#[test]
fn cleanup_staging_dirs_handles_missing_harness_dir() {
    let temp = crate::test_helpers::tempdir();
    let root = temp.path().join("missing-root");

    let removed = aft::migrate_storage::cleanup_staging_dirs(&root, Harness::Opencode).unwrap();

    assert_eq!(removed, 0);
}
