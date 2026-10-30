use super::*;

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(files: usize) -> (tempfile::TempDir, AssemblyRequest) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    let request = AssemblyRequest {
        storage: dir.path().join("storage"),
        project_root: root.clone(),
        family: "bench".into(),
        scope: "bench".into(),
        desired_head: "bench".into(),
        changed_paths: BTreeSet::new(),
        semantic_keys: BTreeMap::new(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    let store = BlobStore::open(&request.storage, &request.family, BlobPlane::Callgraph).unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    // Seed membership in one transaction so fixture creation does not measure
    // per-file extraction or durable puts. Deferred publication never joins it.
    let payload = CallgraphBlob::extract(
        "export function f() {}",
        "typescript",
        super::super::callgraph::PRODUCER,
    )
    .unwrap()
    .to_bytes()
    .unwrap();
    let mut connection = Connection::open(path).unwrap();
    let tx = connection.transaction().unwrap();
    for file in 0..files {
        let directory = root.join(format!("d{:04}", file / 100));
        fs::create_dir_all(&directory).unwrap();
        let source = format!("export function f{file}() {{ return {file}; }}\n");
        fs::write(directory.join(format!("f{file:05}.ts")), &source).unwrap();
        let key = CallgraphKey::from_bytes(
            source.as_bytes(),
            "typescript",
            super::super::callgraph::PRODUCER,
        )
        .full_key();
        tx.execute("INSERT INTO blob_payloads(full_key, payload, payload_digest, payload_schema) VALUES(?1, ?2, ?3, 1)",
            rusqlite::params![key.as_bytes().as_slice(), &payload, blake3::hash(&payload).as_bytes().as_slice()]).unwrap();
    }
    tx.commit().unwrap();
    drop(connection);
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "fixture"]);
    let mut initial = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
    initial.commit().unwrap();
    initial.settle_status();
    (dir, request)
}

#[test]
fn views_warm_edit_avoids_manifest_decode_and_head_walk() {
    let (_dir, mut request) = fixture(16);
    super::super::take_manifest_decodes();
    let heads = crate::alias::head_tree_entry_calls_for_test();
    fs::write(
        request.project_root.join("d0000/f00000.ts"),
        "export function edited() {}\n",
    )
    .unwrap();
    request.changed_paths.insert(b"d0000/f00000.ts".to_vec());
    let _prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
    assert_eq!(
        super::super::take_manifest_decodes(),
        0,
        "warm edit decoded the previous manifest"
    );
    assert_eq!(
        crate::alias::head_tree_entry_calls_for_test(),
        heads,
        "warm edit relisted HEAD"
    );
}

#[test]
#[ignore = "5k/60k phase-timing benchmark; run explicitly with --ignored --nocapture"]
fn bench_views_one_file_publication() {
    for files in [5_000, 60_000] {
        let (_dir, mut request) = fixture(files);
        for edit in 0..6 {
            fs::write(
                request.project_root.join("d0000/f00000.ts"),
                format!("export function edited{edit}() {{ return {edit}; }}\n"),
            )
            .unwrap();
            request.changed_paths.insert(b"d0000/f00000.ts".to_vec());
            let mut prepared = prepare_checkout_deferring(&request, true, &mut |_| Ok(())).unwrap();
            assert!(prepared.commit().unwrap().published);
            prepared.settle_status();
            println!(
                "manifest_bench files={files} edit={edit} {}",
                publication_profile_line(&prepared.profile)
            );
        }
        let view = ViewStore::open(&request.storage, &request.scope).unwrap();
        let memory = manifest_incremental::cache_memory_for_test(view.view_dir());
        println!("manifest_cache_memory files={files} payload_bytes={} estimated_bytes={} metadata_size={} head_path_size={} json_bytes={} head_bytes={} path_index_bytes={}", memory.payload_bytes, memory.estimated_bytes, memory.metadata_size, memory.head_path_size, memory.json_bytes, memory.head_bytes, memory.path_index_bytes);
    }
}
