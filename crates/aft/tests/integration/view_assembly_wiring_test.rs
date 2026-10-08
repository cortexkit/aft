use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use aft::blob_store::{BlobPlane, BlobStore, SemanticKey};
use aft::views::assembly::{
    head_tree_fingerprint, prepare_checkout, publish_checkout, AssemblyRequest,
};
use aft::views::{ManifestEntry, RelPath, ViewStore};
use rusqlite::Connection;
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
        callgraph: true,
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

    // The trigger is copied with the base database. A full rewrite on publication
    // would delete this unchanged file and fail instead of hiding extra work.
    let view = aft::views::ViewStore::open(storage.path(), "main-view").unwrap();
    let database = view
        .derived_path(initial.generation.as_deref().unwrap())
        .unwrap();
    rusqlite::Connection::open(database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER preserve_untouched_file BEFORE DELETE ON files
         WHEN old.path = 'file_319.rs'
         BEGIN SELECT RAISE(ABORT, 'unchanged file rewritten'); END;",
        )
        .unwrap();

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
fn abandoned_derived_builds_release_keepers_before_cleanup() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    fs::write(project.path().join("lib.rs"), "pub fn base() {}\n").unwrap();
    commit(project.path(), "base");
    let family = "abandoned-build-family";
    let scope = "abandoned-build-view";
    let initial = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    let view = ViewStore::open(storage.path(), scope).unwrap();
    fs::write(project.path().join("lib.rs"), "pub fn changed() {}\n").unwrap();
    commit(project.path(), "change");
    let changed = request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::from([b"lib.rs".to_vec()]),
        true,
    );
    let assert_removed = |generation: &str| {
        for name in [
            format!("derived-{generation}.sqlite"),
            format!("derived-{generation}.sqlite-wal"),
            format!("derived-{generation}.sqlite-shm"),
            format!("manifest-{generation}.json"),
            format!("trigram-{generation}.bin"),
        ] {
            assert!(
                !view.view_dir().join(&name).exists(),
                "abandoned file remains: {name}"
            );
        }
    };

    // Cancel after materialization and durable manifest preparation, while the
    // assembler still owns its checkpoint keeper. No sweep should be needed.
    let prepared = prepare_checkout(&changed, &mut |_| Ok(())).unwrap();
    let cancelled = prepared.report().generation.clone().unwrap();
    assert!(view.derived_path(&cancelled).unwrap().is_file());
    assert!(view.manifest_path(&cancelled).unwrap().is_file());
    drop(prepared);
    assert_removed(&cancelled);
    assert_eq!(view.current_generation().unwrap(), initial.generation);

    // Two identical builds from the same base get distinct names. A CAS loser
    // must not leave a second full database even though the winner stays live.
    let mut winner = prepare_checkout(&changed, &mut |_| Ok(())).unwrap();
    let mut loser = prepare_checkout(&changed, &mut |_| Ok(())).unwrap();
    let losing = loser.report().generation.clone().unwrap();
    let winning = winner.commit().unwrap().generation.unwrap();
    assert!(loser.commit().is_err());
    drop(loser);
    assert_removed(&losing);
    assert_eq!(
        view.current_generation().unwrap().as_deref(),
        Some(winning.as_str())
    );
    assert!(view.derived_path(&winning).unwrap().is_file());

    // A callback error at the last off-barrier phase takes the same Drop path.
    fs::write(project.path().join("lib.rs"), "pub fn next() {}\n").unwrap();
    commit(project.path(), "next");
    let next = request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::new(),
        true,
    );
    let mut failed_generation = None;
    let result = prepare_checkout(&next, &mut |phase| {
        if phase == "cas" {
            failed_generation = fs::read_dir(view.view_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .find_map(|name| {
                    name.strip_prefix("manifest-3-")
                        .and_then(|_| name.strip_prefix("manifest-"))
                        .and_then(|name| name.strip_suffix(".json"))
                        .map(str::to_owned)
                });
            return Err(aft::views::ViewError::InvalidManifest(
                "cancel after preparation".into(),
            ));
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_removed(&failed_generation.expect("callback reached durable generation"));
    assert_eq!(
        view.current_generation().unwrap().as_deref(),
        Some(winning.as_str())
    );
}

#[test]
fn abandoned_derived_build_keeps_a_live_query_pin() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    fs::write(project.path().join("lib.rs"), "pub fn reader() {}\n").unwrap();
    commit(project.path(), "base");
    let prepared = prepare_checkout(
        &request(
            storage.path(),
            project.path(),
            "pinned-build-family",
            "pinned-build-view",
            BTreeSet::new(),
            true,
        ),
        &mut |_| Ok(()),
    )
    .unwrap();
    let generation = prepared.report().generation.clone().unwrap();
    let view = ViewStore::open(storage.path(), "pinned-build-view").unwrap();
    let reader = aft::pins::QueryPin::acquire(view.view_dir(), &generation).unwrap();
    drop(prepared);
    assert!(view.derived_path(&generation).unwrap().is_file());
    assert!(view.manifest_path(&generation).unwrap().is_file());
    drop(reader);
    assert_eq!(view.sweep_generations().unwrap(), 1);
    assert!(!view.derived_path(&generation).unwrap().exists());
}

#[test]
fn semantic_plane_follows_an_immediately_published_callgraph_plane() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    let rel_path = b"lib.rs".to_vec();
    let first_source = b"pub fn first() {}\n";
    fs::write(project.path().join("lib.rs"), first_source).unwrap();
    commit(project.path(), "first");

    let family = "semantic-switch-family";
    let scope = "semantic-view";
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
        scope,
        BTreeSet::new(),
        true,
    );
    initial.require_semantic = true;
    initial.semantic_keys = BTreeMap::from([(rel_path.clone(), semantic_key.to_hex())]);
    assert!(publish_checkout(&initial).unwrap().published);

    let second_source = b"pub fn second() {}\npub fn caller() { second(); }\n";
    fs::write(project.path().join("lib.rs"), second_source).unwrap();
    commit(project.path(), "second");
    let mut switched = request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::from([rel_path.clone()]),
        true,
    );
    switched.require_semantic = true;
    let report = publish_checkout(&switched).unwrap();
    assert!(report.published);
    assert_eq!(report.pending_paths, BTreeSet::from([rel_path.clone()]));
    let manifest = report.manifest.unwrap();
    let entry = manifest
        .get(&RelPath::new(rel_path.clone()).unwrap())
        .unwrap();
    assert!(matches!(
        entry,
        ManifestEntry::Regular { planes, .. }
            if planes.callgraph.is_some() && planes.semantic.is_none()
    ));

    let view = ViewStore::open(storage.path(), scope).unwrap();
    let callgraph_generation = report.generation.unwrap();
    let callgraph_db = view.derived_path(&callgraph_generation).unwrap();
    let connection = Connection::open(&callgraph_db).unwrap();
    let node_count: usize = connection
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE name = 'second'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        node_count, 1,
        "the callgraph plane is queryable while semantic is pending"
    );
    connection
        .execute_batch(
            "CREATE TRIGGER forbid_callgraph_rewrite BEFORE DELETE ON nodes BEGIN SELECT RAISE(ABORT, 'callgraph plane was rewritten'); END;",
        )
        .unwrap();
    drop(connection);

    let second_semantic_key =
        SemanticKey::for_current(second_source, &rel_path, "fixture-model").full_key();
    BlobStore::open(storage.path(), family, BlobPlane::Semantic)
        .unwrap()
        .put(&second_semantic_key, b"second-vector")
        .unwrap();
    switched.semantic_keys = BTreeMap::from([(rel_path, second_semantic_key.to_hex())]);
    let semantic_report = publish_checkout(&switched).unwrap();
    assert!(semantic_report.published);
    assert!(semantic_report.pending_paths.is_empty());
    assert_eq!(semantic_report.blob_puts, 0);
    let fill_generation = semantic_report.generation.unwrap();
    assert_eq!(
        view.derived_path(&fill_generation).unwrap(),
        callgraph_db,
        "semantic fill must reuse the durable callgraph database, not clone or checkpoint it"
    );
    view.sweep_generations().unwrap();
    assert!(
        callgraph_db.is_file(),
        "the fill pins its shared derived owner"
    );
    Connection::open(&callgraph_db)
        .unwrap()
        .execute_batch("DROP TRIGGER forbid_callgraph_rewrite")
        .unwrap();
    fs::write(project.path().join("lib.rs"), "pub fn third() {}\n").unwrap();
    commit(project.path(), "third");
    let next = publish_checkout(&request(
        storage.path(),
        project.path(),
        family,
        scope,
        BTreeSet::from([b"lib.rs".to_vec()]),
        true,
    ))
    .unwrap();
    assert!(
        next.published,
        "a graph edit after a fill must accept the reused base"
    );
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

/// With the call graph off, a publication extracts no callgraph payload and
/// its source entries carry no callgraph key; once the call graph is turned
/// on, a later publication that changes other files still extracts the
/// unchanged ones it never extracted.
#[test]
fn callgraph_off_publication_extracts_nothing_and_on_fills_it_in() {
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    for index in 0..5 {
        fs::write(
            project.path().join(format!("file_{index}.rs")),
            format!("pub fn value_{index}() -> usize {{ {index} }}\n"),
        )
        .unwrap();
    }
    commit(project.path(), "base");
    let family = "callgraph-off-family";
    let mut off = request(
        storage.path(),
        project.path(),
        family,
        "off-view",
        BTreeSet::new(),
        true,
    );
    off.callgraph = false;
    let report = publish_checkout(&off).unwrap();
    assert!(report.published);
    assert_eq!(report.blob_puts, 0, "no callgraph payload is extracted");
    let manifest = report.manifest.unwrap();
    let callgraph_keys = |manifest: &aft::views::Manifest| {
        manifest
            .entries()
            .filter(|(_, entry)| {
                matches!(entry, ManifestEntry::Regular { planes, .. } if planes.callgraph.is_some())
            })
            .count()
    };
    assert_eq!(callgraph_keys(&manifest), 0);

    fs::write(
        project.path().join("file_0.rs"),
        "pub fn value_0() -> usize { 100 }\n",
    )
    .unwrap();
    commit(project.path(), "edit");
    let on = request(
        storage.path(),
        project.path(),
        family,
        "off-view",
        BTreeSet::from([b"file_0.rs".to_vec()]),
        true,
    );
    let report = publish_checkout(&on).unwrap();
    assert!(report.published);
    assert_eq!(
        report.blob_puts, 5,
        "the four unchanged files are extracted too"
    );
    assert_eq!(callgraph_keys(&report.manifest.unwrap()), 5);
}

#[test]
#[ignore = "publication profile over a complete worker checkout; run explicitly"]
fn worker_checkout_publication_reuses_blobs_across_linked_worktrees() {
    use std::io::Write;
    use std::process::Stdio;
    use std::time::Instant;
    let project = tempdir().unwrap();
    let storage = tempdir().unwrap();
    let linked = tempdir().unwrap();
    let archive = Command::new("git")
        .args(["archive", "HEAD"])
        .output()
        .unwrap();
    assert!(archive.status.success());
    let mut unpack = Command::new("tar")
        .arg("-x")
        .current_dir(project.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    unpack
        .stdin
        .take()
        .unwrap()
        .write_all(&archive.stdout)
        .unwrap();
    assert!(unpack.wait().unwrap().success());
    git(project.path(), &["init", "--quiet"]);
    commit(project.path(), "worker checkout profile");
    git(
        project.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.path().to_str().unwrap(),
            "HEAD",
        ],
    );
    let family_a = aft::search_index::artifact_cache_key(project.path());
    let family_b = aft::search_index::artifact_cache_key(linked.path());
    assert_eq!(
        family_a, family_b,
        "linked checkouts must share the repository family"
    );
    let started = Instant::now();
    let first = publish_checkout(&request(
        storage.path(),
        project.path(),
        &family_a,
        "profile-first",
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    let first_elapsed = started.elapsed();
    let started = Instant::now();
    let second = publish_checkout(&request(
        storage.path(),
        linked.path(),
        &family_b,
        "profile-second",
        BTreeSet::new(),
        true,
    ))
    .unwrap();
    eprintln!(
        "worker publication: first={first_elapsed:?} puts={} second={:?} puts={}",
        first.blob_puts,
        started.elapsed(),
        second.blob_puts
    );
    assert!(first.published && second.published);
    assert!(first.blob_puts > 0);
    assert_eq!(
        second.blob_puts, 0,
        "same-commit linked checkout must build no new payloads"
    );
}
