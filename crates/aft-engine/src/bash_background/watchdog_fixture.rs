use super::*;
use crate::bash_background::persistence::{allocate_task_layout, write_task_at};

pub fn watchdog_fixture(
    storage: &Path,
    session: &str,
    delivered: bool,
    days: u64,
) -> (PersistedTask, TaskPaths) {
    let resolved = allocate_task_layout(storage, session).unwrap();
    let mut metadata = PersistedTask::starting(
        resolved.paths.task_id.clone(),
        session.into(),
        "watchdog-fixture".into(),
        storage.join("deleted-workdir"),
        Some(storage.join("deleted-root")),
        None,
        true,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    metadata.completion_delivered = delivered;
    metadata.finished_at = Some(unix_millis() - days * 24 * 60 * 60 * 1000);
    if days > 0 {
        metadata.started_at = metadata.finished_at.unwrap().saturating_sub(60_000);
        metadata.duration_ms = Some(60_000);
    }
    write_task_at(&resolved, &metadata).unwrap();
    for artifact in [
        &resolved.paths.stdout,
        &resolved.paths.stderr,
        &resolved.paths.exit,
    ] {
        fs::write(artifact, b"").unwrap();
    }
    filetime::set_file_mtime(
        &resolved.paths.json,
        filetime::FileTime::from_unix_time((unix_millis() / 1000) as i64 - 25 * 60 * 60, 0),
    )
    .unwrap();
    (metadata, resolved.paths)
}
