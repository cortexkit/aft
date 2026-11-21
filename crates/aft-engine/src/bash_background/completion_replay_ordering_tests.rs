use super::*;
use crate::bash_background::persistence::{task_paths, write_task};
use serde_json::Value;

pub(crate) const SESSION: &str = "replay-ordering";
const SECRET: &str = "opaque ordering credential";
pub(crate) const MASKED: &str = "<REDACTED_SECRET>";

pub(crate) fn registry(storage: &Path) -> (BgTaskRegistry, Arc<Mutex<Vec<PushFrame>>>) {
    let (registry, _db, frames) = super::tests::registry_with_db_and_frames(storage);
    // Drive observation explicitly; a periodic watchdog tick or GC must not
    // choose the interleaving that these tests are meant to exercise.
    registry
        .inner
        .watchdog_started
        .store(true, Ordering::SeqCst);
    registry
        .inner
        .persisted_gc_started
        .store(true, Ordering::SeqCst);
    (registry, frames)
}

fn persisted_running(storage: &Path) -> (PersistedTask, TaskPaths) {
    let paths = task_paths(storage, SESSION, &random_slug()).unwrap();
    let mut metadata = PersistedTask::starting(
        paths.task_id.clone(),
        SESSION.into(),
        "printf output".into(),
        storage.into(),
        Some(storage.into()),
        None,
        true,
        false,
    );
    metadata.harness = Some("opencode".into());
    metadata.status = BgTaskStatus::Running;
    metadata.completion_has_env_secrets = Some(false);
    metadata.completion_has_non_ticket_env_secrets = Some(false);
    write_task(&paths.json, &metadata).unwrap();
    fs::write(&paths.stdout, "output\n").unwrap();
    fs::write(&paths.stderr, "").unwrap();
    (metadata, paths)
}

pub(crate) fn live_secret_task(registry: &BgTaskRegistry, storage: &Path) -> Arc<BgTask> {
    let (mut metadata, paths) = persisted_running(storage);
    metadata.completion_has_env_secrets = Some(true);
    metadata.completion_has_non_ticket_env_secrets = Some(true);
    write_task(&paths.json, &metadata).unwrap();
    let task_id = metadata.task_id.clone();
    registry
        .insert_rehydrated_task(metadata, paths, true)
        .unwrap();
    let task = registry.task(&task_id).unwrap();
    *task.completion_secrets.lock().unwrap() =
        Some(crate::log_redact::SecretValues::from_environment([(
            std::ffi::OsStr::new("DEPLOY_TOKEN"),
            std::ffi::OsStr::new(SECRET),
        )]));
    fs::write(&task.paths.stdout, format!("finished {SECRET}\n")).unwrap();
    task
}

pub(crate) fn completion_frames(frames: &Arc<Mutex<Vec<PushFrame>>>, task_id: &str) -> Vec<Value> {
    frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| {
            let value = serde_json::to_value(frame).unwrap();
            (value["type"] == "bash_completed" && value["task_id"] == task_id).then_some(value)
        })
        .collect()
}

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

pub(crate) fn capture_live_ordering_frames(owner: &BgTaskRegistry) -> Arc<Mutex<Vec<PushFrame>>> {
    owner.inner.watchdog_started.store(true, Ordering::SeqCst);
    owner
        .inner
        .persisted_gc_started
        .store(true, Ordering::SeqCst);
    owner.set_harness(Harness::Opencode);
    let frames = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&frames);
    *owner.inner.progress_sender.lock().unwrap() = Some(Arc::new(Box::new(move |frame| {
        captured.lock().unwrap().push(frame);
    })));
    frames
}

pub(crate) fn pause_ordering_publication(
    task: &BgTask,
) -> (
    crossbeam_channel::Receiver<()>,
    crossbeam_channel::Sender<()>,
) {
    let (reached, reached_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release) = crossbeam_channel::bounded(1);
    TERMINAL_PUBLICATION_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(task.task_id.clone(), KillSignalGate { reached, release });
    (reached_rx, release_tx)
}
