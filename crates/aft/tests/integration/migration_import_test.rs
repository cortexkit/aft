use std::fs;
use std::path::Path;

use aft::blob_store::{BlobPlane, BlobStore, SemanticKey};
use aft::migration::{
    import_legacy_semantic, rebuild_legacy_callgraph_once, CallgraphMigrationOutcome,
    SemanticMigrationOutcome, SemanticMigrationRequest,
};
use aft::views::{ManifestEntry, RelPath, ViewStore};

fn fingerprint() -> String {
    serde_json::json!({
        "backend": "test",
        "model": "counting",
        "base_url": "test",
        "dimension": 2,
        "chunking_version": 2,
    })
    .to_string()
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn write_legacy_snapshot(
    path: &Path,
    model_fingerprint: &str,
    rows: &[(&[u8], &[u8])],
    version: u8,
) {
    let mut bytes = Vec::new();
    bytes.push(version);
    if version != 6 && version != 7 {
        fs::write(path, bytes).unwrap();
        return;
    }
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    put_bytes(&mut bytes, model_fingerprint.as_bytes());
    bytes.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (legacy_path, source) in rows {
        put_bytes(&mut bytes, legacy_path);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(source.len() as u64).to_le_bytes());
        bytes.extend_from_slice(blake3::hash(source).as_bytes());
    }
    for (legacy_path, _source) in rows {
        put_bytes(&mut bytes, legacy_path);
        put_bytes(&mut bytes, b"entry");
        if version == 7 {
            put_bytes(&mut bytes, b"");
        }
        bytes.push(0);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(1);
        put_bytes(&mut bytes, b"fn entry() {}");
        put_bytes(&mut bytes, b"file:src/lib.rs kind:function name:entry");
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&0.0f32.to_le_bytes());
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn compatible_same_root_snapshot_publishes_a_view_without_reembedding() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let source = root.join("src/lib.rs");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "pub fn entry() {}\n").unwrap();
    let storage = temp.path().join("storage");
    let request = SemanticMigrationRequest::for_root(&storage, &root, fingerprint());
    write_legacy_snapshot(
        &request.legacy_semantic_path(),
        &request.configured_model_fingerprint,
        &[(b"src/lib.rs", b"pub fn entry() {}\n")],
        7,
    );

    let report = import_legacy_semantic(&request).unwrap();
    assert_eq!(report.outcome, SemanticMigrationOutcome::Imported);
    assert_eq!(report.imported_rows, 1);
    assert_eq!(report.reembedded_rows, 0);
    assert_eq!(report.skipped_rows, 0);
    assert_eq!(report.chunker_version, "semantic-v1");
    assert_eq!(report.embed_template_version, "semantic-v1");

    let key = SemanticKey::from_bytes(
        b"pub fn entry() {}\n",
        b"src/lib.rs",
        &request.chunker_version,
        &request.embed_template_version,
        &request.configured_model_fingerprint,
    )
    .full_key();
    let store = BlobStore::open(&storage, request.family.clone(), BlobPlane::Semantic).unwrap();
    assert!(store.get(&key).unwrap().is_some());

    let view = ViewStore::open(&storage, &request.view).unwrap();
    let generation = view.current_generation().unwrap().unwrap();
    let manifest = view.load_manifest(&generation).unwrap();
    assert!(matches!(
        manifest.get(&RelPath::new(b"src/lib.rs".to_vec()).unwrap()),
        Some(ManifestEntry::Regular { planes, .. }) if planes.semantic.as_deref() == Some(&key.to_hex())
    ));

    assert!(request.legacy_semantic_path().is_file());
    fs::remove_dir_all(view.view_dir()).unwrap();
    assert!(
        request.legacy_semantic_path().is_file(),
        "rollback deletes only the view; the legacy artifact remains available"
    );
}

#[test]
fn absolute_rows_are_relativized_per_row_and_external_rows_do_not_abort_import() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let source = root.join("src/in_root.rs");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "pub fn in_root() {}\n").unwrap();
    let outside = temp.path().join("outside.rs");
    fs::write(&outside, "pub fn outside() {}\n").unwrap();
    let storage = temp.path().join("storage");
    let request = SemanticMigrationRequest::for_root(&storage, &root, fingerprint());
    let source = fs::canonicalize(source).unwrap();
    let outside = fs::canonicalize(outside).unwrap();
    write_legacy_snapshot(
        &request.legacy_semantic_path(),
        &request.configured_model_fingerprint,
        &[
            (
                source.to_string_lossy().as_bytes(),
                b"pub fn in_root() {}\n",
            ),
            (
                outside.to_string_lossy().as_bytes(),
                b"pub fn outside() {}\n",
            ),
        ],
        7,
    );

    let report = import_legacy_semantic(&request).unwrap();
    assert_eq!(report.outcome, SemanticMigrationOutcome::Imported);
    assert_eq!(report.imported_rows, 1);
    assert_eq!(report.outside_root_rows, 1);
    assert_eq!(report.skipped_rows, 1);

    let view = ViewStore::open(&storage, &request.view).unwrap();
    let generation = view.current_generation().unwrap().unwrap();
    let manifest = view.load_manifest(&generation).unwrap();
    assert!(manifest
        .get(&RelPath::new(b"src/in_root.rs".to_vec()).unwrap())
        .is_some());
}

#[cfg(target_os = "linux")]
#[test]
fn absolute_non_utf8_legacy_path_keeps_its_exact_relative_bytes() {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path().join("root")).unwrap_or_else(|_| {
        let root = temp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::canonicalize(root).unwrap()
    });
    let source = root
        .join("src")
        .join(std::ffi::OsString::from_vec(vec![0xff, b'.', b'r', b's']));
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "pub fn byte_path() {}\n").unwrap();
    let storage = temp.path().join("storage");
    let request = SemanticMigrationRequest::for_root(&storage, &root, fingerprint());
    let absolute = source.as_os_str().as_bytes().to_vec();
    write_legacy_snapshot(
        &request.legacy_semantic_path(),
        &request.configured_model_fingerprint,
        &[(&absolute, b"pub fn byte_path() {}\n")],
        7,
    );

    let report = import_legacy_semantic(&request).unwrap();
    assert_eq!(report.imported_rows, 1);
    let view = ViewStore::open(&storage, &request.view).unwrap();
    let generation = view.current_generation().unwrap().unwrap();
    let manifest = view.load_manifest(&generation).unwrap();
    assert!(manifest
        .get(&RelPath::new(vec![b's', b'r', b'c', b'/', 0xff, b'.', b'r', b's']).unwrap())
        .is_some());
}

#[test]
fn unstamped_snapshot_requests_one_rebuild_instead_of_failing_the_migration() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let storage = temp.path().join("storage");
    let request = SemanticMigrationRequest::for_root(&storage, &root, fingerprint());
    fs::create_dir_all(request.legacy_semantic_path().parent().unwrap()).unwrap();
    write_legacy_snapshot(
        &request.legacy_semantic_path(),
        &request.configured_model_fingerprint,
        &[],
        5,
    );

    let first = import_legacy_semantic(&request).unwrap();
    assert!(matches!(
        first.outcome,
        SemanticMigrationOutcome::RebuildRequired { .. }
    ));
    let second = import_legacy_semantic(&request).unwrap();
    assert!(matches!(
        second.outcome,
        SemanticMigrationOutcome::RebuildAlreadyScheduled { .. }
    ));
}

#[test]
fn staged_callgraph_rebuild_runs_once_then_rebinds_the_ready_store() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn entry() {}\n").unwrap();
    let storage = temp.path().join("storage");
    let canonical_root = fs::canonicalize(&root).unwrap();
    let family = aft::search_index::artifact_cache_key(&canonical_root);
    aft::root_cache::configure_artifact_access(&canonical_root, &family, false);

    assert_eq!(
        rebuild_legacy_callgraph_once(&storage, &root, 1).unwrap(),
        CallgraphMigrationOutcome::Rebuilt
    );
    assert_eq!(
        rebuild_legacy_callgraph_once(&storage, &root, 1).unwrap(),
        CallgraphMigrationOutcome::AlreadyCurrent
    );
}
