use super::*;
use crate::bash_background::persistence::{task_paths, write_task};
use serde_json::Value;

pub const SESSION: &str = "replay-ordering";
const SECRET: &str = "opaque ordering credential";
pub const MASKED: &str = "<REDACTED_SECRET>";

pub fn registry(storage: &Path) -> (BgTaskRegistry, Arc<Mutex<Vec<PushFrame>>>) {
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

pub(super) fn persisted_running(storage: &Path) -> (PersistedTask, TaskPaths) {
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

pub fn live_secret_task(registry: &BgTaskRegistry, storage: &Path) -> Arc<BgTask> {
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

pub fn completion_frames(frames: &Arc<Mutex<Vec<PushFrame>>>, task_id: &str) -> Vec<Value> {
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

pub fn capture_live_ordering_frames(owner: &BgTaskRegistry) -> Arc<Mutex<Vec<PushFrame>>> {
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

pub fn pause_ordering_publication(
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
