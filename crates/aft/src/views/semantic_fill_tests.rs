use super::*;

#[test]
fn semantic_fill_has_no_derived_writes_or_checkpoint_and_reader_uses_owner() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let source = b"pub fn example() {}\n";
    fs::write(project.path().join("lib.rs"), source).unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "base",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(project.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let mut request = AssemblyRequest {
        storage: storage.path().to_path_buf(),
        project_root: project.path().to_path_buf(),
        family: "fill-family".into(),
        scope: "fill-scope".into(),
        desired_head: "head".into(),
        changed_paths: BTreeSet::from([b"lib.rs".to_vec()]),
        semantic_keys: Default::default(),
        require_semantic: true,
        allow_blob_put: true,
    };
    let initial = publish_checkout(&request).unwrap();
    assert!(initial.published);
    assert_eq!(initial.pending_paths, BTreeSet::from([b"lib.rs".to_vec()]));
    let view = ViewStore::open(storage.path(), &request.scope).unwrap();
    let owner_path = view
        .derived_path(initial.generation.as_deref().unwrap())
        .unwrap();
    let connection = Connection::open(&owner_path).unwrap();
    connection.execute_batch("CREATE TRIGGER no_metadata_writes BEFORE INSERT ON meta BEGIN SELECT RAISE(ABORT, 'derived metadata rewritten'); END;").unwrap();
    drop(connection);
    let key =
        crate::blob_store::SemanticKey::for_current(source, b"lib.rs", "fixture-model").full_key();
    crate::blob_store::BlobStore::open(
        storage.path(),
        &request.family,
        crate::blob_store::BlobPlane::Semantic,
    )
    .unwrap()
    .put(&key, b"vector")
    .unwrap();
    request
        .semantic_keys
        .insert(b"lib.rs".to_vec(), key.to_hex());
    let mut prepared = prepare_checkout(&request, &mut |_| Ok(())).unwrap();
    assert!(
        prepared.derived_checkpoint.is_none(),
        "semantic fill scheduled a callgraph checkpoint"
    );
    assert_eq!(prepared.profile.materialization_call_ms, 0);
    let fill = prepared.commit().unwrap();
    assert!(fill.published);
    assert!(fill.pending_paths.is_empty());
    let reader = crate::callgraph_store::ReadonlyCallGraphStore::open_manifest_view(
        project.path().to_path_buf(),
        request.family,
        view.view_dir().to_path_buf(),
        fill.generation.as_deref().unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(reader.sqlite_path(), owner_path);
}

#[test]
fn prepared_callgraph_retains_committed_wal_before_pointer_publication() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    fs::write(project.path().join("lib.rs"), "pub fn retained() {}\n").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(project.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let request = AssemblyRequest {
        storage: storage.path().to_path_buf(),
        project_root: project.path().to_path_buf(),
        family: "wal-family".into(),
        scope: "wal-scope".into(),
        desired_head: "head".into(),
        changed_paths: BTreeSet::new(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
    };
    let prepared = prepare_checkout(&request, &mut |_| Ok(())).unwrap();
    let (path, _) = prepared
        .derived_checkpoint
        .as_ref()
        .expect("graph checkpoint keeper");
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(
        fs::metadata(&wal).is_ok_and(|metadata| metadata.len() > 32),
        "committed graph WAL was checkpointed on writer close before pointer publication"
    );
}
