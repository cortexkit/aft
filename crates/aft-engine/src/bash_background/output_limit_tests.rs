use super::*;
use crate::bash_background::{buffer, watchdog, HardKill};

const SESSION: &str = "output-limit";

fn spawn(registry: &BgTaskRegistry, dir: &Path, command: &str, pty: bool) -> Arc<BgTask> {
    let run = format!(
        "{command}; touch ready; {}",
        crate::test_gate::gate("release", "sleep 0.02")
    );
    let task_id = if pty {
        registry.spawn_pty_with_shell(
            SpawnPlan::Unsandboxed,
            &run,
            crate::bash_background::BashShell::Bash,
            PathBuf::from("/bin/bash"),
            SESSION.into(),
            dir.into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(30)),
            None,
            dir.into(),
            10,
            true,
            false,
            Some(dir.into()),
            24,
            80,
        )
    } else {
        registry.spawn(
            SpawnPlan::Unsandboxed,
            &run,
            SESSION.into(),
            dir.into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(30)),
            None,
            dir.into(),
            10,
            true,
            false,
            Some(dir.into()),
        )
    }
    .unwrap();
    registry.task_for_test(&task_id).unwrap()
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn tick(registry: &BgTaskRegistry) {
    watchdog::run_pass(registry, WatchdogPassCause::Tick, &mut HashMap::new());
}

#[test]
fn output_limit_stops_piped_command_and_caps_retained_output() {
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.inner.shutdown.store(true, Ordering::SeqCst);
    let cap = buffer::DISK_LIMIT_BYTES;
    let limit = cap + 12;
    let _limit = buffer::limit_for_test(session_tasks_dir(dir.path(), SESSION), limit);
    // Seek instead of writing 100 MiB of payload. Neither stream crosses the
    // running limit on its own; the terminal cap still trims stdout.
    let task = spawn(&registry, dir.path(), &format!("dd if=/dev/zero bs=1 count=1 seek={cap} 2>/dev/null; printf end; printf 'stderr-content-over-limit' >&2"), false);
    wait_until("piped command did not write its output", || {
        dir.path().join("ready").exists()
    });
    let len = |path| fs::metadata(path).unwrap().len();
    assert_eq!(len(&task.paths.stdout), cap + 4);
    assert!(len(&task.paths.stderr) < limit);
    assert!(len(&task.paths.stdout) + len(&task.paths.stderr) > limit);
    wait_until("output limit did not stop the piped command", || {
        tick(&registry);
        task.is_terminal()
    });
    let snapshot = registry
        .status(&task.task_id, SESSION, None, None, 8192)
        .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
    assert_eq!(
        snapshot.info.status_reason.as_deref(),
        Some(buffer::OUTPUT_LIMIT_REASON)
    );
    assert_eq!(len(&task.paths.stdout), cap);
    assert!(len(&task.paths.stderr) <= cap);
    use std::io::{Read, Seek, SeekFrom};
    let mut stdout = fs::File::open(&task.paths.stdout).unwrap();
    stdout.seek(SeekFrom::End(-3)).unwrap();
    let mut tail = [0; 3];
    stdout.read_exact(&mut tail).unwrap();
    assert_eq!(&tail, b"end");
}

#[test]
fn output_limit_stops_pty_command_from_combined_capture() {
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.inner.shutdown.store(true, Ordering::SeqCst);
    let _limit = buffer::limit_for_test(session_tasks_dir(dir.path(), SESSION), 12);
    let task = spawn(
        &registry,
        dir.path(),
        "printf '12345678'; printf 'abcdefgh' >&2",
        true,
    );
    wait_until("output limit did not stop the PTY command", || {
        tick(&registry);
        task.is_terminal()
    });
    let snapshot = registry
        .status(&task.task_id, SESSION, None, None, 8192)
        .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
    assert_eq!(
        snapshot.info.status_reason.as_deref(),
        Some(buffer::OUTPUT_LIMIT_REASON)
    );
    assert!(fs::metadata(snapshot.output_path.unwrap()).unwrap().len() <= buffer::DISK_LIMIT_BYTES);
}

#[test]
fn output_limit_leaves_under_limit_command_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.inner.shutdown.store(true, Ordering::SeqCst);
    let _limit = buffer::limit_for_test(session_tasks_dir(dir.path(), SESSION), 8);
    let task = spawn(&registry, dir.path(), "printf test; printf err >&2", false);
    wait_until("piped command did not write its output", || {
        dir.path().join("ready").exists()
    });
    tick(&registry);
    assert!(task.lock_state().unwrap().metadata.status_reason.is_none());
    assert!(!task.timeout_kill_queued.load(Ordering::SeqCst));
    let snapshot = registry
        .status(&task.task_id, SESSION, None, None, 8192)
        .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Running);
    assert_eq!(snapshot.info.status_reason, None);
    fs::write(dir.path().join("release"), "").unwrap();
    wait_until("under-limit command did not exit normally", || {
        tick(&registry);
        task.is_terminal()
    });
    let snapshot = registry
        .status(&task.task_id, SESSION, None, None, 8192)
        .unwrap();
    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
}
