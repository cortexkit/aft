use super::{counters::WatcherCounters, ProjectWatcher};
use std::sync::{atomic::AtomicU64, mpsc, Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub(super) fn note_backend_wait(counters: &WatcherCounters) {
    let mut activity = counters.backend_activity.lock().unwrap();
    activity.0 += 1;
    activity.1 += crate::watcher_filter::EXCLUSION_TEST_PATHS.with(|calls| calls.replace(0));
}

#[test]
fn idle_backend_waits_without_collecting_exclusion_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();
    let (tx, _rx) = mpsc::channel();
    let counters = super::counters::watcher_counters_for_root(&root);
    let _watcher = ProjectWatcher::create(
        root,
        Vec::new(),
        tx,
        Arc::new(RwLock::new(None)),
        Arc::new(AtomicU64::new(1)),
    )
    .unwrap();
    assert!(!counters.backend_exclusions().paths.is_empty());
    let deadline = Instant::now() + Duration::from_secs(5);
    while counters.backend_activity.lock().unwrap().0 == 0 {
        assert!(Instant::now() < deadline, "backend did not start");
        thread::sleep(Duration::from_millis(1));
    }
    let (before_waits, before_paths) = *counters.backend_activity.lock().unwrap();
    let started = Instant::now();
    thread::sleep(Duration::from_secs(2));
    let (waits, paths) = *counters.backend_activity.lock().unwrap();
    let wakes = waits - before_waits;
    let paths = paths - before_paths;
    println!(
        "idle backend: {wakes} wakes, {paths} paths() calls, {:.2} wakes/s/root",
        wakes as f64 / started.elapsed().as_secs_f64()
    );
    assert!(
        wakes <= 2 && paths == 0,
        "{wakes} wakes, {paths} paths() calls"
    );
}
