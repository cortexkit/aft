use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use aft::blob_store::{BlobPlane, BlobStore, SemanticKey};
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
        semantic_keys: Default::default(),
        require_semantic: false,
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
    assert_eq!(
        initial.blob_puts, 320,
        "every file is new content on the first publish"
    );

    // A real branch: 300 of the 320 files change content on it.
    git(project.path(), &["checkout", "--quiet", "-b", "feature"]);
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
        changed.clone(),
        true,
    ))
    .unwrap();
    assert!(switched.published);
    assert_eq!(
        switched.blob_puts, 300,
        "exactly the changed files are new content; the 20 untouched ones are reused"
    );

    // Switching back is the content-addressed claim: every blob for the base
    // tree already exists, so the publication must put nothing, whether the
    // watcher reports the 300 paths or (as an oversized batch) nothing at all.
    git(project.path(), &["checkout", "--quiet", "-"]);
    let back = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        "main-view",
        changed,
        true,
    ))
    .unwrap();
    assert!(back.published);
    assert_eq!(back.blob_puts, 0, "switching back re-derives nothing");
    let back_full = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        "main-view",
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    assert_eq!(
        back_full.blob_puts, 0,
        "a full rebuild over unchanged content puts nothing either"
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

#[test]
fn republishing_an_unchanged_checkout_keeps_the_current_generation() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    for index in 0..8 {
        fs::write(
            project.path().join(format!("file_{index}.rs")),
            format!("pub fn value_{index}() -> usize {{ {index} }}\n"),
        )
        .unwrap();
    }
    commit(project.path(), "base");
    let family = "unchanged-republish-family";
    let scope = "unchanged-view";
    let initial = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    assert!(initial.published);
    let generation = initial.generation.clone().expect("first generation");
    let view_dir = aft::views::ViewStore::open(storage.path(), scope)
        .unwrap()
        .view_dir()
        .to_path_buf();
    let manifest_count = || {
        fs::read_dir(&view_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("manifest-"))
            .count()
    };
    let manifests_after_first = manifest_count();

    // The semantic-ready trigger republishes with an empty changed set (a full
    // rebuild); with HEAD untouched it must not mint a second generation.
    let repeat = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    assert!(!repeat.published, "unchanged checkout must not republish");
    assert_eq!(repeat.generation.as_deref(), Some(generation.as_str()));
    assert_eq!(
        manifest_count(),
        manifests_after_first,
        "no new manifest file for an unchanged checkout"
    );
    assert_eq!(
        aft::views::ViewStore::open(storage.path(), scope)
            .unwrap()
            .current_generation()
            .unwrap()
            .as_deref(),
        Some(generation.as_str())
    );

    // A real content change still publishes a new generation.
    fs::write(
        project.path().join("file_0.rs"),
        "pub fn value_0() -> usize { 100 }\n",
    )
    .unwrap();
    commit(project.path(), "change");
    let changed = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::from([b"file_0.rs".to_vec()]),
        true,
    ))
    .unwrap();
    assert!(changed.published);
    assert_ne!(changed.generation.as_deref(), Some(generation.as_str()));
}

#[test]
fn changed_path_does_not_inherit_previous_semantic_blob() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    let rel_path = b"lib.rs".to_vec();
    let first_source = b"pub fn first() {}\n";
    fs::write(project.path().join("lib.rs"), first_source).unwrap();
    commit(project.path(), "first");

    let family = "semantic-switch-family";
    let semantic_key =
        SemanticKey::for_current(first_source, &rel_path, "fixture-model").full_key();
    BlobStore::open(storage.path(), family, BlobPlane::Semantic)
        .unwrap()
        .put(&semantic_key, b"fixture-vector")
        .unwrap();
    let mut initial = request(
        storage.path(),
        project.path(),
        family,
        "semantic-view",
        BTreeSet::new(),
        true,
    );
    initial.require_semantic = true;
    initial.semantic_keys = BTreeMap::from([(rel_path.clone(), semantic_key.to_hex())]);
    assert!(publish_checkout(&initial).unwrap().published);

    fs::write(project.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
    commit(project.path(), "second");
    let mut switched = request(
        storage.path(),
        project.path(),
        family,
        "semantic-view",
        BTreeSet::from([rel_path.clone()]),
        true,
    );
    switched.require_semantic = true;
    let report = publish_checkout(&switched).unwrap();
    assert!(!report.published);
    assert_eq!(report.pending_paths, BTreeSet::from([rel_path]));
}

#[cfg(unix)]
#[test]
fn publication_crash_child() {
    let Some(root) = std::env::var_os("AFT_VIEW_CRASH_ROOT") else {
        return;
    };
    let storage = std::env::var_os("AFT_VIEW_CRASH_STORAGE").unwrap();
    let root = std::path::PathBuf::from(root);
    let storage = std::path::PathBuf::from(storage);
    let _prepared = aft::views::assembly::prepare_checkout(
        &request(
            &storage,
            &root,
            "crash-family",
            "crash-view",
            BTreeSet::new(),
            true,
        ),
        &mut |phase| {
            if phase == "cas" {
                // The generation is durable but has not left the off-barrier
                // build. SIGKILL must not expose it through the current pointer.
                fs::write(storage.join("build-gated"), b"ready").unwrap();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
            Ok(())
        },
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn killed_off_barrier_build_preserves_pointer_and_sweeps_generation() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    fs::write(project.path().join("tracked.rs"), "pub fn previous() {}\n").unwrap();
    commit(project.path(), "previous");
    let initial = publish_checkout(&request(
        storage.path(),
        project.path(),
        "crash-family",
        "crash-view",
        BTreeSet::new(),
        true,
    ))
    .unwrap()
    .generation
    .unwrap();
    fs::write(project.path().join("tracked.rs"), "pub fn partial() {}\n").unwrap();
    commit(project.path(), "partial");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "view_assembly_wiring_test::publication_crash_child",
            "--nocapture",
        ])
        .env("AFT_VIEW_CRASH_ROOT", project.path())
        .env("AFT_VIEW_CRASH_STORAGE", storage.path())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !storage.path().join("build-gated").is_file() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not reach the off-barrier publication gate");
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before gate"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let view = aft::views::ViewStore::open(storage.path(), "crash-view").unwrap();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_eq!(
        view.current_generation().unwrap().as_deref(),
        Some(initial.as_str())
    );
    assert_eq!(
        view.sweep_generations().unwrap(),
        1,
        "one killed generation must be reclaimed"
    );
    for entry in fs::read_dir(view.view_dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.starts_with("derived-")
            || name.starts_with("trigram-")
            || name.starts_with("manifest-")
        {
            assert!(name.contains(&initial), "orphan generation file: {name}");
        }
    }
    assert!(view.derived_path(&initial).unwrap().is_file());
    assert!(view.load_manifest(&initial).is_ok());
}
