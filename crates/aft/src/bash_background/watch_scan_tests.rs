//! Watch scans over captures that a restart interrupts or that cannot all be
//! opened.

use super::tests::{pattern_match_frames, registry_with_db_and_frames};
use super::watch_scan::{set_match_writes_before_stop_for_test, set_watch_scan_budget_for_test};
use super::*;

/// Spawn a long-running piped task with a repeating `READY` watch. The
/// registry's watchdog is stopped first, so only the test scans the output.
fn watched_task(storage: &Path, registry: &BgTaskRegistry) -> (Arc<BgTask>, String) {
    let task_id = registry
        .spawn(
            SpawnPlan::Unsandboxed,
            "sleep 30",
            "session".to_string(),
            storage.to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(60)),
            None,
            storage.to_path_buf(),
            10,
            true,
            false,
            Some(storage.to_path_buf()),
        )
        .unwrap();
    registry
        .inner
        .shutdown
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let watch_id = registry
        .register_watch(
            task_id.clone(),
            WatchPattern::Substring("READY".into()),
            false,
        )
        .unwrap();
    (
        registry.task_for_session(&task_id, "session").unwrap(),
        watch_id,
    )
}

fn append(path: &Path, bytes: &[u8]) {
    fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn an_unreadable_stderr_capture_does_not_stop_stdout_matches() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, _db, frames) = registry_with_db_and_frames(dir.path());
    let (task, _watch) = watched_task(dir.path(), &registry);
    fs::remove_file(&task.paths.stderr).unwrap();
    append(&task.paths.stdout, b"READY now\n");

    registry.scan_task_watch_output(&task);

    assert!(
        pattern_match_frames(&frames)
            .iter()
            .any(|frame| frame.task_id == task.task_id),
        "the stdout match was not delivered because stderr could not be opened"
    );
    let _ = registry.kill(&task.task_id, "session");
}

#[test]
fn a_restart_between_two_match_writes_loses_neither_match() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path();
    let (registry, _db, _frames) = registry_with_db_and_frames(storage);
    let (task, watch_id) = watched_task(storage, &registry);
    // Two matches in different 64 KiB scan chunks, the second far more than
    // a scan's overlap before the end, so resuming at the end cannot see it.
    let filler = format!("{}\n", "f".repeat(99)).repeat(700);
    let mut output = b"READY one\n".to_vec();
    output.extend_from_slice(filler.as_bytes());
    output.extend_from_slice(b"READY two\n");
    output.extend_from_slice(filler[..16 * 1024].as_bytes());
    append(&task.paths.stdout, &output);

    // Stop the scan right after it saved its first match, as a restart
    // would.
    set_watch_scan_budget_for_test(Some(Duration::from_secs(30)));
    set_match_writes_before_stop_for_test(Some(1));
    registry.scan_task_watch_output(&task);
    set_match_writes_before_stop_for_test(None);
    set_watch_scan_budget_for_test(None);
    registry.clear_task_watch_state(&task.task_id);

    let (replayed, _db2, replay_frames) = registry_with_db_and_frames(storage);
    replayed
        .replay_session_inner(storage, "session", None)
        .unwrap();

    let mut contexts: Vec<String> = pattern_match_frames(&replay_frames)
        .into_iter()
        .filter(|frame| frame.watch_id == watch_id)
        .map(|frame| frame.context)
        .collect();
    contexts.extend(
        replayed
            .pending_pattern_matches_for_session("session")
            .into_iter()
            .filter(|frame| frame.watch_id == watch_id)
            .map(|frame| frame.context),
    );
    assert!(
        contexts.iter().any(|context| context.contains("READY two")),
        "the second match was neither delivered nor saved after the restart: {contexts:?}"
    );
    let _ = replayed.kill(&task.task_id, "session");
    let _ = registry.kill(&task.task_id, "session");
}
