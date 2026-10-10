//! Live, process-wide index telemetry. Guards own entries, so every return,
//! cancellation and unwind removes work without requiring a successful publish.
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

const LIST_CAP: usize = 20;
const RATE_WINDOW: Duration = Duration::from_secs(300);
static REGISTRY: LazyLock<Arc<Registry>> = LazyLock::new(|| Arc::new(Registry::default()));
thread_local! {
    static CURRENT: RefCell<Option<Arc<Mutex<Entry>>>> = const { RefCell::new(None) };
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    running: BTreeMap<u64, Arc<Mutex<Entry>>>,
    queued: BTreeMap<u64, Arc<Mutex<Entry>>>,
}

#[derive(Debug, Default)]
pub(crate) struct Registry(Mutex<State>);

#[derive(Debug)]
struct Entry {
    root: String,
    root_label: String,
    kind: String,
    phase: &'static str,
    started: Instant,
    started_at_ms: u64,
    done: u64,
    total: Option<u64>,
    chunks_embedded: Option<u64>,
    samples: VecDeque<(Instant, u64)>,
    thread: ThreadId,
    fallback: bool,
    shadowed: bool,
    cancellation: Option<crate::executor::JobCancellation>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Running {
    pub root: String,
    pub root_label: String,
    pub kind: String,
    pub phase: &'static str,
    pub started_at_ms: u64,
    pub done: u64,
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks_embedded: Option<u64>,
    pub rate_per_minute: Option<f64>,
    pub eta_seconds: Option<f64>,
    pub age_seconds: f64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Queued {
    pub root: String,
    pub kind: String,
    pub waited_seconds: f64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Snapshot {
    pub running: Vec<Running>,
    pub queued: Vec<Queued>,
    pub warming_roots: u64,
    #[serde(skip_serializing_if = "Omitted::is_empty")]
    pub omitted: Omitted,
}

#[derive(Debug, Serialize)]
pub(crate) struct Omitted {
    pub running: usize,
    pub queued: usize,
}

impl Omitted {
    fn is_empty(&self) -> bool {
        self.running == 0 && self.queued == 0
    }
}

/// The rate is files/minute in the current phase, measured over the last five
/// minutes. No completed files in that window means no rate and no ETA.
impl Entry {
    fn render(&self, now: Instant) -> Running {
        let baseline = self
            .samples
            .iter()
            .find(|(at, _)| now.saturating_duration_since(*at) <= RATE_WINDOW);
        let rate = baseline.and_then(|(at, done)| {
            let elapsed = now.saturating_duration_since(*at).as_secs_f64();
            let delta = self.done.saturating_sub(*done);
            (elapsed >= 1.0 && delta > 0).then(|| delta as f64 * 60.0 / elapsed)
        });
        Running {
            root: self.root.clone(),
            root_label: self.root_label.clone(),
            kind: self.kind.clone(),
            phase: self.phase,
            started_at_ms: self.started_at_ms,
            done: self.done,
            total: self.total,
            chunks_embedded: self.chunks_embedded,
            rate_per_minute: rate,
            eta_seconds: self
                .total
                .zip(rate)
                .map(|(total, rate)| total.saturating_sub(self.done) as f64 * 60.0 / rate),
            age_seconds: now.saturating_duration_since(self.started).as_secs_f64(),
        }
    }

    fn advance(&mut self, done: u64, now: Instant) {
        // At most one sample per second; no per-file allocation or growing
        // history. Keep the baseline before adding the new completed count.
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > RATE_WINDOW)
        {
            self.samples.pop_front();
        }
        if self
            .samples
            .back()
            .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= Duration::from_secs(1))
        {
            self.samples.push_back((now, self.done));
        }
        self.done = done;
    }
}

#[derive(Debug)]
pub(crate) struct Job {
    registry: Arc<Registry>,
    id: u64,
    queued: bool,
    entry: Arc<Mutex<Entry>>,
}

impl Registry {
    fn register(self: &Arc<Self>, root: &str, kind: &str, queued: bool, fallback: bool) -> Job {
        let now = Instant::now();
        let cancellation = crate::executor::current_job_cancellation();
        let mut samples = VecDeque::with_capacity(301);
        samples.push_back((now, 0));
        let entry = Arc::new(Mutex::new(Entry {
            root: root.to_owned(),
            root_label: root_label(Path::new(root)),
            kind: kind.to_owned(),
            phase: "starting",
            started: now,
            started_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            done: 0,
            total: None,
            chunks_embedded: None,
            samples,
            thread: std::thread::current().id(),
            fallback,
            shadowed: false,
            cancellation: cancellation.clone(),
        }));
        let mut state = lock(&self.0);
        // Detailed work hides this thread's admission-only entry. Keep the
        // latter alive so persistence after a builder returns is still visible.
        // Permit accounting remains entirely in the limiter.
        if !queued && !fallback {
            for old in state.running.values() {
                let mut old = lock(old);
                if old.fallback && old.thread == std::thread::current().id() {
                    old.shadowed = true;
                    old.root = root.to_owned();
                    old.root_label = lock(&entry).root_label.clone();
                    old.kind = kind.to_owned();
                }
            }
        }
        state.next_id += 1;
        let id = state.next_id;
        let entries = if queued {
            &mut state.queued
        } else {
            &mut state.running
        };
        if !cancellation.is_some_and(|token| token.cancel_already_requested()) {
            entries.insert(id, Arc::clone(&entry));
        }
        Job {
            registry: Arc::clone(self),
            id,
            queued,
            entry,
        }
    }

    fn cancel_root(&self, root: &str) {
        let mut state = lock(&self.0);
        state.running.retain(|_, entry| lock(entry).root != root);
        state.queued.retain(|_, entry| lock(entry).root != root);
    }

    fn cancel_token(&self, key: usize) {
        let mut state = lock(&self.0);
        let keep = |_: &u64, entry: &mut Arc<Mutex<Entry>>| {
            !lock(entry)
                .cancellation
                .as_ref()
                .is_some_and(|token| token.progress_key() == key)
        };
        state.running.retain(keep);
        state.queued.retain(keep);
    }

    fn snapshot(&self, warming_roots: u64, now: Instant) -> Snapshot {
        let state = lock(&self.0);
        let running = state.running.values().filter(|entry| !lock(entry).shadowed);
        let running_count = running.clone().count();
        Snapshot {
            running: running
                .take(LIST_CAP)
                .map(|entry| lock(entry).render(now))
                .collect(),
            queued: state
                .queued
                .values()
                .take(LIST_CAP)
                .map(|entry| {
                    let entry = lock(entry);
                    Queued {
                        root: entry.root.clone(),
                        kind: entry.kind.clone(),
                        waited_seconds: now.saturating_duration_since(entry.started).as_secs_f64(),
                    }
                })
                .collect(),
            warming_roots,
            omitted: Omitted {
                running: running_count.saturating_sub(LIST_CAP),
                queued: state.queued.len().saturating_sub(LIST_CAP),
            },
        }
    }
}

fn root_label(root: &Path) -> String {
    let basename = root
        .file_name()
        .unwrap_or(root.as_os_str())
        .to_string_lossy();
    // Alfonso's directories are hash-keyed, so the parent basename is not the
    // repository name. Read only the small worktree gitdir marker at admission,
    // never on health's frame loop and never by spawning git. Matched by path
    // components, not text, so Windows' `\` separators match too.
    let components: Vec<_> = root.components().map(|c| c.as_os_str()).collect();
    let in_alfonso_worktrees = components
        .windows(2)
        .any(|pair| pair[0] == "alfonso" && pair[1] == "worktrees");
    if in_alfonso_worktrees {
        let marker = root.join(".git");
        if std::fs::metadata(&marker).is_ok_and(|m| m.is_file() && m.len() <= 4096) {
            if let Ok(marker) = std::fs::read_to_string(marker) {
                if let Some(gitdir) = marker.trim().strip_prefix("gitdir: ") {
                    let gitdir = root.join(gitdir);
                    if let Some(repo) = gitdir
                        .ancestors()
                        .find(|p| p.file_name().is_some_and(|n| n == ".git"))
                        .and_then(Path::parent)
                        .and_then(Path::file_name)
                    {
                        return format!("{} (worker {})", repo.to_string_lossy(), basename)
                            .replace(['\n', '\r'], " ");
                    }
                }
            }
        }
    }
    basename.replace(['\n', '\r'], " ")
}

impl Drop for Job {
    fn drop(&mut self) {
        let mut state = lock(&self.registry.0);
        let entries = if self.queued {
            &mut state.queued
        } else {
            &mut state.running
        };
        entries.remove(&self.id);
        if !self.queued && !lock(&self.entry).fallback {
            let thread = lock(&self.entry).thread;
            let has_detail = state.running.values().any(|entry| {
                let entry = lock(entry);
                !entry.fallback && entry.thread == thread
            });
            if !has_detail {
                for entry in state.running.values() {
                    let mut entry = lock(entry);
                    if entry.fallback && entry.thread == thread {
                        entry.shadowed = false;
                        entry.phase = "finishing";
                        entry.done = 0;
                        entry.total = None;
                        entry.samples.clear();
                    }
                }
            }
        }
    }
}

pub(crate) fn admitted(root: &str, kind: &str) -> Option<Job> {
    if CURRENT.with(|current| current.borrow().is_some()) {
        return None;
    }
    Some(REGISTRY.register(root, kind, false, true))
}

pub(crate) fn queued(root: &str, kind: &str) -> Job {
    REGISTRY.register(root, kind, true, false)
}

/// Thread-local scope routes embedding counters to the caller's file job even
/// through retry/bisection helpers. It is never sent to a worker pool.
pub(crate) struct Scope {
    _job: Job,
    previous: Option<Arc<Mutex<Entry>>>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

/// Cold builds and full fills announce their start. Incremental maintenance
/// selects Quiet explicitly because it can run every second on a busy root.
#[derive(Clone, Copy)]
pub(crate) enum StartLog {
    Info,
    Quiet,
}

fn start_message(root: &Path, kind: &str, total: Option<usize>, log: StartLog) -> Option<String> {
    if matches!(log, StartLog::Quiet) {
        return None;
    }
    Some(format!(
        "indexing start root={} kind={} total={:?}",
        root.display(),
        kind,
        total
    ))
}

pub(crate) fn start(root: &Path, kind: &str, total: Option<usize>, log: StartLog) -> Scope {
    let job = REGISTRY.register(&root.to_string_lossy(), kind, false, false);
    lock(&job.entry).total = total.map(|n| n as u64);
    if let Some(message) = start_message(root, kind, total, log) {
        crate::slog_info!("{}", message);
    }
    let previous = CURRENT.with(|current| current.replace(Some(Arc::clone(&job.entry))));
    Scope {
        _job: job,
        previous,
        _thread_bound: std::marker::PhantomData,
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.previous.take()));
    }
}

pub(crate) fn phase(phase: &'static str, total: Option<usize>) {
    CURRENT.with(|current| {
        if let Some(entry) = current.borrow().as_ref() {
            let mut entry = lock(entry);
            if entry.phase != phase {
                entry.done = 0;
                entry.samples.clear();
                entry.samples.push_back((Instant::now(), 0));
            }
            entry.phase = phase;
            entry.total = total.map(|n| n as u64);
        }
    });
}

pub(crate) fn advance(files: usize) {
    CURRENT.with(|current| {
        if let Some(entry) = current.borrow().as_ref() {
            let mut entry = lock(entry);
            let done = entry.done.saturating_add(files as u64);
            entry.advance(done, Instant::now());
        }
    });
}

pub(crate) fn remaining(files: usize) {
    CURRENT.with(|current| {
        if let Some(entry) = current.borrow().as_ref() {
            let mut entry = lock(entry);
            entry.total = Some(entry.done.saturating_add(files as u64));
        }
    });
}

pub(crate) fn completed(files: usize) {
    CURRENT.with(|current| {
        if let Some(entry) = current.borrow().as_ref() {
            lock(entry).advance(files as u64, Instant::now());
        }
    });
}

pub(crate) struct Counter(Arc<Mutex<Entry>>);
impl Counter {
    pub(crate) fn advance(&self, files: usize) {
        let mut entry = lock(&self.0);
        let done = entry.done.saturating_add(files as u64);
        entry.advance(done, Instant::now());
    }
}

pub(crate) fn counter() -> Option<Counter> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map(|entry| Counter(Arc::clone(entry)))
    })
}

pub(crate) fn embedded(chunks: usize) {
    CURRENT.with(|current| {
        if let Some(entry) = current.borrow().as_ref() {
            let mut entry = lock(entry);
            entry.chunks_embedded = Some(entry.chunks_embedded.unwrap_or(0) + chunks as u64);
        }
    });
}

pub(crate) fn cancel_root(root: &Path) {
    REGISTRY.cancel_root(&root.to_string_lossy());
}
pub(crate) fn cancel_token(key: usize) {
    REGISTRY.cancel_token(key);
}
pub(crate) fn snapshot(warming_roots: u64) -> Snapshot {
    REGISTRY.snapshot(warming_roots, Instant::now())
}

/// Test fixtures observe only their unique root. The production census keeps
/// its cap, so a parallel test with many older jobs cannot hide a fixture's row.
#[cfg(test)]
pub(crate) fn snapshot_for_root(root: &Path, warming_roots: u64) -> Snapshot {
    let root = root.to_string_lossy();
    let now = Instant::now();
    let state = lock(&REGISTRY.0);
    Snapshot {
        running: state
            .running
            .values()
            .find_map(|entry| {
                let entry = lock(entry);
                (entry.root == root && !entry.shadowed).then(|| entry.render(now))
            })
            .into_iter()
            .collect(),
        queued: state
            .queued
            .values()
            .find_map(|entry| {
                let entry = lock(entry);
                (entry.root == root).then(|| Queued {
                    root: entry.root.clone(),
                    kind: entry.kind.clone(),
                    waited_seconds: now.saturating_duration_since(entry.started).as_secs_f64(),
                })
            })
            .into_iter()
            .collect(),
        warming_roots,
        omitted: Omitted {
            running: 0,
            queued: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_refreshes_do_not_emit_info_start_messages() {
        assert!(start_message(
            Path::new("/repo"),
            "callgraph refresh",
            Some(1),
            StartLog::Quiet
        )
        .is_none());
        assert!(start_message(
            Path::new("/repo"),
            "view publication",
            None,
            StartLog::Quiet
        )
        .is_none());
        assert!(
            start_message(Path::new("/repo"), "callgraph build", None, StartLog::Info)
                .unwrap()
                .contains("indexing start root=/repo kind=callgraph build total=None")
        );
    }

    #[test]
    fn progress_and_recent_rate_are_measured() {
        let registry = Arc::new(Registry::default());
        let job = registry.register("/repo", "semantic fill", false, false);
        let now = lock(&job.entry).started;
        lock(&job.entry).total = Some(100);
        lock(&job.entry).advance(20, now + Duration::from_secs(60));
        let snapshot = registry.snapshot(3, now + Duration::from_secs(60));
        let row = &snapshot.running[0];
        assert_eq!(row.done, 20);
        assert_eq!(row.total, Some(100));
        assert_eq!(row.rate_per_minute, Some(20.0));
        assert_eq!(row.eta_seconds, Some(240.0));
        assert_eq!(row.age_seconds, 60.0);
        assert!(registry
            .snapshot(3, now + RATE_WINDOW + Duration::from_secs(61))
            .running[0]
            .eta_seconds
            .is_none());
    }

    #[test]
    fn unknown_total_and_unmeasured_rate_are_null() {
        let registry = Arc::new(Registry::default());
        let job = registry.register("/repo", "walk", false, false);
        let value = serde_json::to_value(registry.snapshot(0, Instant::now())).unwrap();
        assert!(value["running"][0]["total"].is_null());
        assert!(value["running"][0]["rate_per_minute"].is_null());
        assert!(value["running"][0]["eta_seconds"].is_null());
        let now = lock(&job.entry).started;
        lock(&job.entry).advance(20, now + Duration::from_secs(60));
        let row = registry
            .snapshot(0, now + Duration::from_secs(60))
            .running
            .remove(0);
        assert_eq!(row.rate_per_minute, Some(20.0));
        assert_eq!(row.total, None);
        assert_eq!(
            row.eta_seconds, None,
            "a rate alone does not determine an ETA"
        );
    }

    #[test]
    fn empty_omission_counts_are_not_serialized() {
        let snapshot = Snapshot {
            running: Vec::new(),
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 0,
                queued: 0,
            },
        };
        let value = serde_json::to_value(snapshot).unwrap();
        assert!(value.get("omitted").is_none());

        let snapshot = Snapshot {
            running: Vec::new(),
            queued: Vec::new(),
            warming_roots: 0,
            omitted: Omitted {
                running: 2,
                queued: 3,
            },
        };
        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(value["omitted"]["running"], 2);
        assert_eq!(value["omitted"]["queued"], 3);
    }

    #[test]
    fn worker_checkout_labels_name_the_repository_and_worker() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("prefrontal");
        let gitdir = repository.join(".git/worktrees/bg_1a2b");
        let checkout = fixture.path().join("alfonso/worktrees/task-123/bg_1a2b");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(
            checkout.join(".git"),
            format!("gitdir: {}", gitdir.display()),
        )
        .unwrap();

        assert_eq!(root_label(&checkout), "prefrontal (worker bg_1a2b)");
    }

    #[test]
    fn finished_job_is_removed() {
        let registry = Arc::new(Registry::default());
        let job = registry.register("/repo", "build", false, false);
        assert_eq!(registry.snapshot(0, Instant::now()).running.len(), 1);
        drop(job);
        assert!(registry.snapshot(0, Instant::now()).running.is_empty());
        let permit = registry.register("unknown", "unclassified", false, true);
        let detail = registry.register("/repo", "build", false, false);
        let snapshot = registry.snapshot(0, Instant::now());
        assert_eq!(
            snapshot.running.len(),
            1,
            "admission must not duplicate detailed work"
        );
        assert_eq!(snapshot.running[0].root, "/repo");
        drop(detail);
        let snapshot = registry.snapshot(0, Instant::now());
        assert_eq!(
            snapshot.running.len(),
            1,
            "post-build persistence still holds the permit"
        );
        assert_eq!(snapshot.running[0].phase, "finishing");
        assert_eq!(snapshot.running[0].total, None);
        drop(permit);
        assert!(registry.snapshot(0, Instant::now()).running.is_empty());
    }

    #[test]
    fn cancelled_root_removes_running_and_queued_without_resurrection() {
        let registry = Arc::new(Registry::default());
        let job = registry.register("/repo", "build", false, false);
        let _queued = registry.register("/repo", "fill", true, false);
        let _other = registry.register("/other", "fill", true, false);
        registry.cancel_root("/repo");
        lock(&job.entry).advance(1, Instant::now());
        let snapshot = registry.snapshot(0, Instant::now());
        assert!(snapshot.running.is_empty());
        assert_eq!(snapshot.queued.len(), 1);
        assert_eq!(snapshot.queued[0].root, "/other");
    }

    #[test]
    fn lists_are_capped_and_count_omissions() {
        let registry = Arc::new(Registry::default());
        let mut jobs = Vec::new();
        for _ in 0..23 {
            jobs.push(registry.register("/repo", "build", false, false));
            jobs.push(registry.register("/repo", "build", true, false));
        }
        let snapshot = registry.snapshot(49, Instant::now());
        assert_eq!(snapshot.running.len(), 20);
        assert_eq!(snapshot.queued.len(), 20);
        assert_eq!(snapshot.omitted.running, 3);
        assert_eq!(snapshot.omitted.queued, 3);
        assert_eq!(snapshot.warming_roots, 49);
    }

    #[test]
    fn token_cancellation_removes_scoped_work_immediately() {
        let token = crate::executor::JobCancellation::new();
        let _token = crate::executor::install_job_cancellation(token.clone());
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let _job = start(root, "fill", Some(10), StartLog::Quiet);
        let _queued = queued(&root.to_string_lossy(), "other");
        assert!(snapshot_for_root(root, 0)
            .running
            .iter()
            .any(|job| job.root == root.to_string_lossy()));
        token.request_cancel();
        advance(1);
        assert!(!snapshot_for_root(root, 0)
            .running
            .iter()
            .any(|job| job.root == root.to_string_lossy()));
        assert!(!snapshot_for_root(root, 0)
            .queued
            .iter()
            .any(|job| job.root == root.to_string_lossy()));
    }
}
