use super::*;
use std::sync::mpsc::{channel, Receiver, Sender};

struct HelperGate {
    reached: Sender<()>,
    release: Receiver<()>,
    finished: Sender<()>,
}

static HELPER_GATES: LazyLock<Mutex<HashMap<PathBuf, HelperGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

thread_local! {
    static SINGLE_ACCESS_BUDGET: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

pub(super) fn single_access_budget_override() -> Option<Duration> {
    SINGLE_ACCESS_BUDGET.with(std::cell::Cell::get)
}

/// Runs `work` with a shorter single-call limit, which is also the walk stall
/// window, on this thread only. Both are read on the requesting thread.
pub(crate) fn with_single_access_budget<T>(budget: Duration, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Duration>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SINGLE_ACCESS_BUDGET.with(|budget| budget.set(self.0));
        }
    }
    let _restore = Restore(SINGLE_ACCESS_BUDGET.with(|current| current.replace(Some(budget))));
    work()
}

/// A test hook on the per-directory calls a walk makes under `roots`, or with
/// `entries`, on the per-entry calls inside one directory.
struct WalkHook {
    roots: Vec<PathBuf>,
    entries: bool,
    delay: Duration,
    block_at: Option<usize>,
    /// For an entry hook: block the call for exactly this path (one of `roots`).
    block_exact: bool,
    seen: AtomicUsize,
    /// Paths of the calls made while the walk could still go on, in order.
    before_expiry: Mutex<Vec<PathBuf>>,
    /// Calls made after the walk's deadline passed or its request stopped
    /// waiting: work the walk should no longer be doing.
    after_expiry: AtomicUsize,
    blocked: Mutex<Option<Sender<PathBuf>>>,
    release: Mutex<Option<Receiver<()>>>,
}

static WALK_HOOKS: LazyLock<Mutex<Vec<Arc<WalkHook>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Removes its hook when dropped, and releases a blocked call.
pub(crate) struct WalkHookGuard {
    hook: Arc<WalkHook>,
    release: Option<Sender<()>>,
    /// Receives the path of the call the hook blocked.
    pub(crate) blocked: Receiver<PathBuf>,
}

impl WalkHookGuard {
    pub(crate) fn release(&mut self) {
        self.release.take();
    }
    /// Paths of the hooked calls made before the walk had to stop, in order.
    pub(crate) fn calls_before_expiry(&self) -> Vec<PathBuf> {
        self.hook.before_expiry.lock().unwrap().clone()
    }
    /// How many hooked calls the walk made after it had to stop.
    pub(crate) fn calls_after_expiry(&self) -> usize {
        self.hook.after_expiry.load(Ordering::Relaxed)
    }
    /// Checks the partial result of a walk its deadline stopped: it holds at
    /// least one file, and only files the walk checked before it had to stop.
    /// Returns how many files it checked before then are missing from `found`.
    pub(crate) fn files_missed_before_expiry(&self, found: &[PathBuf]) -> usize {
        let checked = self.calls_before_expiry();
        let checked_files = checked
            .iter()
            .filter(|path| path.is_file())
            .collect::<HashSet<_>>();
        assert!(!found.is_empty(), "the walk kept no file it found");
        for path in found {
            assert!(
                checked_files.contains(path),
                "{} was kept but not checked before the walk had to stop",
                path.display()
            );
        }
        checked_files.len() - found.len()
    }
}

/// A canonical tree of 1,250 entries: 20 files at the root, and 30
/// directories of 40 files each. A walk slowed by 1 ms per entry check takes
/// several seconds over all of it, so a short walk deadline expires early.
pub(crate) fn many_entry_tree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    for file in 0..20 {
        std::fs::write(root.join(format!("root_{file:02}.rs")), "pub fn f() {}\n").unwrap();
    }
    for directory in 0..30 {
        let parent = root.join(format!("directory_{directory:02}"));
        std::fs::create_dir(&parent).unwrap();
        for file in 0..40 {
            std::fs::write(parent.join(format!("file_{file:02}.rs")), "pub fn f() {}\n").unwrap();
        }
    }
    (dir, root)
}

/// Runs a walk on a helper limiter of its own with a single slot, and reports
/// whether that slot was already free when the walk's reply reached the caller.
pub(crate) fn run_walk_in_own_slot<T: Send + 'static>(
    path: &Path,
    deadline: Instant,
    work: impl FnOnce(PathBuf, &WalkControl) -> T + Send + 'static,
) -> (io::Result<WalkEnd<T>>, bool) {
    let limiter = Arc::new(Limiter::new(1));
    let end = run_walk_with_limiter(path, deadline, Arc::clone(&limiter), work);
    let free = *limiter.live.lock().unwrap() == 0;
    (end, free)
}

impl Drop for WalkHookGuard {
    fn drop(&mut self) {
        WALK_HOOKS
            .lock()
            .unwrap()
            .retain(|hook| !Arc::ptr_eq(hook, &self.hook));
        self.release.take();
    }
}

fn walk_hook(
    root: &Path,
    entries: bool,
    delay: Duration,
    block_at: Option<usize>,
    block_exact: bool,
) -> WalkHookGuard {
    let mut roots = vec![root.to_path_buf()];
    if let Ok(canonical) = std::fs::canonicalize(root) {
        roots.push(canonical);
    }
    let (blocked_tx, blocked) = channel();
    let (release, release_rx) = channel();
    let hook = Arc::new(WalkHook {
        roots,
        entries,
        delay,
        block_at,
        block_exact,
        seen: AtomicUsize::new(0),
        before_expiry: Mutex::new(Vec::new()),
        after_expiry: AtomicUsize::new(0),
        blocked: Mutex::new(Some(blocked_tx)),
        release: Mutex::new(Some(release_rx)),
    });
    WALK_HOOKS.lock().unwrap().push(Arc::clone(&hook));
    WalkHookGuard {
        hook,
        release: Some(release),
        blocked,
    }
}

/// Slows every per-directory call a walk makes under `root` by `delay`. The
/// walk keeps making progress, so it is slow, not stalled.
pub(crate) fn slow_walk(root: &Path, delay: Duration) -> WalkHookGuard {
    walk_hook(root, false, delay, None, false)
}

/// Slows every per-entry call a walk makes under `root` by `delay`: each entry
/// read, type check, or filter inside a directory. Each call returns, so the
/// walk is slow, not stalled, however many entries one directory holds.
pub(crate) fn slow_walk_entries(root: &Path, delay: Duration) -> WalkHookGuard {
    walk_hook(root, true, delay, None, false)
}

/// Blocks the per-entry call a walk makes for exactly `path` until the guard
/// is released or dropped: one entry whose check never returns.
pub(crate) fn stall_walk_entry(path: &Path) -> WalkHookGuard {
    walk_hook(path, true, Duration::ZERO, None, true)
}

/// Blocks the `access`th per-directory call a walk makes under `root` until
/// the guard is released or dropped: one call that stops making progress.
pub(crate) fn stall_walk_at(root: &Path, access: usize) -> WalkHookGuard {
    walk_hook(root, false, Duration::ZERO, Some(access), false)
}

pub(super) fn before_walk_access(path: &Path) {
    run_walk_hook(path, false);
}

pub(super) fn before_walk_entry(path: &Path) {
    run_walk_hook(path, true);
}

fn run_walk_hook(path: &Path, entries: bool) {
    let hook = WALK_HOOKS
        .lock()
        .unwrap()
        .iter()
        .find(|hook| {
            hook.entries == entries && hook.roots.iter().any(|root| path.starts_with(root))
        })
        .cloned();
    let Some(hook) = hook else {
        return;
    };
    let seen = hook.seen.fetch_add(1, Ordering::Relaxed) + 1;
    // Hooked calls run on the walk's helper thread, where its control is set.
    let expired = WALK.with(|walk| walk.borrow().as_ref().is_some_and(|walk| walk.expired()));
    if expired {
        hook.after_expiry.fetch_add(1, Ordering::Relaxed);
    } else {
        hook.before_expiry.lock().unwrap().push(path.to_path_buf());
    }
    let block = if hook.block_exact {
        hook.roots.iter().any(|root| path == root)
    } else {
        hook.block_at == Some(seen)
    };
    if block {
        if let Some(blocked) = hook.blocked.lock().unwrap().take() {
            let _ = blocked.send(path.to_path_buf());
        }
        let release = hook.release.lock().unwrap().take();
        if let Some(release) = release {
            let _ = release.recv_timeout(Duration::from_secs(30));
        }
    }
    if !hook.delay.is_zero() {
        std::thread::sleep(hook.delay);
    }
}

static BLOCKED_STATS: LazyLock<Mutex<HashMap<PathBuf, (Sender<()>, Receiver<()>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Blocks the next single `stat` of `path` until dropped.
pub(crate) struct StatBlock {
    path: PathBuf,
    pub(crate) reached: Receiver<()>,
    release: Option<Sender<()>>,
}

impl Drop for StatBlock {
    fn drop(&mut self) {
        BLOCKED_STATS.lock().unwrap().remove(&self.path);
        self.release.take();
    }
}

pub(crate) fn block_stat(path: &Path) -> StatBlock {
    let (reached_tx, reached) = channel();
    let (release, release_rx) = channel();
    BLOCKED_STATS
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (reached_tx, release_rx));
    StatBlock {
        path: path.to_path_buf(),
        reached,
        release: Some(release),
    }
}

pub(super) fn before_stat(path: &Path) {
    let blocked = BLOCKED_STATS.lock().unwrap().remove(path);
    if let Some((reached, release)) = blocked {
        let _ = reached.send(());
        let _ = release.recv_timeout(Duration::from_secs(30));
    }
}

#[cfg(unix)]
pub(crate) struct HelperStall {
    path: PathBuf,
    pub(crate) reached: Receiver<()>,
    release: Sender<()>,
    finished: Receiver<()>,
}

#[cfg(unix)]
impl Drop for HelperStall {
    fn drop(&mut self) {
        HELPER_GATES.lock().unwrap().remove(&self.path);
        if self.release.send(()).is_ok() {
            let _ = self.finished.recv_timeout(IO_BUDGET);
        }
    }
}

#[cfg(unix)]
pub(crate) fn stall_helper(path: &Path) -> HelperStall {
    let (reached, reached_rx) = channel();
    let (release, release_rx) = channel();
    let (finished, finished_rx) = channel();
    HELPER_GATES.lock().unwrap().insert(
        path.to_path_buf(),
        HelperGate {
            reached,
            release: release_rx,
            finished,
        },
    );
    HelperStall {
        path: path.to_path_buf(),
        reached: reached_rx,
        release,
        finished: finished_rx,
    }
}

static BLOCKED_DIRECTORIES: LazyLock<Mutex<HashMap<PathBuf, Arc<AtomicUsize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn before_helper(path: &Path) {
    let gate = HELPER_GATES.lock().unwrap().remove(path);
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
            Arc::new(Limiter::new(1)),
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
    let limiter = Arc::new(Limiter::new(1));
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

#[test]
fn running_helper_timeout_has_only_native_platform_hint() {
    let (release_tx, release_rx) = channel();
    let (started_tx, started_rx) = channel();
    let (finished_tx, finished_rx) = channel();
    let caller = std::thread::spawn(move || {
        run_with_limiter(
            Path::new("running"),
            None,
            Arc::new(Limiter::new(1)),
            move |_| {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                finished_tx.send(()).unwrap();
                Ok(())
            },
        )
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

#[test]
fn walk_deadline_is_not_cut_to_the_single_access_limit() {
    let limiter = Arc::new(Limiter::new(1));
    let budget = Budget::after(Duration::from_secs(30));
    let requested = Instant::now() + Duration::from_secs(20);
    // The walk keeps moving for three times the single-call limit. A healthy
    // walk that outlasts one call's limit must still finish.
    let end = with_single_access_budget(Duration::from_millis(100), || {
        with_budget(Some(budget.clone()), || {
            run_walk_with_limiter(Path::new("slow-walk"), requested, limiter, |_, control| {
                let started = Instant::now();
                while started.elapsed() < Duration::from_millis(300) {
                    if control.advance() {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Some(control.deadline())
            })
        })
    });
    let Ok(WalkEnd::Finished(Some(deadline))) = end else {
        panic!("a walk that kept making progress was stopped or reported as blocked");
    };
    assert_eq!(
        deadline, requested,
        "the walk keeps the deadline it asked for"
    );
    assert!(budget.failure.lock().unwrap().is_none());
}

#[test]
fn request_budget_still_bounds_a_walks_explicit_deadline() {
    let limiter = Arc::new(Limiter::new(1));
    let budget = Budget::after(Duration::from_millis(400));
    let started = Instant::now();
    let end = with_budget(Some(budget.clone()), || {
        run_walk_with_limiter(
            Path::new("long-walk"),
            Instant::now() + Duration::from_secs(30),
            limiter,
            |_, control| {
                let mut entries = 0usize;
                while !control.advance() {
                    entries += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
                (entries, control.deadline())
            },
        )
    });
    let Ok(WalkEnd::Finished((entries, deadline))) = end else {
        panic!("the walk must stop itself at its deadline and return");
    };
    assert!(entries > 0);
    assert!(
        deadline < budget.deadline,
        "the walk's deadline leaves reply time inside the request budget"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(budget.failure.lock().unwrap().is_none());
}

#[test]
fn stalled_walk_names_the_stalled_path_and_its_stop_flag_frees_the_slot() {
    let limiter = Arc::new(Limiter::new(4));
    let (blocked_tx, blocked_rx) = channel();
    let (release_tx, release_rx) = channel::<()>();
    let budget = Budget::after(Duration::from_secs(30));
    let walker_limiter = Arc::clone(&limiter);
    let end = with_single_access_budget(Duration::from_millis(150), || {
        with_budget(Some(budget.clone()), || {
            run_walk_with_limiter(
                Path::new("walk-root"),
                Instant::now() + Duration::from_secs(30),
                walker_limiter,
                move |_, control| {
                    control.advance();
                    control.reached(Path::new("walk-root/stuck"));
                    blocked_tx.send(()).unwrap();
                    let _ = release_rx.recv_timeout(Duration::from_secs(10));
                    // Once the stuck call returns, the walk would go on to its
                    // own 30 s deadline unless the request told it to stop.
                    while !control.advance() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                },
            )
        })
    });
    blocked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let Ok(WalkEnd::Stopped {
        stall: Some(stall), ..
    }) = end
    else {
        panic!("a walk stuck in one call must be reported as stalled");
    };
    assert_eq!(stall.path(), Path::new("walk-root/stuck"));
    assert!(
        budget.failure.lock().unwrap().is_none(),
        "the caller decides how to record the stall"
    );
    assert_eq!(
        *limiter.live.lock().unwrap(),
        1,
        "the stuck walk holds its slot"
    );
    release_tx.send(()).unwrap();
    let released = Instant::now();
    while *limiter.live.lock().unwrap() != 0 {
        assert!(
            released.elapsed() < Duration::from_secs(2),
            "the abandoned walk kept its helper slot after the request ended"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stall_record_keeps_partial_results_or_fails_the_reply() {
    let stall = WalkStall {
        path: PathBuf::from("root/stuck"),
        waited: Duration::from_secs(5),
    };
    let budget = Budget::after(Duration::from_secs(5));
    let response = with_budget(Some(budget.clone()), || {
        stall.record(true);
        assert!(
            prior_failure().is_none(),
            "later calls in the request still run"
        );
        // The router finishes the reply twice; the stall is named once.
        let once = finish_response(Response::success(
            "walk",
            serde_json::json!({"text": "1 file matching *.rs", "complete": true}),
        ));
        finish_response(once)
    });
    let expected = format!(
        "read_blocked: {}: {}",
        Path::new("root/stuck").display(),
        blocked_reason(Duration::from_secs(5))
    );
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["complete"], false);
    assert_eq!(
        response.data["text"],
        format!("1 file matching *.rs\n\n{expected}")
    );
    assert_eq!(
        response.data["gaps"],
        serde_json::json!([{"kind": "read_blocked", "path": "root/stuck", "reason": expected}])
    );

    let budget = Budget::after(Duration::from_secs(5));
    let response = with_budget(Some(budget), || {
        stall.record(false);
        finish_response(Response::success("walk", serde_json::json!({"text": ""})))
    });
    assert!(!response.success);
    assert_eq!(response.data["code"], "read_blocked");
    assert_eq!(response.data["message"], expected);
}
