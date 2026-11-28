use super::test_support::ordering::*;
use super::*;
#[test]
fn completion_before_replay_keeps_masked_preview_without_replaying_live_push() {
    let storage = tempfile::tempdir().unwrap();
    let (owner, live_frames) = registry(storage.path());
    let task = live_secret_task(&owner, storage.path());
    fs::write(&task.paths.exit, "0").unwrap();
    owner.poll_task(&task).unwrap();
    let live = completion_frames(&live_frames, &task.task_id);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["output_preview"], format!("finished {MASKED}\n"));

    let (restored, replay_frames) = registry(storage.path());
    restored.replay_session(storage.path(), SESSION).unwrap();
    let queued = restored.drain_completions_for_session(Some(SESSION));
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].task_id, task.task_id);
    assert_eq!(
        queued[0].output_preview,
        live[0]["output_preview"].as_str().unwrap()
    );
    assert!(
        completion_frames(&replay_frames, &task.task_id).is_empty(),
        "recovered completions are delivered through drain, not live push"
    );
}

#[test]
fn replay_during_terminal_publication_does_not_steal_live_completion() {
    let storage = tempfile::tempdir().unwrap();
    let (owner, frames) = registry(storage.path());
    let task = live_secret_task(&owner, storage.path());
    let (reached_tx, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    TERMINAL_PUBLICATION_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            task.task_id.clone(),
            KillSignalGate {
                reached: reached_tx,
                release: release_rx,
            },
        );
    fs::write(&task.paths.exit, "0").unwrap();
    let publisher = {
        let registry = owner.clone();
        let task = Arc::clone(&task);
        std::thread::spawn(move || registry.poll_task(&task))
    };
    reached_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        owner
            .observed_status(&task.task_id, SESSION, 0)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Completed
    );
    assert!(owner
        .drain_completions_for_session(Some(SESSION))
        .is_empty());
    // The terminal row is durable, but its live publisher still owns delivery.
    // Replay in this process must keep the registered task instead of queuing
    // a drain-only replacement that would deduplicate away the later push.
    owner.replay_session(storage.path(), SESSION).unwrap();
    assert!(task.is_terminal());
    let snapshot = owner.completion_snapshot_for_task(&task).unwrap();
    assert_eq!(snapshot.exit_code, Some(0));
    assert_eq!(snapshot.output_preview, format!("finished {MASKED}\n"));
    assert!(owner.pending_completions_for_session(SESSION).is_empty());
    release_tx.send(()).unwrap();
    publisher.join().unwrap().unwrap();
    let live = completion_frames(&frames, &task.task_id);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["output_preview"], format!("finished {MASKED}\n"));
    assert_eq!(owner.drain_completions_for_session(Some(SESSION)).len(), 1);
}

#[test]
fn exit_marker_before_during_or_after_replay_status_preserves_completion() {
    for marker_at in ["before", "during", "after"] {
        let storage = tempfile::tempdir().unwrap();
        let (registry, frames) = registry(storage.path());
        let (metadata, paths) = persisted_running(storage.path());
        if marker_at == "before" {
            fs::write(&paths.exit, "0").unwrap();
        }
        let probed = std::cell::Cell::new(false);
        registry
            .rehydrate_resolved_task_with_process_probe(metadata, paths.clone(), SESSION, |_| {
                assert_ne!(
                    marker_at, "before",
                    "an existing marker needs no liveness probe"
                );
                probed.set(true);
                if marker_at == "during" {
                    // Inject a marker after replay's first read, then return
                    // true to simulate a wrapper that has not exited yet.
                    fs::write(&paths.exit, "0").unwrap();
                }
                true
            })
            .unwrap();
        assert_eq!(probed.get(), marker_at != "before");
        let initial = registry
            .observed_status(&paths.task_id, SESSION, 0)
            .unwrap();
        assert_eq!(
            initial.info.status,
            if marker_at == "before" {
                BgTaskStatus::Completed
            } else {
                BgTaskStatus::Running
            }
        );
        if marker_at == "after" {
            fs::write(&paths.exit, "0").unwrap();
        }
        let task = registry.task(&paths.task_id).unwrap();
        registry.poll_task(&task).unwrap();
        let queued = registry.drain_completions_for_session(Some(SESSION));
        assert_eq!(queued.len(), 1, "marker written {marker_at} replay");
        assert_eq!(queued[0].status, BgTaskStatus::Completed);
        assert_eq!(queued[0].exit_code, Some(0));
        assert_eq!(queued[0].output_preview, "output\n");
        assert_eq!(
            completion_frames(&frames, &paths.task_id).len(),
            usize::from(marker_at != "before"),
            "marker written {marker_at} replay"
        );
    }
}
