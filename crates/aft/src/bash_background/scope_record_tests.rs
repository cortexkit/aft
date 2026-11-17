use super::*;

fn record(store: &Path, session: &str, id: &str, terminal: bool) {
    let task = create_task_layout(store, session, id).unwrap();
    let mut metadata = PersistedTask::starting(
        id.into(),
        session.into(),
        "sleep 60".into(),
        store.into(),
        None,
        None,
        false,
        false,
    );
    if terminal {
        metadata.mark_terminal(
            crate::bash_background::BgTaskStatus::Completed,
            Some(0),
            None,
        );
    }
    write_task_at(&task, &metadata).unwrap();
}

#[test]
fn task_scope_record_scan_covers_sessions_and_harnesses() {
    let root = tempfile::tempdir().unwrap();
    let stores = [
        root.path().to_owned(),
        root.path().join("opencode"),
        root.path().join("pi"),
        root.path().join("runner"),
        root.path().join("mcp--fixture"),
        root.path().join("fed--fixture"),
    ];
    let mut expected = std::collections::BTreeSet::new();
    for (i, store) in stores.iter().enumerate() {
        for j in 0..2 {
            let id = format!("bash-{:016x}", expected.len() + 1);
            record(store, &format!("store-{i}-session-{j}"), &id, i != 1);
            expected.insert(id);
        }
        assert!(store.join("bash-tasks").is_dir());
    }
    let mut cursor = StorageTaskRecordCursor::new(root.path()).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut sessions = std::collections::BTreeSet::new();
    loop {
        let batch = discover_storage_task_records(root.path(), &mut cursor).unwrap();
        assert!(batch.entries_scanned <= 128 && batch.records.len() <= 128);
        for row in batch.records {
            sessions.insert(row.session_dir);
            assert!(seen.insert(row.task_id));
        }
        if batch.finished {
            break;
        }
    }
    assert_eq!(seen, expected);
    assert!(sessions
        .iter()
        .any(|path| path.starts_with(root.path().join("opencode/bash-tasks"))));
    let store = &stores[1];
    let mut cursor = StorageTaskRecordCursor::new(store).unwrap();
    let mut count = 0;
    loop {
        let batch = discover_storage_task_records(store, &mut cursor).unwrap();
        count += batch.records.len();
        if batch.finished {
            break;
        }
    }
    assert_eq!(
        count, 2,
        "harness root ascended outside its ownership boundary"
    );
}

#[test]
fn task_scope_record_scan_budget_resumes() {
    let root = tempfile::tempdir().unwrap();
    for i in 0..300 {
        record(root.path(), "budget", &format!("bash-{i:016x}"), true);
        std::fs::write(
            session_tasks_dir(root.path(), "budget").join(format!("invalid-{i}")),
            "",
        )
        .unwrap();
    }
    let mut cursor = StorageTaskRecordCursor::new(root.path()).unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut entries = 0;
    let mut calls = 0;
    loop {
        let batch = discover_storage_task_records(root.path(), &mut cursor).unwrap();
        calls += 1;
        assert!(batch.entries_scanned <= 128 && batch.records.len() <= 128);
        entries += batch.entries_scanned;
        for row in batch.records {
            assert!(seen.insert(row.task_id));
        }
        if batch.finished {
            break;
        }
    }
    assert_eq!(seen.len(), 300);
    assert!(
        entries >= 600 && calls >= 5,
        "invalid entries did not consume budget"
    );
}

#[test]
fn task_scope_record_scan_skips_oversized_body() {
    let root = tempfile::tempdir().unwrap();
    let id = "bash-0000000000000001";
    record(root.path(), "oversized", id, true);
    let task = resolve_task(root.path(), "oversized", id).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&task.paths.json)
        .unwrap()
        .write_all(&vec![b' '; 1_048_576])
        .unwrap();
    work_counts::reset();
    let mut cursor = StorageTaskRecordCursor::new(root.path()).unwrap();
    let mut rows = Vec::new();
    let mut oversized = 0;
    loop {
        let batch = discover_storage_task_records(root.path(), &mut cursor).unwrap();
        oversized += batch.skipped_oversized;
        rows.extend(batch.records);
        if batch.finished {
            break;
        }
    }
    assert!(
        rows.is_empty(),
        "oversized record body granted scope ownership"
    );
    assert_eq!(oversized, 1);
    assert!(cursor.rejected_task_ids.contains(id));
    assert_eq!(
        work_counts::get().metadata_reads,
        0,
        "oversized record body was read"
    );
}

#[test]
fn task_scope_record_scan_refuses_foreign_or_unreadable_records() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let corrupt = "bash-0000000000000001";
    let symlink = "bash-0000000000000002";
    let duplicate = "bash-0000000000000003";
    record(root.path(), "safe", corrupt, true);
    let task = resolve_task(root.path(), "safe", corrupt).unwrap();
    std::fs::write(&task.paths.json, "corrupt").unwrap();
    record(foreign.path(), "foreign", symlink, true);
    std::os::unix::fs::symlink(
        session_tasks_dir(foreign.path(), "foreign").join(symlink),
        session_tasks_dir(root.path(), "safe").join(symlink),
    )
    .unwrap();
    std::os::unix::fs::symlink(foreign.path(), root.path().join("opencode")).unwrap();
    record(root.path(), "safe", duplicate, true);
    record(root.path(), "other", duplicate, false);
    let mut cursor = StorageTaskRecordCursor::new(root.path()).unwrap();
    let mut rows = Vec::new();
    loop {
        let batch = discover_storage_task_records(root.path(), &mut cursor).unwrap();
        rows.extend(batch.records);
        if batch.finished {
            break;
        }
    }
    rows.retain(|row| !cursor.rejected_task_ids.contains(&row.task_id));
    assert!(rows.is_empty(), "unsafe record granted ownership");
    assert!(cursor.rejected_task_ids.contains(corrupt));
    assert!(cursor.rejected_task_ids.contains(duplicate));
}
