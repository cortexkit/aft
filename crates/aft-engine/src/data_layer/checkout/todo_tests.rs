use super::*;

fn fixture(files: usize) -> (tempfile::TempDir, Checkout, Vec<Vec<u8>>) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let paths = (0..files)
        .map(|index| {
            let path = format!("file_{index:05}.rs");
            std::fs::write(
                root.join(&path),
                format!("// TODO: marker {index}\npub const VALUE: usize = {index};\n"),
            )
            .unwrap();
            path.into_bytes()
        })
        .collect::<Vec<_>>();
    let checkout = Checkout::bind(
        Arc::new(DataLayer::new(16 << 20, 2)),
        &temp.path().join("store"),
        FamilyId::from_digest(Digest128::of(b"todo-counts")),
        &root,
    )
    .unwrap();
    checkout.build().unwrap();
    (temp, checkout, paths)
}

#[test]
fn todo_count_one_edit_decodes_one_record_not_unchanged_files() {
    let (_temp, checkout, paths) = fixture(129);
    let limit = crate::inspect::scanners::source_text::MAX_SOURCE_BYTES;
    checkout
        .todo_counts(&checkout.view().unwrap(), &paths, limit)
        .unwrap();
    std::fs::write(
        checkout.root.join("file_00064.rs"),
        "// FIXME: edited\npub const VALUE: usize = 9000;\n",
    )
    .unwrap();
    checkout.record_paths(&[paths[64].clone()]).unwrap();
    let before = source_work_for_test(checkout.root()).todo_decodes;
    checkout
        .todo_counts(&checkout.view().unwrap(), &paths, limit)
        .unwrap();
    assert_eq!(
        source_work_for_test(checkout.root()).todo_decodes - before,
        1
    );
    let before = source_work_for_test(checkout.root()).todo_decodes;
    checkout.refresh_callgraph().unwrap();
    checkout.publish_changes().unwrap();
    checkout
        .todo_counts(&checkout.view().unwrap(), &paths, limit)
        .unwrap();
    assert_eq!(
        source_work_for_test(checkout.root()).todo_decodes - before,
        0,
        "publication reuses unchanged record keys, including the edit"
    );
    let before = source_work_for_test(checkout.root()).todo_decodes;
    checkout
        .todo_markers(&checkout.view().unwrap(), &paths, limit)
        .unwrap();
    assert_eq!(
        source_work_for_test(checkout.root()).todo_decodes - before,
        paths.len(),
        "the uncached path used before the memo really decodes every record"
    );
}

#[test]
fn todo_count_cache_does_not_hide_a_missing_overlay_or_size_limit() {
    let (_temp, checkout, paths) = fixture(2);
    let limit = crate::inspect::scanners::source_text::MAX_SOURCE_BYTES;
    std::fs::write(checkout.root.join("file_00000.rs"), "// BUG: first edit\n").unwrap();
    checkout.record_paths(&[paths[0].clone()]).unwrap();
    let old_view = checkout.view().unwrap();
    checkout.todo_counts(&old_view, &paths, limit).unwrap();
    std::fs::write(
        checkout.root.join("file_00000.rs"),
        "// FIXME: a different edit\n",
    )
    .unwrap();
    checkout.record_paths(&[paths[0].clone()]).unwrap();
    assert!(matches!(
        checkout.todo_counts(&old_view, &paths, limit).unwrap()[0],
        PathTodoCounts::Missing
    ));
    assert!(matches!(
        checkout
            .todo_counts(&checkout.view().unwrap(), &paths, 1)
            .unwrap()[0],
        PathTodoCounts::TooLarge
    ));
    std::fs::remove_file(checkout.root.join("file_00000.rs")).unwrap();
    checkout.record_paths(&[paths[0].clone()]).unwrap();
    assert!(matches!(
        checkout
            .todo_counts(&checkout.view().unwrap(), &paths, limit)
            .unwrap()[0],
        PathTodoCounts::Absent
    ));
}

#[test]
fn todo_count_memo_bounds_entries_and_path_storage() {
    let (_temp, checkout, paths) = fixture(1);
    let view = checkout.view().unwrap();
    let record = view.current_state().get(&paths[0]).unwrap();
    let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
    let mut memo = TodoCountMemo::default();
    for index in 0..(TodoCountMemo::MAX_ENTRIES + 2) {
        let mut path = vec![b'x'; 64];
        path[..std::mem::size_of::<usize>()].copy_from_slice(&index.to_le_bytes());
        memo.insert(&path, key, PathTodoCounts::Recorded(None));
        assert!(memo.entries.len() <= TodoCountMemo::MAX_ENTRIES);
        assert!(memo.path_bytes <= TodoCountMemo::MAX_PATH_BYTES);
        if index + 1 == TodoCountMemo::MAX_ENTRIES {
            use aft_store::heap::HeapBytes as _;
            eprintln!(
                "TODO_MEMO_BOUND entries={} path_bytes={} heap_bytes={}",
                memo.entries.len(),
                memo.path_bytes,
                memo.heap_bytes(&mut aft_store::heap::Seen::new())
            );
        }
    }
    let path = vec![b'x'; TodoCountMemo::MAX_PATH_BYTES];
    memo.insert(&path, key, PathTodoCounts::Recorded(None));
    assert_eq!(memo.entries.len(), 1);
    memo.insert(b"another", key, PathTodoCounts::Recorded(None));
    assert_eq!(memo.path_bytes, 7);
    memo.insert(
        &vec![b'x'; TodoCountMemo::MAX_PATH_BYTES + 1],
        key,
        PathTodoCounts::Recorded(None),
    );
    assert_eq!(memo.entries.len(), 1);
}
