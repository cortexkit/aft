use super::*;

fn metadata(task: &ResolvedTask) -> PersistedTask {
    PersistedTask::starting(
        task.paths.task_id.clone(),
        "metadata-persistence".into(),
        "synthetic task".into(),
        task.paths.dir.clone(),
        None,
        None,
        true,
        false,
    )
}

#[test]
fn metadata_persistence_db_row_resolves_both_streams_once() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let metadata = metadata(&task);
    write_task_at(&task, &metadata).unwrap();
    fs::write(&task.paths.stdout, b"stdout").unwrap();
    fs::write(&task.paths.stderr, b"err").unwrap();
    work_counts::reset();
    let row = metadata.to_bash_task_row("runner", &task.paths).unwrap();
    let counts = work_counts::get();
    assert_eq!(row.output_bytes, Some(9));
    assert_eq!(counts.metadata_reads, 1, "one layout verification per row");
    assert_eq!(
        counts.parses, 1,
        "one typed decode supplies the version too"
    );
    assert_eq!(counts.artifact_opens, 2, "both output files are sampled");
}

#[test]
fn metadata_persistence_read_decodes_task_once() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let metadata = metadata(&task);
    write_task_at(&task, &metadata).unwrap();
    work_counts::reset();
    let saved = read_task_at(&task).unwrap();
    assert_eq!(saved.task_id, metadata.task_id);
    assert_eq!(saved.command, "synthetic task");
    assert_eq!(work_counts::get().metadata_reads, 1);
    assert_eq!(work_counts::get().parses, 1, "no intermediate Value tree");
}

#[test]
fn metadata_persistence_future_shape_is_refused_before_decode_error() {
    let storage = tempfile::tempdir().unwrap();
    for version in [u64::from(SCHEMA_VERSION) + 1, u64::MAX] {
        let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
        // Deliberately omit today's required fields: an unknown future shape
        // must retain the unsupported-format error that prevents quarantine.
        fs::write(
            &task.paths.json,
            format!(r#"{{"schema_version":{version}}}"#),
        )
        .unwrap();
        let error = read_task_at(&task).unwrap_err();
        assert!(
            crate::persisted_format::UnsupportedPersistedFormat::from_io_error(&error).is_some(),
            "future schema {version} was treated as corrupt: {error}"
        );
    }
}

#[test]
fn metadata_persistence_current_shape_errors_remain_decode_errors() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let valid = serde_json::to_string(&metadata(&task)).unwrap();
    for content in [
        format!(r#"{{"schema_version":{SCHEMA_VERSION}}}"#),
        valid.replacen("{", &format!(r#"{{"schema_version":{SCHEMA_VERSION},"#), 1),
        "{broken".into(),
    ] {
        fs::write(&task.paths.json, &content).unwrap();
        let expected = serde_json::from_str::<PersistedTask>(&content).unwrap_err();
        let error = read_task_at(&task).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), expected.to_string());
        assert!(
            crate::persisted_format::UnsupportedPersistedFormat::from_io_error(&error).is_none()
        );
    }
}

#[test]
fn metadata_persistence_equal_bytes_skip_replacement_and_changed_bytes_write() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let mut metadata = metadata(&task);
    write_task_at(&task, &metadata).unwrap();
    work_counts::reset();
    write_task_at(&task, &metadata).unwrap();
    update_task_at(&task, |_| {}).unwrap();
    assert_eq!(work_counts::get().metadata_writes, 0);
    assert_eq!(work_counts::get().renames, 0);
    assert_eq!(work_counts::get().syncs, 0);

    metadata.command.push_str(" changed");
    write_task_at(&task, &metadata).unwrap();
    assert_eq!(work_counts::get().metadata_writes, 1);
    assert_eq!(work_counts::get().renames, 1);
    assert_eq!(read_task_at(&task).unwrap().command, metadata.command);

    // Compare on-disk bytes even when another writer replaced the record.
    let mut foreign = metadata.clone();
    foreign.command.push_str(" by another writer");
    write_task_at(&task, &foreign).unwrap();
    work_counts::reset();
    write_task_at(&task, &metadata).unwrap();
    assert_eq!(work_counts::get().renames, 1);
    assert_eq!(read_task_at(&task).unwrap().command, metadata.command);
}

#[test]
fn metadata_persistence_equal_checkpoint_keeps_durability_upgrade() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let metadata = metadata(&task);
    let _owner =
        super::super::registry::DbWriteOrder::for_task(&metadata.session_id, &metadata.task_id);
    write_task_at_with(&task, &metadata, ReplaceSync::File).unwrap();
    work_counts::reset();
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    let counts = work_counts::get();
    assert_eq!(counts.metadata_writes, 0);
    assert_eq!(counts.renames, 0);
    assert_eq!(
        counts.syncs, 2,
        "equal bytes must not elide a durability upgrade"
    );
    work_counts::reset();
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    assert_eq!(work_counts::get().syncs, 0);
}

#[test]
fn metadata_persistence_changed_checkpoint_invalidates_durable_memo() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let mut metadata = metadata(&task);
    let _owner =
        super::super::registry::DbWriteOrder::for_task(&metadata.session_id, &metadata.task_id);
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    let original = metadata.clone();
    metadata.command.push_str(" checkpoint");
    write_task_at_with(&task, &metadata, ReplaceSync::File).unwrap();
    write_task_at_with(&task, &original, ReplaceSync::File).unwrap();
    work_counts::reset();
    write_task_at_with(&task, &original, ReplaceSync::FileAndDirectory).unwrap();
    assert_eq!(work_counts::get().syncs, 2);
    assert_eq!(work_counts::get().renames, 0);
}

#[test]
fn metadata_persistence_durable_memo_observes_other_writers_and_directories() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let metadata = metadata(&task);
    let _owner =
        super::super::registry::DbWriteOrder::for_task(&metadata.session_id, &metadata.task_id);
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    let mut acknowledged = metadata.clone();
    acknowledged.completion_delivered = true;
    write_task_at_with(&task, &acknowledged, ReplaceSync::FileAndDirectory).unwrap();
    work_counts::reset();
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    assert_eq!(work_counts::get().renames, 1);
    assert_eq!(work_counts::get().syncs, 2);

    let other_storage = tempfile::tempdir().unwrap();
    let other = create_task_layout(
        other_storage.path(),
        &metadata.session_id,
        &metadata.task_id,
    )
    .unwrap();
    // An equal file in a different store has not inherited this directory's sync.
    write_task_at_with(&other, &metadata, ReplaceSync::File).unwrap();
    work_counts::reset();
    write_task_at_with(&other, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    assert_eq!(work_counts::get().renames, 0);
    assert_eq!(work_counts::get().syncs, 2);
}

#[test]
fn metadata_persistence_failed_directory_sync_does_not_seed_memo() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let mut metadata = metadata(&task);
    let owner =
        super::super::registry::DbWriteOrder::for_task(&metadata.session_id, &metadata.task_id);
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    metadata.command.push_str(" changed");
    with_task_io_fault(TaskIoFault::SyncFailureAfter(1), || {
        write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory)
    })
    .unwrap_err();
    work_counts::reset();
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    assert_eq!(
        work_counts::get().syncs,
        2,
        "retry must finish the failed directory sync"
    );
    assert_eq!(work_counts::get().renames, 0);
    drop(owner);
    work_counts::reset();
    write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory).unwrap();
    assert_eq!(
        work_counts::get().syncs,
        2,
        "an unowned record has no durability memo"
    );
}

#[test]
fn metadata_persistence_shortcut_sync_failure_retries_durable_replacement() {
    for failed_sync in [0, 1] {
        let storage = tempfile::tempdir().unwrap();
        let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
        let metadata = metadata(&task);
        let _owner =
            super::super::registry::DbWriteOrder::for_task(&metadata.session_id, &metadata.task_id);
        write_task_at_with(&task, &metadata, ReplaceSync::File).unwrap();
        work_counts::reset();
        with_task_io_fault(TaskIoFault::SyncFailureAfter(failed_sync), || {
            write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory)
        })
        .unwrap();
        let counts = work_counts::get();
        assert_eq!(counts.metadata_writes, 1);
        assert_eq!(counts.renames, 1);
        // The shortcut reaches the injected file/directory failure, then the
        // replacement must successfully sync both its new file and directory.
        assert_eq!(counts.syncs, failed_sync + 3);
        assert_eq!(
            fs::read(&task.paths.json).unwrap(),
            serde_json::to_vec_pretty(&metadata).unwrap()
        );
    }
}

#[test]
fn metadata_persistence_replacement_sync_failure_preserves_old_record() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let mut metadata = metadata(&task);
    write_task_at_with(&task, &metadata, ReplaceSync::File).unwrap();
    let old = fs::read(&task.paths.json).unwrap();
    metadata.command.push_str(" changed");
    work_counts::reset();
    let error = with_task_io_fault(TaskIoFault::SyncFailureAfter(0), || {
        write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory)
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(work_counts::get().metadata_writes, 1);
    assert_eq!(work_counts::get().syncs, 1);
    assert_eq!(work_counts::get().renames, 0);
    assert_eq!(fs::read(&task.paths.json).unwrap(), old);
}

#[cfg(windows)]
#[test]
fn metadata_persistence_directory_sync_refuses_replaced_pinned_directory() {
    let storage = tempfile::tempdir().unwrap();
    let path = storage.path().join("pinned");
    fs::create_dir(&path).unwrap();
    let dir = PinnedDir::open(&path).unwrap();
    fs::rename(&path, storage.path().join("moved")).unwrap();
    fs::create_dir(&path).unwrap();
    work_counts::reset();
    let error = sync_task_directory(&dir).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(work_counts::get().syncs, 0);
}

#[cfg(unix)]
#[test]
fn metadata_persistence_compare_open_failure_falls_back_to_durable_replacement() {
    for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Interrupted] {
        let storage = tempfile::tempdir().unwrap();
        let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
        let metadata = metadata(&task);
        write_task_at(&task, &metadata).unwrap();
        let opens = std::rc::Rc::new(std::cell::Cell::new(0));
        let injected = opens.clone();
        work_counts::reset();
        with_openat_file_hook(
            Box::new(move |_, name, _| {
                if name != OsStr::new(METADATA_FILE) {
                    return None;
                }
                injected.set(injected.get() + 1);
                Some(Err(io::Error::new(
                    kind,
                    "injected comparison open failure",
                )))
            }),
            || write_task_at_with(&task, &metadata, ReplaceSync::FileAndDirectory),
        )
        .unwrap();
        assert_eq!(opens.get(), 1);
        let counts = work_counts::get();
        assert_eq!(counts.metadata_writes, 1);
        assert_eq!(counts.renames, 1);
        assert_eq!(counts.syncs, 2);
        assert_eq!(
            fs::read(&task.paths.json).unwrap(),
            serde_json::to_vec_pretty(&metadata).unwrap()
        );
    }
}

#[cfg(unix)]
#[test]
fn metadata_persistence_compare_read_failure_falls_back_to_replacement() {
    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let metadata = metadata(&task);
    write_task_at(&task, &metadata).unwrap();
    let path = task.paths.json.clone();
    let opens = std::rc::Rc::new(std::cell::Cell::new(0));
    let injected = opens.clone();
    work_counts::reset();
    with_openat_file_hook(
        Box::new(move |_, name, _| {
            if name != OsStr::new(METADATA_FILE) {
                return None;
            }
            injected.set(injected.get() + 1);
            // A regular but write-only handle passes validation and makes the
            // byte comparison's read fail with EBADF on both Linux and macOS.
            Some(OpenOptions::new().write(true).open(&path))
        }),
        || write_task_at(&task, &metadata),
    )
    .unwrap();
    assert_eq!(opens.get(), 1);
    assert_eq!(work_counts::get().metadata_writes, 1);
    assert_eq!(work_counts::get().renames, 1);
    assert_eq!(
        fs::read(&task.paths.json).unwrap(),
        serde_json::to_vec_pretty(&metadata).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn metadata_persistence_compare_replaced_handle_falls_back_to_replacement() {
    use std::os::unix::fs::MetadataExt;

    let storage = tempfile::tempdir().unwrap();
    let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
    let metadata = metadata(&task);
    write_task_at(&task, &metadata).unwrap();
    let path = task.paths.json.clone();
    let control = PinnedDir::open(&task.paths.control_dir).unwrap();
    let bytes = fs::read(&path).unwrap();
    let replacements = std::rc::Rc::new(std::cell::Cell::new(0));
    let injected = replacements.clone();
    work_counts::reset();
    with_artifact_validation_hook_for_test(
        move |file| {
            let opened = file.metadata().unwrap();
            let current = fs::metadata(&path).unwrap();
            if (opened.dev(), opened.ino()) == (current.dev(), current.ino()) {
                randomized_atomic_replace(&control, OsStr::new(METADATA_FILE), &bytes).unwrap();
                injected.set(injected.get() + 1);
            }
        },
        || write_task_at(&task, &metadata),
    )
    .unwrap();
    assert_eq!(replacements.get(), 1);
    assert_eq!(work_counts::get().metadata_writes, 1);
    assert_eq!(
        work_counts::get().renames,
        2,
        "one racing writer plus the fallback"
    );
    assert_eq!(
        fs::read(&task.paths.json).unwrap(),
        serde_json::to_vec_pretty(&metadata).unwrap()
    );
}

#[test]
fn metadata_persistence_crash_child() {
    let Some(path) = std::env::var_os("AFT_METADATA_CRASH_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let mut metadata = read_task(&path).unwrap();
    metadata.command = "new complete record".into();
    let after_rename = std::env::var("AFT_METADATA_CRASH_AFTER_RENAME").unwrap() == "true";
    ATOMIC_REPLACE_CRASH.with(|phase| phase.set(Some(after_rename)));
    write_task(&path, &metadata).unwrap();
    panic!("crash hook did not run");
}

#[test]
fn metadata_persistence_crash_leaves_old_or_new_complete_record() {
    let storage = tempfile::tempdir().unwrap();
    for after_rename in [false, true] {
        let task = allocate_task_layout(storage.path(), "metadata-persistence").unwrap();
        let old = metadata(&task);
        write_task_at(&task, &old).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bash_background::persistence::metadata_persistence_tests::metadata_persistence_crash_child",
                "--nocapture",
            ])
            .env("AFT_METADATA_CRASH_PATH", &task.paths.json)
            .env("AFT_METADATA_CRASH_AFTER_RENAME", after_rename.to_string())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        let mut expected = old;
        if after_rename {
            expected.command = "new complete record".into();
        }
        assert_eq!(
            fs::read(&task.paths.json).unwrap(),
            serde_json::to_vec_pretty(&expected).unwrap()
        );
        read_task_at(&task).unwrap();
    }
}

/// Manual I/O measurement, excluded from ordinary filtered regression runs.
/// Seed real directory-layout metadata, but never register or execute a command.
#[test]
#[ignore = "creates a throwaway 160,000-task store for filesystem measurements"]
fn metadata_persistence_synthetic_store() {
    let scratch = std::env::current_dir().unwrap().join("target/scratch");
    fs::create_dir_all(&scratch).unwrap();
    let storage = tempfile::Builder::new()
        .prefix("metadata-store-")
        .tempdir_in(&scratch)
        .unwrap();
    let started = std::time::Instant::now();
    let mut samples = Vec::new();
    for index in 0..160_000 {
        let harness = ["runner", "opencode", "pi"][index % 3];
        let session = format!("session-{}", index % 30);
        let session_dir = session_tasks_dir(&storage.path().join(harness), &session);
        let paths = TaskPaths::directory(session_dir, &format!("bash-{index:016x}"));
        fs::create_dir_all(&paths.control_dir).unwrap();
        fs::create_dir_all(&paths.io_dir).unwrap();
        let mut record = PersistedTask::starting(
            paths.task_id.clone(),
            session,
            "synthetic task".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        if index < 40 {
            record.mark_running(123, 123);
        } else {
            record.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        }
        fs::write(&paths.json, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        if index < 80 {
            samples.push((
                resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
                record,
            ));
        }
        if index == 79 || index == 159_999 {
            eprintln!("seeded {} folders in {:?}", index + 1, started.elapsed());
            measure_replacements(index + 1, &mut samples);
        }
    }
}

fn measure_replacements(folders: usize, samples: &mut [(ResolvedTask, PersistedTask)]) {
    for legacy in [true, false] {
        work_counts::reset();
        let started = std::time::Instant::now();
        for _ in 0..10 {
            for (task, record) in samples.iter() {
                if legacy {
                    // The former writer serialized and replaced every request.
                    let bytes = serde_json::to_vec_pretty(record).unwrap();
                    work_counts::record_metadata_write();
                    atomic_replace(
                        &task.dirs.control,
                        OsStr::new(METADATA_FILE),
                        &bytes,
                        ReplaceSync::for_task(record),
                    )
                    .unwrap();
                } else {
                    write_task_at(task, record).unwrap();
                }
            }
        }
        eprintln!(
            "folders={folders} legacy={legacy} unchanged=800 elapsed={:?} counts={:?}",
            started.elapsed(),
            work_counts::get()
        );
        assert_eq!(work_counts::get().renames, if legacy { 800 } else { 0 });
    }
    work_counts::reset();
    let started = std::time::Instant::now();
    for (task, record) in samples.iter_mut() {
        record.status_reason = Some(format!("changed at {folders} folders"));
        write_task_at(task, record).unwrap();
    }
    eprintln!(
        "folders={folders} changed=80 elapsed={:?} counts={:?}",
        started.elapsed(),
        work_counts::get()
    );
    assert_eq!(work_counts::get().metadata_writes, 80);
    assert_eq!(work_counts::get().renames, 80);
}
