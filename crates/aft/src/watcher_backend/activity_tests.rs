use super::{counters::WatcherCounters, ProjectWatcher};
use crate::{config::Config, context::AppContext, parser::TreeSitterProvider};
use std::path::Path;
use std::sync::{atomic::AtomicU64, mpsc, Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub(super) fn note_backend_wait(counters: &WatcherCounters) {
    let mut activity = counters.backend_activity.lock().unwrap();
    activity.0 += 1;
    activity.1 += crate::watcher_filter::EXCLUSION_TEST_PATHS.with(|calls| calls.replace(0));
}

#[derive(Debug)]
pub(super) struct RegistrationGate {
    reached: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

pub(super) fn pause_after_matcher_registration(counters: &WatcherCounters) {
    let gate = counters.matcher_registration_gate.lock().unwrap().take();
    if let Some(gate) = gate {
        gate.reached.send(()).unwrap();
        gate.release.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

fn wait_until_backend_receives(counters: &WatcherCounters) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while counters.backend_activity.lock().unwrap().0 == 0 {
        assert!(Instant::now() < deadline, "backend did not start");
        thread::sleep(Duration::from_millis(1));
    }
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
    wait_until_backend_receives(&counters);
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

fn context_for(root: &Path) -> AppContext {
    AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root.to_path_buf()),
            ..Config::default()
        },
    )
}

#[test]
fn idle_backend_observes_matcher_publication_without_file_events() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let ctx = context_for(&root);
    let (tx, _rx) = mpsc::channel();
    let _watcher = ProjectWatcher::create(
        root,
        Vec::new(),
        tx,
        ctx.shared_gitignore(),
        ctx.gitignore_generation(),
    )
    .unwrap();
    let counters = ctx.watcher_counters();
    wait_until_backend_receives(&counters);

    let deadline = Instant::now() + Duration::from_millis(200);
    ctx.clear_gitignore();
    while counters.backend_exclusions().matcher_generation != 1 {
        assert!(
            Instant::now() < deadline,
            "matcher publication did not wake the backend"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn backend_reads_initial_generation_after_registering_matcher_wake() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let ctx = context_for(&root);
    let counters = ctx.watcher_counters();
    let (reached, registered) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    *counters.matcher_registration_gate.lock().unwrap() =
        Some(RegistrationGate { reached, release });
    let (tx, _rx) = mpsc::channel();
    let matcher = ctx.shared_gitignore();
    let generation = ctx.gitignore_generation();
    let create =
        thread::spawn(move || ProjectWatcher::create(root, Vec::new(), tx, matcher, generation));
    registered.recv_timeout(Duration::from_secs(5)).unwrap();
    ctx.clear_gitignore();
    resume.send(()).unwrap();
    let _watcher = create.join().unwrap().unwrap();
    wait_until_backend_receives(&counters);
    assert_eq!(counters.backend_exclusions().matcher_generation, 1);
    // The first plan already saw the matcher; the backend did not have to replan.
    assert_eq!(counters.backend_activity.lock().unwrap().1, 0);
}

#[cfg(target_os = "macos")]
#[test]
fn idle_fsevents_backend_drains_native_flush_without_file_events() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let counters = super::counters::watcher_counters_for_root(&root);
    let (tx, _rx) = mpsc::channel();
    let _watcher = ProjectWatcher::create(
        root,
        Vec::new(),
        tx,
        Arc::new(RwLock::new(None)),
        Arc::new(AtomicU64::new(1)),
    )
    .unwrap();
    wait_until_backend_receives(&counters);
    assert!(counters
        .bash_flush
        .native_flush_only(Instant::now() + Duration::from_secs(1)));
}
