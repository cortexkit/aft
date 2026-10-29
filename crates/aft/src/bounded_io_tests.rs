use super::*;
use std::sync::mpsc::{channel, Receiver, Sender};

struct HelperGate {
    reached: Sender<()>,
    release: Receiver<()>,
    finished: Sender<()>,
}

static HELPER_GATES: LazyLock<Mutex<HashMap<PathBuf, HelperGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RUNNING_GATES: LazyLock<Mutex<HashMap<PathBuf, HelperGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(unix)]
pub(crate) struct HelperStall {
    path: PathBuf,
    gates: &'static Mutex<HashMap<PathBuf, HelperGate>>,
    pub(crate) reached: Receiver<()>,
    release: Sender<()>,
    finished: Receiver<()>,
}

#[cfg(unix)]
impl Drop for HelperStall {
    fn drop(&mut self) {
        self.gates.lock().unwrap().remove(&self.path);
        if self.release.send(()).is_ok() {
            let _ = self.finished.recv_timeout(IO_BUDGET);
        }
    }
}

#[cfg(unix)]
pub(crate) fn stall_helper(path: &Path) -> HelperStall {
    stall_helper_at(path, &HELPER_GATES)
}

#[cfg(unix)]
pub(crate) fn stall_running_helper(path: &Path) -> HelperStall {
    stall_helper_at(path, &RUNNING_GATES)
}

#[cfg(unix)]
fn stall_helper_at(
    path: &Path,
    gates: &'static Mutex<HashMap<PathBuf, HelperGate>>,
) -> HelperStall {
    let (reached, reached_rx) = channel();
    let (release, release_rx) = channel();
    let (finished, finished_rx) = channel();
    gates.lock().unwrap().insert(
        path.to_path_buf(),
        HelperGate {
            reached,
            release: release_rx,
            finished,
        },
    );
    HelperStall {
        path: path.to_path_buf(),
        gates,
        reached: reached_rx,
        release,
        finished: finished_rx,
    }
}

static BLOCKED_DIRECTORIES: LazyLock<Mutex<HashMap<PathBuf, Arc<AtomicUsize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn before_helper(path: &Path) {
    wait_at_gate(path, &HELPER_GATES);
}

pub(super) fn after_helper_start(path: &Path) {
    wait_at_gate(path, &RUNNING_GATES);
}

fn wait_at_gate(path: &Path, gates: &Mutex<HashMap<PathBuf, HelperGate>>) {
    let gate = gates.lock().unwrap().remove(path);
    if let Some(gate) = gate {
        let _ = gate.reached.send(());
        let _ = gate.release.recv();
        let _ = gate.finished.send(());
    }
}

pub(super) fn before_directory_probe(path: &Path) -> io::Result<()> {
    if let Some(fired) = BLOCKED_DIRECTORIES.lock().unwrap().get(path) {
        fired.fetch_add(1, Ordering::Relaxed);
        return Err(failure(
            path,
            "read_blocked",
            "injected stalled directory enumeration",
        ));
    }
    Ok(())
}

struct BlockDirectory(PathBuf, Arc<AtomicUsize>);

impl BlockDirectory {
    fn new(path: &Path) -> Self {
        let fired = Arc::new(AtomicUsize::new(0));
        BLOCKED_DIRECTORIES
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), Arc::clone(&fired));
        Self(path.to_path_buf(), fired)
    }
}

impl Drop for BlockDirectory {
    fn drop(&mut self) {
        BLOCKED_DIRECTORIES.lock().unwrap().remove(&self.0);
    }
}

#[test]
fn reused_helper_starts_each_job_with_clean_counters_and_hooks() {
    use crate::commands::read::{
        IMAGE_PROCESSING_DELAY, LEGACY_HASHLINE_READ, LEGACY_READ_RENDER_BYTES,
    };
    use crate::hashline::snapshot::{SOURCE_AFTER_READ, SOURCE_READS};
    use crate::inspect::job::CANONICALIZE_CALLS;
    use crate::search_index::{
        audit_record, audit_work, AuditWork, AUDIT_CANCEL_ELAPSED, AUDIT_IO_PROBE,
    };

    let pool = Limiter::new(1);
    let nested_pool = pool.clone();
    let first = run_with_limiter(Path::new("first"), None, pool.clone(), move |_| {
        SOURCE_READS.with(|count| count.set(count.get() + 1));
        LEGACY_READ_RENDER_BYTES.with(|count| count.set(count.get() + 7));
        LEGACY_HASHLINE_READ.with(|flag| flag.set(true));
        IMAGE_PROCESSING_DELAY.with(|delay| delay.set(Duration::from_secs(1)));
        SOURCE_AFTER_READ.with(|hook| *hook.borrow_mut() = Some(Box::new(|| panic!("old hook"))));
        CANONICALIZE_CALLS.with(|calls| *calls.borrow_mut() = Some(vec![PathBuf::from("old")]));
        AUDIT_IO_PROBE.with(|probe| *probe.borrow_mut() = Some(Box::new(|| panic!("old probe"))));
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.set(Some(Duration::from_secs(1))));
        audit_record(|work| {
            work.cancel_probes += 1;
            work.corpus_opens += 1;
            work.sort_stats += 1;
            work.sort_canonicalizes += 1;
            work.watcher_global += 1;
            work.watcher_head += 1;
            work.filter_keys += 1;
            work.presence_checks += 1;
        });
        MUTATION_ADMISSION.with(|flag| flag.set(true));
        DIRECTORIES.with(|cache| {
            let mut cache = cache.borrow_mut();
            cache.0 = 42;
            cache
                .1
                .insert(PathBuf::from("old"), Access { direct: true });
        });
        let thread = std::thread::current().id();
        run_with_limiter(Path::new("nested"), None, nested_pool, move |_| {
            assert_eq!(std::thread::current().id(), thread);
            assert_eq!(SOURCE_READS.with(|count| count.get()), 1);
            assert_eq!(LEGACY_READ_RENDER_BYTES.with(|count| count.get()), 7);
            assert_eq!(audit_work().filter_keys, 1);
            Ok(())
        })?;
        Ok(std::thread::current().id())
    })
    .unwrap();
    let second = run_with_limiter(Path::new("second"), None, pool, |_| {
        Ok((
            std::thread::current().id(),
            SOURCE_READS.with(|count| count.get()),
            LEGACY_READ_RENDER_BYTES.with(|count| count.get()),
            LEGACY_HASHLINE_READ.with(|flag| flag.get()),
            IMAGE_PROCESSING_DELAY.with(|delay| delay.get()),
            SOURCE_AFTER_READ.with(|hook| hook.borrow().is_none()),
            DIRECTORIES.with(|cache| cache.borrow().1.is_empty()),
            audit_work(),
            MUTATION_ADMISSION.with(|flag| flag.get()),
            CANONICALIZE_CALLS.with(|calls| calls.borrow().is_none()),
            AUDIT_IO_PROBE.with(|probe| probe.borrow().is_none()),
            AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.get()),
        ))
    })
    .unwrap();
    assert_eq!(first, second.0, "both jobs must use the same helper");
    assert_eq!((second.1, second.2), (0, 0), "per-job counters leaked");
    assert!(!second.3, "legacy read flag leaked");
    assert_eq!(second.4, Duration::ZERO, "image delay leaked");
    assert!(second.5, "source-read hook leaked");
    assert!(second.6, "directory cache leaked");
    assert_eq!(second.7, AuditWork::default(), "grep work counters leaked");
    assert!(!second.8, "mutation admission leaked");
    assert!(second.9, "canonicalization collector leaked");
    assert!(second.10, "grep I/O probe leaked");
    assert_eq!(second.11, None, "grep cancellation clock leaked");
}

#[test]
fn helper_job_scope_restores_state_after_unwind() {
    use crate::commands::read::LEGACY_READ_RENDER_BYTES;
    use crate::hashline::snapshot::SOURCE_READS;

    let previous_reads = SOURCE_READS.with(|count| count.replace(11));
    let previous_render = LEGACY_READ_RENDER_BYTES.with(|count| count.replace(13));
    let unwound = std::panic::catch_unwind(|| {
        let _state = HelperJobScope::enter();
        assert_eq!(SOURCE_READS.with(|count| count.get()), 0);
        assert_eq!(LEGACY_READ_RENDER_BYTES.with(|count| count.get()), 0);
        SOURCE_READS.with(|count| count.set(1));
        LEGACY_READ_RENDER_BYTES.with(|count| count.set(7));
        panic!("unwind a helper job");
    });
    let reads = SOURCE_READS.with(|count| count.replace(previous_reads));
    let render = LEGACY_READ_RENDER_BYTES.with(|count| count.replace(previous_render));
    assert!(unwound.is_err());
    assert_eq!((reads, render), (11, 13));
}

#[test]
fn helper_health_does_not_start_an_unused_pool() {
    let helpers = OnceLock::new();
    let health = helper_health_from(&helpers);
    assert_eq!(health["status"], "not_started");
    assert_eq!(health["pool_size"], IO_POOL_SIZE);
    assert_eq!(health["thread_cap"], MAX_IO_THREADS);
    assert_eq!(health["live"], 0);
    assert!(helpers.get().is_none(), "health started the helper factory");
}

#[test]
fn helper_health_does_not_wait_for_the_state_lock() {
    let pool = Limiter::new(1);
    let state = pool.state.lock().unwrap();
    let reader = pool.clone();
    let (tx, rx) = channel();
    let thread = std::thread::spawn(move || tx.send(reader.health()).unwrap());
    let health = rx.recv_timeout(Duration::from_secs(1));
    drop(state);
    thread.join().unwrap();
    let health = health.expect("health waited for the helper state lock");
    assert_eq!(health["status"], "unavailable");
    assert_eq!(health["reason"], "state_busy");
    assert!(
        health.get("live").is_none(),
        "unavailable counts are unknown"
    );
    assert_eq!(pool.health()["status"], "ready");
}

#[test]
fn metadata_and_canonicalize_do_not_enumerate_an_ancestor() {
    let dir = tempfile::tempdir().unwrap();
    let ancestor = std::fs::canonicalize(dir.path()).unwrap();
    let root = ancestor.join("project");
    std::fs::create_dir(&root).unwrap();
    let blocked = BlockDirectory::new(&ancestor);
    assert!(
        with_budget(Some(Budget::after(IO_BUDGET)), || metadata(&root))
            .unwrap()
            .is_dir()
    );
    let resolved = with_budget(Some(Budget::after(IO_BUDGET)), || canonicalize(&root)).unwrap();
    assert_eq!(resolved, std::fs::canonicalize(&root).unwrap());
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

// Only macOS and Linux currently identify local filesystems for direct access;
// Windows and unknown mounts retain per-file helper isolation.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn sibling_metadata_reuses_traversal_without_granting_listing_access() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    assert!(local_filesystem(&root).unwrap());
    let paths: Vec<_> = (0..8).map(|n| root.join(format!("file-{n}"))).collect();
    for path in &paths {
        std::fs::write(path, "sibling").unwrap();
    }
    let blocked = BlockDirectory::new(&root);
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        for path in &paths {
            assert!(metadata(path).unwrap().is_file());
        }
        assert_eq!(budget.measurements(), (1, 1), "one cold stat helper");
        assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
        let error = read(&paths[0], None).unwrap_err();
        assert!(error.to_string().contains("read_blocked"), "{error}");
        assert_eq!(blocked.1.load(Ordering::Relaxed), 1);
        assert_eq!(
            budget.measurements(),
            (2, 2),
            "reading still probes the parent"
        );
    });
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn canonicalize_warms_sibling_traversal_shared_only_within_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    assert!(local_filesystem(&root).unwrap());
    let first = root.join("first");
    let second = root.join("second");
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&second, "second").unwrap();
    let blocked = BlockDirectory::new(&root);
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        assert_eq!(canonicalize(&first).unwrap(), first);
        assert_eq!(canonicalize(&second).unwrap(), second);
        let worker_budget = budget.clone();
        let worker_path = second.clone();
        std::thread::spawn(move || {
            with_budget(Some(worker_budget), || {
                assert!(metadata(&worker_path).unwrap().is_file());
            });
        })
        .join()
        .unwrap();
        assert_eq!(budget.measurements(), (1, 1));
    });
    assert!(!cached_traversal(&root), "no evidence outside the request");
    let fresh_budget = Budget::after(IO_BUDGET);
    with_budget(Some(fresh_budget.clone()), || {
        assert!(metadata(&second).unwrap().is_file());
        assert_eq!(
            fresh_budget.measurements(),
            (1, 1),
            "a new request probes again"
        );
    });
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

// A directory probed by one thread of a request answers a stat below it on any
// other thread of the same request. Grep reads files on rayon workers, then
// stats every matched file again on the thread that sorts the results; that
// thread must not spawn a helper per directory it did not read itself.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn directory_probed_on_another_thread_answers_child_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    assert!(local_filesystem(&root).unwrap());
    let file = root.join("file");
    std::fs::write(&file, "contents").unwrap();
    let budget = Budget::after(IO_BUDGET);
    let worker_budget = budget.clone();
    let worker_root = root.clone();
    std::thread::spawn(move || {
        with_budget(Some(worker_budget), || {
            probe_directory(&worker_root, None).unwrap();
        });
    })
    .join()
    .unwrap();
    assert_eq!(budget.measurements(), (1, 1), "one directory probe");
    with_budget(Some(budget.clone()), || {
        assert!(metadata(&file).unwrap().is_file());
        assert!(canonicalize(&root).is_ok());
        assert!(probe_target(&root).is_ok());
    });
    assert_eq!(
        budget.measurements(),
        (1, 1),
        "the request's finished directory probe must be reused on every thread"
    );
    // A new request does not inherit that probe.
    let fresh_budget = Budget::after(IO_BUDGET);
    with_budget(Some(fresh_budget.clone()), || {
        assert!(metadata(&file).unwrap().is_file());
    });
    assert_eq!(fresh_budget.measurements(), (1, 1));
}

#[test]
fn failed_stats_do_not_warm_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        assert_eq!(metadata(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(
            canonicalize(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!cached_traversal(dir.path()));
        assert_eq!(budget.measurements(), (2, 2));
        std::fs::write(&path, "now present").unwrap();
        assert!(metadata(&path).unwrap().is_file());
        assert_eq!(budget.measurements(), (3, 3));
    });
}

#[test]
fn traversal_does_not_answer_an_explicit_directory_access_probe() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("file");
    std::fs::write(&path, "contents").unwrap();
    let blocked = BlockDirectory::new(&root);
    with_budget(Some(Budget::after(IO_BUDGET)), || {
        assert!(metadata(&path).unwrap().is_file());
        let error = probe_target(&root).unwrap_err();
        assert!(error.to_string().contains("read_blocked"), "{error}");
        assert_eq!(blocked.1.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn timed_out_traversal_does_not_publish_late_success() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("file");
    std::fs::write(&path, "contents").unwrap();
    let budget = Budget::after(Duration::from_millis(250));
    let caller_budget = budget.clone();
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    struct Finished(Sender<()>);
    impl Drop for Finished {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let caller = std::thread::spawn(move || {
        with_budget(Some(caller_budget), || {
            run_traversal(&path, move |path| {
                let result = std::fs::metadata(path)?;
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                Ok((result, Finished(finished_tx)))
            })
            .err()
            .unwrap()
        })
    });
    let started = started_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap();
    release_tx.send(()).unwrap();
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    started.expect("the stat must finish before holding its result past the deadline");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(timeout_code(&error), "read_blocked");
    with_budget(Some(budget), || assert!(!cached_traversal(&root)));
}

#[cfg(unix)]
#[test]
fn traversal_keeps_symlink_stats_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let file = root.join("file");
    let link = root.join("link");
    std::fs::write(&file, "contents").unwrap();
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let budget = Budget::after(IO_BUDGET);
    with_budget(Some(budget.clone()), || {
        assert!(metadata(&file).unwrap().is_file());
        assert!(metadata(&link).unwrap().is_file());
        assert_eq!(canonicalize(&link).unwrap(), file);
        assert_eq!(budget.measurements(), (3, 3));
    });
}

#[test]
fn delete_does_not_enumerate_project_root_parent() {
    let dir = tempfile::tempdir().unwrap();
    let ancestor = std::fs::canonicalize(dir.path()).unwrap();
    let root = ancestor.join("project");
    std::fs::create_dir(&root).unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.restrict_to_project_root = true;
    config.storage_dir = Some(ancestor.join("store"));
    config.backup.enabled = Some(false);
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let blocked = BlockDirectory::new(&ancestor);
    // Separate requests exercise both the initial root resolution and its memo.
    for name in ["cold", "cached"] {
        let target = root.join(name);
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file.txt"), "delete me").unwrap();
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": name, "command": "delete_file", "files": [target], "recursive": true
        }))
        .unwrap();
        let response = crate::commands::delete_file::handle_delete_file(&request, &ctx);
        assert!(response.success, "{name}: {:?}", response.data);
        assert!(!target.exists());
    }
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

#[test]
fn explicit_directory_probe_still_observes_blocked_access() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = BlockDirectory::new(dir.path());
    let error = with_budget(Some(Budget::after(IO_BUDGET)), || {
        probe_directory(dir.path(), None)
    })
    .unwrap_err();
    assert!(error.to_string().contains("read_blocked"));
    assert_eq!(blocked.1.load(Ordering::Relaxed), 1);
}

#[test]
fn unscheduled_helper_is_queue_timeout_and_never_runs_late() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unscheduled");
    let (reached_tx, reached_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    HELPER_GATES.lock().unwrap().insert(
        path.clone(),
        HelperGate {
            reached: reached_tx,
            release: release_rx,
            finished: finished_tx,
        },
    );
    let fired = Arc::new(AtomicUsize::new(0));
    let work_fired = Arc::clone(&fired);
    let (dropped_tx, dropped_rx) = channel();
    struct Dropped(Sender<()>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let dropped = Dropped(dropped_tx);
    let caller = std::thread::spawn(move || {
        run_with_limiter(
            &path,
            Some(Instant::now() + Duration::from_millis(80)),
            Limiter::new(1),
            move |_| {
                let _dropped = dropped;
                work_fired.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        )
    });
    let reached = reached_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap().unwrap_err();
    release_tx.send(()).unwrap();
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    dropped_rx.recv_timeout(IO_BUDGET).unwrap();
    reached.expect("the helper must reach the scheduling gate");
    assert!(
        error.to_string().contains("read_io_queue_timeout"),
        "{error}"
    );
    assert!(!error.to_string().contains("read_blocked"), "{error}");
    assert_eq!(
        fired.load(Ordering::Relaxed),
        0,
        "expired work must not start after release"
    );
}

#[test]
fn saturated_limiter_is_queue_timeout_and_does_not_start_work() {
    let limiter = Limiter::new(1);
    let _permit = limiter.acquire(Instant::now() + IO_BUDGET).unwrap();
    let fired = Arc::new(AtomicUsize::new(0));
    let work_fired = Arc::clone(&fired);
    let error = run_with_limiter(
        Path::new("queued"),
        Some(Instant::now() + Duration::from_millis(80)),
        limiter,
        move |_| {
            work_fired.fetch_add(1, Ordering::Relaxed);
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("read_io_queue_timeout"),
        "{error}"
    );
    assert!(!error.to_string().contains("read_blocked"), "{error}");
    assert_eq!(fired.load(Ordering::Relaxed), 0);
    let cached_error = ProbeError::from(&error).error();
    let response = access_error("queued", Path::new("queued"), cached_error);
    assert_eq!(
        response.data["code"], "read_io_queue_timeout",
        "{response:?}"
    );
}

#[cfg(unix)]
#[test]
fn queued_time_does_not_consume_running_access_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queued-then-healthy");
    let gate = stall_helper(&path);
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (result_tx, result_rx) = channel();
    let caller = std::thread::spawn(move || {
        result_tx
            .send(with_budget(
                Some(Budget::after(Duration::from_secs(30))),
                || {
                    run_with_timeout(
                        &path,
                        None,
                        Limiter::new(1),
                        Duration::from_secs(6),
                        None,
                        move |_| {
                            started_tx.send(()).unwrap();
                            let _ = release_rx.recv();
                            Ok(42)
                        },
                    )
                },
            ))
            .unwrap();
    });
    gate.reached.recv_timeout(IO_BUDGET).unwrap();
    // A three-second queue leaves ample scheduling margin. Four seconds of
    // running work fit the six-second access cap, but not a cap begun at enqueue.
    assert!(result_rx.recv_timeout(Duration::from_secs(3)).is_err());
    drop(gate);
    let started = started_rx.recv_timeout(IO_BUDGET);
    let early = result_rx.recv_timeout(Duration::from_secs(4));
    let _ = release_tx.send(());
    caller.join().unwrap();
    let result = early.unwrap_or_else(|_| result_rx.recv_timeout(IO_BUDGET).unwrap());
    assert!(
        started.is_ok(),
        "the filesystem operation must actually start: {started:?}; run_with_timeout returned {result:?}"
    );
    assert_eq!(
        result.unwrap(),
        42,
        "queueing must not exhaust the access cap"
    );
}

#[test]
fn running_access_keeps_shorter_request_deadline() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    let caller = std::thread::spawn(move || {
        with_budget(Some(Budget::after(Duration::from_millis(80))), || {
            run_with_timeout(
                Path::new("running-past-request"),
                None,
                Limiter::new(1),
                Duration::from_secs(1),
                None,
                move |_| {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    let _ = finished_tx.send(());
                    Ok(())
                },
            )
        })
    });
    let started = started_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap().unwrap_err();
    let _ = release_tx.send(());
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    started.expect("the filesystem operation must actually start");
    assert_eq!(timeout_code(&error), "read_blocked");
    let message = error.to_string();
    let duration = message
        .split_once("within ")
        .unwrap()
        .1
        .split_once(" s")
        .unwrap()
        .0
        .parse::<f64>()
        .unwrap();
    assert!((0.0..=0.080).contains(&duration), "{error}");
}

#[cfg(unix)]
#[test]
fn queued_time_without_request_budget_does_not_consume_running_access_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queued-without-request-budget");
    let gate = stall_helper(&path);
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let (result_tx, result_rx) = channel();
    let caller = std::thread::spawn(move || {
        assert!(current().is_none(), "this call must have no request budget");
        result_tx
            .send(run_with_timeout(
                &path,
                None,
                Limiter::new(1),
                Duration::from_secs(1),
                None,
                move |_| {
                    started_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                    Ok(42)
                },
            ))
            .unwrap();
    });
    gate.reached.recv_timeout(IO_BUDGET).unwrap();
    assert!(result_rx.recv_timeout(Duration::from_millis(750)).is_err());
    drop(gate);
    let started = started_rx.recv_timeout(IO_BUDGET);
    let early = result_rx.recv_timeout(Duration::from_millis(500));
    let _ = release_tx.send(());
    caller.join().unwrap();
    started.expect("the filesystem operation must actually start");
    let result = early.unwrap_or_else(|_| result_rx.recv_timeout(IO_BUDGET).unwrap());
    assert_eq!(result.unwrap(), 42, "queueing is not a request deadline");
}

#[cfg(unix)]
#[test]
fn shared_probe_waiter_follows_queued_then_running_owner() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let queued = stall_helper(&path);
    let running = stall_running_helper(&path);
    let budget = Budget::after(Duration::from_secs(20));
    let owner_path = path.clone();
    let owner_budget = budget.clone();
    let owner = std::thread::spawn(move || {
        with_budget(Some(owner_budget), || probe_directory(&owner_path, None))
    });
    queued.reached.recv_timeout(IO_BUDGET).unwrap();
    let (waiting_tx, waiting_rx) = channel();
    let (result_tx, result_rx) = channel();
    let waiter_budget = budget.clone();
    let waiter = std::thread::spawn(move || {
        with_budget(Some(waiter_budget), || {
            waiting_tx.send(()).unwrap();
            result_tx.send(probe_directory(&path, None)).unwrap();
        });
    });
    waiting_rx.recv_timeout(IO_BUDGET).unwrap();
    // Neither phase exceeds the access cap, but their sum exceeds the old
    // waiter's fixed five-second allowance.
    assert!(result_rx.recv_timeout(Duration::from_millis(3750)).is_err());
    drop(queued);
    running.reached.recv_timeout(IO_BUDGET).unwrap();
    let early = result_rx.recv_timeout(Duration::from_millis(2500));
    drop(running);
    let owner_result = owner.join().unwrap();
    waiter.join().unwrap();
    let result = early.unwrap_or_else(|_| result_rx.recv_timeout(IO_BUDGET).unwrap());
    result.expect("a shared waiter must receive the healthy owner's result");
    owner_result.unwrap();
    assert_eq!(budget.measurements(), (1, 1), "the probe must be shared");
    assert!(!budget.failed.load(Ordering::Acquire));
}

#[cfg(unix)]
#[test]
fn shared_probe_waiter_keeps_shorter_request_deadline_after_owner_starts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let queued = stall_helper(&path);
    let running = stall_running_helper(&path);
    let budget = Budget::after(Duration::from_secs(10));
    let owner_path = path.clone();
    let owner_budget = budget.clone();
    let owner = std::thread::spawn(move || {
        with_budget(Some(owner_budget), || probe_directory(&owner_path, None))
    });
    queued.reached.recv_timeout(IO_BUDGET).unwrap();
    let (waiting_tx, waiting_rx) = channel();
    let (result_tx, result_rx) = channel();
    let waiter = std::thread::spawn(move || {
        with_budget(Some(budget), || {
            let at = Instant::now() + Duration::from_millis(500);
            waiting_tx.send(at).unwrap();
            result_tx.send(probe_directory(&path, Some(at))).unwrap();
        });
    });
    let at = waiting_rx.recv_timeout(IO_BUDGET).unwrap();
    assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
    drop(queued);
    running.reached.recv_timeout(IO_BUDGET).unwrap();
    let result = result_rx.recv_timeout(Duration::from_secs(2));
    let finished = Instant::now();
    drop(running);
    let _ = owner.join().unwrap();
    waiter.join().unwrap();
    let error = result.unwrap().unwrap_err();
    assert_eq!(timeout_code(&error), "read_io_queue_timeout", "{error}");
    assert!(finished >= at, "the waiter must not expire early");
    assert!(finished < at + Duration::from_secs(1));
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
#[test]
fn shared_probe_waiters_preserve_owner_queue_and_access_timeout_codes() {
    for running in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let gate = if running {
            stall_running_helper(&path)
        } else {
            stall_helper(&path)
        };
        let budget =
            Budget::after(Duration::from_secs(5)).with_access_budget(Duration::from_millis(200));
        let owner_path = path.clone();
        let owner_budget = budget.clone();
        let (owner_tx, owner_rx) = channel();
        let owner = std::thread::spawn(move || {
            let result = with_budget(Some(owner_budget), || probe_directory(&owner_path, None));
            owner_tx.send(result).unwrap();
        });
        gate.reached.recv_timeout(IO_BUDGET).unwrap();
        let (result_tx, result_rx) = channel();
        let waiter = std::thread::spawn(move || {
            with_budget(Some(budget), || {
                result_tx.send(probe_directory(&path, None)).unwrap();
            });
        });
        // A loaded runner can delay the test thread past an arbitrary short
        // grace period while the timed-out helpers are still waiting at this gate.
        // Observe both deadline results before releasing the helper.
        let result = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the shared-probe waiter must observe its deadline");
        let owner_result = owner_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the shared-probe owner must observe its deadline");
        drop(gate);
        owner.join().unwrap();
        waiter.join().unwrap();
        let owner_error = owner_result.unwrap_err();
        let waiter_error = result.unwrap_err();
        let expected = if running {
            "read_blocked"
        } else {
            "read_io_queue_timeout"
        };
        assert_eq!(timeout_code(&owner_error), expected, "{owner_error}");
        assert_eq!(timeout_code(&waiter_error), expected, "{waiter_error}");
    }
}

#[test]
fn running_helper_timeout_has_only_native_platform_hint() {
    let (release_tx, release_rx) = channel();
    let (started_tx, started_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    let caller = std::thread::spawn(move || {
        run_with_limiter(Path::new("running"), None, Limiter::new(1), move |_| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
            finished_tx.send(()).unwrap();
            Ok(())
        })
    });
    let started = started_rx.recv_timeout(IO_BUDGET);
    let error = caller.join().unwrap().unwrap_err();
    release_tx.send(()).unwrap();
    finished_rx.recv_timeout(IO_BUDGET).unwrap();
    started.expect("the helper must start its operation");
    assert!(error.to_string().contains("read_blocked"), "{error}");
    let message = error.to_string();
    let (prefix, duration_and_hint) = message.split_once("within ").unwrap();
    assert_eq!(prefix, "read_blocked: running: OS access did not finish ");
    let (duration, hint) = duration_and_hint.split_once(" s").unwrap();
    assert!(duration.parse::<f64>().unwrap() > 0.0);
    let expected_hint = if cfg!(target_os = "macos") {
        "; on macOS, check ck-aft access in Privacy & Security, then retry"
    } else {
        ""
    };
    assert_eq!(hint, expected_hint);
}

#[cfg(unix)]
fn delete_context(root: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    // Unrestricted validation hands the target path itself to the first
    // helper (the access-policy identity check), so the test gate can key on it.
    config.restrict_to_project_root = false;
    config.storage_dir = Some(root.join(".store"));
    config.backup.enabled = Some(false);
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

/// Runs a recursive delete of a fresh directory while its first path-check
/// helper is held unscheduled for `stall`, then releases it.
#[cfg(unix)]
fn delete_with_unscheduled_helper(
    budget: Option<Budget>,
    stall: Duration,
) -> (crate::protocol::Response, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("file.txt"), "delete me").unwrap();
    let ctx = delete_context(&root);
    let gate = stall_helper(&target);
    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "late-helper", "command": "delete_file", "files": [target], "recursive": true
    }))
    .unwrap();
    let (result_tx, result_rx) = channel();
    let caller = std::thread::spawn(move || {
        let response = with_budget(budget, || {
            crate::commands::delete_file::handle_delete_file(&request, &ctx)
        });
        result_tx.send(response).unwrap();
    });
    gate.reached.recv_timeout(IO_BUDGET).unwrap();
    std::thread::sleep(stall);
    drop(gate);
    let response = result_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    caller.join().unwrap();
    let deleted = !target.exists();
    drop(dir);
    assert_eq!(deleted, response.success, "{:?}", response.data);
    (response, target)
}

// A tool-call delete runs under a long request budget. A helper scheduled
// after the per-access cap has still done nothing, so the delete must wait
// for it rather than refuse with read_io_queue_timeout.
#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
#[test]
fn tool_call_delete_waits_for_a_late_helper_instead_of_refusing() {
    let budget =
        Budget::after(Duration::from_secs(25)).with_access_budget(Duration::from_millis(200));
    let (response, _) = delete_with_unscheduled_helper(Some(budget), Duration::from_millis(800));
    assert!(response.success, "{:?}", response.data);
}

// A native `delete_file` has no caller budget. Hold its helper past the
// five-second access cap, the delay observed on a loaded Windows runner.
#[cfg(unix)]
#[test]
fn native_delete_waits_for_a_helper_scheduled_after_the_access_cap() {
    let (response, _) =
        delete_with_unscheduled_helper(None, IO_BUDGET + Duration::from_millis(500));
    assert!(response.success, "{:?}", response.data);
}

// Waiting for admission must not lengthen OS access: a mutation's helper that
// started and then hangs still fails as read_blocked within the access cap.
#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
#[test]
fn mutation_admission_keeps_the_running_access_cap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("running-mutation-check");
    let gate = stall_running_helper(&path);
    let budget =
        Budget::after(Duration::from_secs(25)).with_access_budget(Duration::from_millis(200));
    let started = Instant::now();
    let error = with_budget(Some(budget), || {
        let _mutation = enter_mutation();
        run(&path, None, |_| Ok(()))
    })
    .unwrap_err();
    let elapsed = started.elapsed();
    drop(gate);
    assert_eq!(timeout_code(&error), "read_blocked", "{error}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

// Reads keep the per-access queue cap: outside a mutation, a helper that is
// not scheduled within one access cap still refuses with
// read_io_queue_timeout.
#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
#[test]
fn read_admission_keeps_the_queue_cap_outside_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queued-read");
    let gate = stall_helper(&path);
    let budget =
        Budget::after(Duration::from_secs(25)).with_access_budget(Duration::from_millis(200));
    let started = Instant::now();
    let error = with_budget(Some(budget), || run(&path, None, |_| Ok(()))).unwrap_err();
    let elapsed = started.elapsed();
    drop(gate);
    assert_eq!(timeout_code(&error), "read_io_queue_timeout", "{error}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}
/// A fixed, in-process read/grep/edit workload, excluding context construction.
/// Process CPU includes helper creation and teardown, not just filesystem calls.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "manual performance measurement"]
fn probe_pool_workload() {
    fn cpu_seconds() -> f64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
            0
        );
        let usage = unsafe { usage.assume_init() };
        (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
    }
    let fixture = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..3)
        .map(|n| fixture.path().join(format!("root-{n}")))
        .collect();
    let contexts: Vec<_> = roots
        .iter()
        .map(|root| {
            for directory in 0..4 {
                let path = root.join(format!("src-{directory}"));
                std::fs::create_dir_all(&path).unwrap();
                for file in 0..8 {
                    std::fs::write(path.join(format!("file-{file}.txt")), "needle alpha\n")
                        .unwrap();
                }
            }
            delete_context(root)
        })
        .collect();
    let spawns = THREAD_SPAWNS.load(Ordering::Relaxed);
    let cpu = cpu_seconds();
    let wall = Instant::now();
    let mut totals = [[0usize; 4]; 3];
    for iteration in 0..100 {
        for (root, ctx) in roots.iter().zip(&contexts) {
            let file = root.join("src-0/file-0.txt");
            for (kind, params) in [
                serde_json::json!({"command": "read", "file": file}),
                serde_json::json!({"command": "grep", "path": root, "pattern": "needle"}),
                serde_json::json!({"command": "edit_match", "file": file, "match": if iteration % 2 == 0 {"alpha"} else {"beta"}, "replacement": if iteration % 2 == 0 {"beta"} else {"alpha"}}),
            ].into_iter().enumerate() {
                let mut params = params;
                params["id"] = serde_json::json!("probe-load");
                let request: RawRequest = serde_json::from_value(params).unwrap();
                let budget = Budget::after(Duration::from_secs(25));
                let response = with_budget(Some(budget.clone()), || command(&request, ctx, |req, ctx| match kind {
                    0 => crate::commands::read::handle_read(req, ctx),
                    1 => crate::commands::grep::handle_grep(req, ctx),
                    _ => crate::commands::edit_match::handle_edit_match(req, ctx),
                }));
                assert!(response.success, "{}: {:?}", request.command, response);
                totals[kind][0] += 1;
                totals[kind][1] += budget.measurements.directory_probes.load(Ordering::Relaxed);
                totals[kind][2] += budget.measurements.traversal_probes.load(Ordering::Relaxed);
                totals[kind][3] += budget.measurements.round_trips.load(Ordering::Relaxed);
            }
        }
    }
    let wall = wall.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu;
    let spawns = THREAD_SPAWNS.load(Ordering::Relaxed) - spawns;
    let probes: usize = totals.iter().map(|row| row[3]).sum();
    println!("PROBE_LOAD calls/directory/traversal/dispatch={totals:?} wall_s={wall:.6} cpu_s={cpu:.6} spawns={spawns} threads_per_s={:.3} cpu_us_per_probe={:.3}", spawns as f64 / wall, cpu * 1_000_000.0 / probes as f64);
}

/// A request that waits on shared work (such as a view's first bind) longer
/// than its whole filesystem deadline still reads afterwards: the wait does
/// not spend the deadline, and a bounded call inside the wait is not refused
/// by it either. The control shows the same wait outside `excluding_wait`
/// refuses the read before it starts.
#[test]
fn excluding_wait_leaves_the_request_deadline_for_the_reads_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let stat = |root: &Path| run(root, None, |path| std::fs::metadata(path).map(|_| ()));
    let budget = Duration::from_secs(1);
    let wait = Duration::from_millis(1500);

    let control = with_budget(Some(Budget::after(budget)), || {
        std::thread::sleep(wait);
        stat(&root)
    })
    .unwrap_err();
    assert!(
        control.to_string().contains("read_deadline_exceeded"),
        "{control}"
    );

    with_budget(Some(Budget::after(budget)), || {
        let inside = excluding_wait(|| {
            std::thread::sleep(wait);
            stat(&root)
        });
        assert!(inside.is_ok(), "{inside:?}");
        assert!(stat(&root).is_ok());
        assert!(current().unwrap().failure.lock().unwrap().is_none());
    });
}
