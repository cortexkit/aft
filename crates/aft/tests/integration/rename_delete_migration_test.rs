use aft::blob_store::{BlobPlane, BlobStore, CallgraphKey, PutOutcome, SemanticKey};

#[test]
fn rename_changes_only_the_semantic_key_and_manifest_removal_does_not_delete_blobs() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path();
    let family = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let source = b"pub fn preserved() {}\n";

    let old_semantic = SemanticKey::for_current(source, b"src/old.rs", "model").full_key();
    let renamed_semantic = SemanticKey::for_current(source, b"src/new.rs", "model").full_key();
    assert_ne!(old_semantic, renamed_semantic);

    let callgraph = CallgraphKey::for_current(source, "rust").full_key();
    let mut semantic = BlobStore::open(storage, family, BlobPlane::Semantic).unwrap();
    let mut callgraphs = BlobStore::open(storage, family, BlobPlane::Callgraph).unwrap();
    assert_eq!(
        semantic.put(&old_semantic, b"old vector").unwrap().outcome,
        PutOutcome::Inserted
    );
    assert_eq!(
        semantic
            .put(&renamed_semantic, b"renamed vector")
            .unwrap()
            .outcome,
        PutOutcome::Inserted,
        "the renamed path has one new semantic key for its re-embed"
    );
    assert_eq!(
        callgraphs.put(&callgraph, b"parse").unwrap().outcome,
        PutOutcome::Inserted
    );
    assert_eq!(
        callgraphs.put(&callgraph, b"parse").unwrap().outcome,
        PutOutcome::Reused,
        "callgraph identity excludes rel_path"
    );

    // Removing the old manifest member is intentionally not a blob-store delete.
    // A later mark-and-sweep owns reclamation after it sees no retained reference.
    assert!(semantic.get(&old_semantic).unwrap().is_some());
    assert_eq!(semantic.usage().unwrap().rows, 2);
}
