use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use aft::views::assembly::{head_tree_fingerprint, publish_checkout, AssemblyRequest};
use tempfile::tempdir;

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_helpers::apply_hermetic_git_env(command.current_dir(root));
    assert!(
        command.args(args).status().unwrap().success(),
        "git {args:?}"
    );
}

fn commit(root: &Path, message: &str) {
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=AFT Tests",
            "-c",
            "user.email=aft-tests@example.com",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

fn request(
    storage: &Path,
    root: &Path,
    family: &str,
    scope: &str,
    changed_paths: BTreeSet<Vec<u8>>,
    allow_blob_put: bool,
) -> AssemblyRequest {
    let head = aft::alias::head_tree_entries(root).unwrap();
    AssemblyRequest {
        storage: storage.to_path_buf(),
        project_root: root.to_path_buf(),
        family: family.to_string(),
        scope: scope.to_string(),
        desired_head: head_tree_fingerprint(&head),
        changed_paths,
        allow_blob_put,
    }
}

#[test]
fn branch_switch_reuses_unchanged_blobs_and_puts_only_changed_files() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    for index in 0..320 {
        fs::write(
            project.path().join(format!("file_{index}.rs")),
            format!("pub fn value_{index}() -> usize {{ {index} }}\n"),
        )
        .unwrap();
    }
    commit(project.path(), "base");
    let family = "branch-switch-family";
    let initial = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        "main-view",
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    assert!(initial.published);
    assert_eq!(
        initial.generation.as_deref().unwrap().split('-').next(),
        Some("1")
    );

    let mut changed = BTreeSet::new();
    for index in 0..300 {
        let name = format!("file_{index}.rs");
        fs::write(
            project.path().join(&name),
            format!("pub fn value_{index}() -> usize {{ {} }}\n", index + 1),
        )
        .unwrap();
        changed.insert(name.into_bytes());
    }
    commit(project.path(), "branch change");
    let switched = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        "main-view",
        changed,
        true,
    ))
    .unwrap();
    assert!(switched.published);
    assert!(
        switched.blob_puts <= 300,
        "blob puts: {}",
        switched.blob_puts
    );
}

#[test]
fn borrow_only_view_never_puts_a_missing_shared_blob_and_reports_pending() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    fs::write(project.path().join("lib.rs"), "pub fn owner() {}\n").unwrap();
    commit(project.path(), "base");
    let family = "worktree-family";
    publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        "owner-view",
        BTreeSet::new(),
        true,
    ))
    .unwrap();

    fs::write(project.path().join("lib.rs"), "pub fn worktree_only() {}\n").unwrap();
    let report = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        "borrower-view",
        BTreeSet::from([b"lib.rs".to_vec()]),
        false,
    ))
    .unwrap();
    assert_eq!(report.blob_puts, 0);
    assert!(!report.published);
    assert!(report.pending_paths.contains(b"lib.rs".as_slice()));
}
