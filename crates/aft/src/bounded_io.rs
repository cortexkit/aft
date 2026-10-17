//! Bound first access to a protection/filesystem boundary, not every file read.
//!
//! A request probes each directory once. Ordinary files in accessible local
//! directories stay on the caller's hot path. Network/removable/unknown volumes
//! and symlinks retain per-file isolation. Helpers own no actor or epoch guard.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub(crate) const IO_BUDGET: Duration = Duration::from_secs(5);
const MAX_IO_THREADS: usize = 64;
static NEXT_BUDGET: AtomicU64 = AtomicU64::new(1);
static HELPERS: LazyLock<Arc<Limiter>> = LazyLock::new(|| Arc::new(Limiter::new(MAX_IO_THREADS)));
#[cfg(test)]
#[derive(Default)]
struct Measurements {
    spawns: AtomicUsize,
    round_trips: AtomicUsize,
}

#[derive(Clone, Debug)]
struct Failure {
    code: &'static str,
    message: String,
    /// Set when a walk stalled on this path after it had already found
    /// results. The reply keeps those results and names the path instead of
    /// becoming an error; see `finish_response`.
    partial: Option<PathBuf>,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

/// Keep admission timeouts distinct when callers turn I/O errors into responses.
pub(crate) fn timeout_code(error: &io::Error) -> &'static str {
    error
        .get_ref()
        .and_then(|error| error.downcast_ref::<Failure>())
        .map_or("read_blocked", |failure| failure.code)
}

#[derive(Clone)]
struct ProbeError {
    kind: io::ErrorKind,
    message: String,
    raw_os_error: Option<i32>,
    failure: Option<Failure>,
}
impl ProbeError {
    fn from(error: &io::Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
            raw_os_error: error.raw_os_error(),
            failure: error
                .get_ref()
                .and_then(|error| error.downcast_ref::<Failure>())
                .cloned(),
        }
    }
    fn error(&self) -> io::Error {
        if let Some(failure) = &self.failure {
            return io::Error::new(self.kind, failure.clone());
        }
        self.raw_os_error
            .map(io::Error::from_raw_os_error)
            .unwrap_or_else(|| io::Error::new(self.kind, self.message.clone()))
    }
}
#[derive(Clone, Copy)]
struct Access {
    direct: bool,
}
struct Probe {
    result: Mutex<Option<Result<Access, ProbeError>>>,
    ready: Condvar,
}
#[derive(Clone)]
pub(crate) struct Budget {
    id: u64,
    pub(crate) deadline: Instant,
    failure: Arc<Mutex<Option<Failure>>>,
    failed: Arc<AtomicBool>,
    directories: Arc<Mutex<HashMap<PathBuf, Arc<Probe>>>>,
    traversable: Arc<Mutex<HashSet<PathBuf>>>,
    permissions: Arc<Mutex<Vec<PermissionObservation>>>,
    #[cfg(test)]
    measurements: Arc<Measurements>,
}
impl Budget {
    pub(crate) fn capped(&self, deadline: Instant) -> Self {
        let mut budget = self.clone();
        budget.deadline = budget.deadline.min(deadline);
        budget
    }
    pub(crate) fn after(duration: Duration) -> Self {
        Self {
            id: NEXT_BUDGET.fetch_add(1, Ordering::Relaxed),
            deadline: Instant::now() + duration,
            failure: Arc::new(Mutex::new(None)),
            failed: Arc::new(AtomicBool::new(false)),
            directories: Arc::new(Mutex::new(HashMap::new())),
            traversable: Arc::new(Mutex::new(HashSet::new())),
            permissions: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            measurements: Arc::new(Measurements::default()),
        }
    }
    pub(crate) fn permissions(&self) -> Vec<PermissionObservation> {
        self.permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    #[cfg(test)]
    pub(crate) fn measurements(&self) -> (usize, usize) {
        (
            self.measurements.spawns.load(Ordering::Relaxed),
            self.measurements.round_trips.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PermissionObservation {
    pub(crate) folder: PathBuf,
    pub(crate) readable: bool,
}

fn observe_permission(path: &Path, result: &io::Result<Access>, macos: bool) {
    if !macos {
        return;
    }
    let readable = match result {
        Ok(_) => true,
        Err(error) if error.raw_os_error() == Some(1) => false,
        _ => return,
    };
    if let Some(budget) = current() {
        budget
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(PermissionObservation {
                folder: permission_folder(path),
                readable,
            });
    }
}

// macOS privacy permissions apply to the enclosing protected folder, not each
// descendant directory.
// This is only a display/deduplication identity; it never changes path policy.
fn permission_folder(path: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        for folder in [
            "Desktop",
            "Documents",
            "Downloads",
            "Pictures",
            "Music",
            "Movies",
        ] {
            let folder = home.join(folder);
            if path.starts_with(&folder) {
                return folder;
            }
        }
    }
    path.to_path_buf()
}

/// The helper only probes access. The caller may mutate only after this returns.
pub(crate) fn probe_access(req_id: &str, path: &Path) -> Result<(), Response> {
    // A new target may have several nonexistent ancestors. Probe the nearest
    // existing directory without creating any of those missing components.
    let result = probe_target(path);
    match result {
        Ok(()) => Ok(()),
        Err(error) if missing_probe_target(&error, path, cfg!(windows)) => {
            let mut parent = path.parent();
            while let Some(folder) = parent {
                match probe_directory(folder, None) {
                    Ok(()) => return Ok(()),
                    Err(error) if missing_probe_target(&error, folder, cfg!(windows)) => {
                        parent = folder.parent()
                    }
                    Err(error) => return Err(access_error(req_id, folder, error)),
                }
            }
            Ok(())
        }
        Err(error) => Err(access_error(req_id, path, error)),
    }
}

fn missing_probe_target(error: &io::Error, path: &Path, windows: bool) -> bool {
    error.kind() == io::ErrorKind::NotFound
        // Win32 rejects a literal glob with ERROR_INVALID_NAME (123). Probe its
        // existing ancestor; the handler expands and validates concrete files.
        // Access-denied errors and ordinary invalid names still fail closed.
        || (windows
            && error.raw_os_error() == Some(123)
            && path.components().any(|component| {
                matches!(component, std::path::Component::Normal(name)
                    if name.to_string_lossy().contains(['*', '?', '[', ']', '{', '}']))
            }))
}

fn access_error(req_id: &str, path: &Path, error: io::Error) -> Response {
    let code = if cfg!(target_os = "macos") && error.raw_os_error() == Some(1) {
        let folder = current()
            .and_then(|budget| {
                budget
                    .permissions()
                    .into_iter()
                    .find(|p| !p.readable)
                    .map(|p| p.folder)
            })
            .unwrap_or_else(|| permission_folder(path.parent().unwrap_or(path)));
        observe_permission(&folder, &Err(io::Error::from_raw_os_error(1)), true);
        return Response::error_with_data(req_id, "macos_permission_denied", format!("macOS denied access to {}; grant ck-aft access in System Settings → Privacy & Security, then retry", folder.display()), serde_json::json!({"folder":folder,"program":"ck-aft"}));
    } else if error.kind() == io::ErrorKind::TimedOut {
        timeout_code(&error)
    } else {
        "path_access_denied"
    };
    Response::error_with_data(
        req_id,
        code,
        format!("{}: {error}", path.display()),
        serde_json::json!({"path":path,"os_error":error.raw_os_error()}),
    )
}

pub(crate) fn finish_response(response: Response) -> Response {
    if matches!(
        response.data["code"].as_str(),
        Some("denied_floor" | "path_outside_root" | "access_read_only")
    ) {
        return response;
    }
    if let Some(folder) = current().and_then(|budget| {
        budget
            .permissions()
            .into_iter()
            .find(|p| !p.readable)
            .map(|p| p.folder)
    }) {
        return Response::error_with_data(&response.id, "macos_permission_denied", format!("macOS denied access to {}; grant ck-aft access in System Settings → Privacy & Security, then retry", folder.display()), serde_json::json!({"folder":folder,"program":"ck-aft"}));
    }
    let error = current().and_then(|budget| {
        budget
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    });
    match error {
        Some(Failure {
            code,
            message,
            partial: Some(path),
        }) if response.success => keep_partial_reply(response, code, &path, &message),
        Some(error) => Response::error(&response.id, error.code, error.message),
        None => response,
    }
}

/// A walk that stalled after finding results keeps them: the reply is marked
/// incomplete, names the stalled path as a gap, and ends with the same
/// `read_blocked` sentence an error reply would carry. `finish_response` can run
/// more than once on one reply, so this never adds the gap or the line twice.
fn keep_partial_reply(mut response: Response, code: &str, path: &Path, message: &str) -> Response {
    if let Some(object) = response.data.as_object_mut() {
        object.insert("complete".into(), serde_json::Value::Bool(false));
        let gap = serde_json::json!({"kind": code, "path": path, "reason": message});
        let gaps = object
            .entry("gaps")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let Some(gaps) = gaps.as_array_mut() {
            if !gaps.contains(&gap) {
                gaps.push(gap);
            }
        }
        if let Some(serde_json::Value::String(text)) = object.get_mut("text") {
            if !text.ends_with(message) {
                if !text.is_empty() {
                    text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
                }
                text.push_str(message);
            }
        }
    }
    response
}
thread_local! {
    static CURRENT: RefCell<Option<Budget>> = const { RefCell::new(None) };
    // Rayon shares the request's single-flight probes, but does not contend on
    // that map per file. Its workers remember successful lookups locally.
    static DIRECTORIES: RefCell<(u64, HashMap<PathBuf, Access>)> = RefCell::new((0, HashMap::new()));
    static HELPER_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    // Set only on a walk's helper thread; see `run_walk`.
    static WALK: RefCell<Option<Arc<WalkControl>>> = const { RefCell::new(None) };
}
pub(crate) struct Scope(Option<Budget>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}
pub(crate) fn current() -> Option<Budget> {
    CURRENT.with(|current| current.borrow().clone())
}
pub(crate) fn enter_if_absent(duration: Duration) -> Scope {
    let budget = current().unwrap_or_else(|| Budget::after(duration));
    Scope(CURRENT.with(|current| current.replace(Some(budget))))
}
pub(crate) fn with_budget<T>(budget: Option<Budget>, work: impl FnOnce() -> T) -> T {
    let _scope = enter(budget);
    work()
}
pub(crate) fn enter(budget: Option<Budget>) -> Scope {
    Scope(CURRENT.with(|current| current.replace(budget)))
}
/// The longest one filesystem call may take, and the longest a walk may go
/// without progress before it counts as stalled.
fn single_access_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = regression_tests::single_access_budget_override() {
        return budget;
    }
    IO_BUDGET
}
fn deadline(at: Option<Instant>) -> Instant {
    at.into_iter()
        .chain(CURRENT.with(|budget| budget.borrow().as_ref().map(|b| b.deadline)))
        .fold(Instant::now() + single_access_budget(), Instant::min)
}
fn failure_message(code: &str, path: &Path, reason: &str) -> String {
    format!("{code}: {}: {reason}", path.display())
}
/// The reason given when one OS call did not return in time.
fn blocked_reason(waited: Duration) -> String {
    let hint = if cfg!(target_os = "macos") {
        "; on macOS, check ck-aft access in Privacy & Security, then retry"
    } else {
        ""
    };
    format!(
        "OS access did not finish within {:.3} s{hint}",
        waited.as_secs_f64()
    )
}
fn failure(path: &Path, code: &'static str, reason: &str) -> io::Error {
    let message = failure_message(code, path, reason);
    if let Some(budget) = current() {
        let mut recorded = budget.failure.lock().unwrap_or_else(|e| e.into_inner());
        // The first failure wins, except over a stall whose results were kept:
        // a later failure makes the whole reply an error.
        if recorded
            .as_ref()
            .is_none_or(|failure| failure.partial.is_some())
        {
            *recorded = Some(Failure {
                code,
                message: message.clone(),
                partial: None,
            });
        }
        budget.failed.store(true, Ordering::Release);
    }
    io::Error::new(
        io::ErrorKind::TimedOut,
        Failure {
            code,
            message,
            partial: None,
        },
    )
}
fn prior_failure() -> Option<io::Error> {
    CURRENT.with(|current| {
        let current = current.borrow();
        let budget = current.as_ref()?;
        if !budget.failed.load(Ordering::Acquire) {
            return None;
        }
        let error = budget
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|failure| io::Error::new(io::ErrorKind::TimedOut, failure.clone()));
        error
    })
}
pub(crate) fn command(
    req: &RawRequest,
    ctx: &AppContext,
    work: impl FnOnce(&RawRequest, &AppContext) -> Response,
) -> Response {
    let duration = if req.command == "grep" {
        crate::grep_executor::FALLBACK_WALK_BUDGET
    } else {
        IO_BUDGET
    };
    let _scope = enter_if_absent(duration);
    // Probe explicit target roots before canonicalization/stat can cross a
    // protected location. Files themselves are never opened by this probe.
    let root = ctx.config().project_root.clone().unwrap_or_default();
    let mut inputs = Vec::new();
    for key in ["path", "file", "directory", "target", "files"] {
        match req.params.get(key) {
            Some(serde_json::Value::String(path)) => inputs.push(path.as_str()),
            Some(serde_json::Value::Array(paths)) => {
                inputs.extend(paths.iter().filter_map(|p| p.as_str()))
            }
            _ => {}
        }
    }
    if !root.as_os_str().is_empty() {
        inputs.push(root.to_str().unwrap_or_default());
    }
    inputs.sort_unstable();
    inputs.dedup();
    let mut probe_error = None;
    for input in inputs {
        if input.starts_with("http://") || input.starts_with("https://") || input.contains("://") {
            continue;
        }
        let path = if Path::new(input).is_absolute() {
            PathBuf::from(input)
        } else {
            root.join(input)
        };
        if let Err(error) = probe_target(&path) {
            // Missing paths still get the handler's normal validation/error.
            if error.kind() == io::ErrorKind::TimedOut {
                probe_error = Some(error);
                break;
            }
        }
    }
    let response = match probe_error {
        Some(error) => Response::error(&req.id, "read_deadline_exceeded", error.to_string()),
        None => work(req, ctx),
    };
    finish_response(response)
}

struct Limiter {
    live: Mutex<usize>,
    available: Condvar,
    max: usize,
}
impl Limiter {
    fn new(max: usize) -> Self {
        Self {
            live: Mutex::new(0),
            available: Condvar::new(),
            max,
        }
    }
    fn acquire(self: &Arc<Self>, at: Instant) -> Option<Permit> {
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        while *live >= self.max {
            let remaining = at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            live = self
                .available
                .wait_timeout(live, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if Instant::now() >= at {
            return None;
        }
        *live += 1;
        Some(Permit(Arc::clone(self)))
    }
}
struct Permit(Arc<Limiter>);
impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.live.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.available.notify_one();
    }
}
/// Spawn only for boundary probes or reads on risky filesystems; whole walks use
/// `run_walk`. A stuck call retains its limiter permit, never executor/root ownership.
pub(crate) fn run<T: Send + 'static>(
    path: &Path,
    at: Option<Instant>,
    work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    run_with_limiter(path, at, Arc::clone(&HELPERS), work)
}
fn run_with_limiter<T: Send + 'static>(
    path: &Path,
    at: Option<Instant>,
    limiter: Arc<Limiter>,
    work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    if let Some(error) = prior_failure() {
        return Err(error);
    }
    // A walk's helper makes its per-directory calls inline instead of taking
    // a helper slot per directory. The request thread watches the walk's
    // progress, so one of these calls that stops returning is still reported,
    // under its own path.
    if let Some(walk) = WALK.with(|walk| walk.borrow().clone()) {
        if walk.expired() {
            // Not a request failure: the walk stops here and reports itself
            // truncated.
            walk.cut.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the walk reached its deadline before this call",
            ));
        }
        walk.reached(path);
        let result = work(path.to_path_buf());
        walk.step();
        return result;
    }
    let started = Instant::now();
    let at = deadline(at);
    if started >= at {
        return Err(failure(
            path,
            "read_deadline_exceeded",
            "filesystem deadline elapsed before this call started",
        ));
    }
    // A bounded walk already owns an isolated helper. Do not consume another
    // slot per nested directory probe (or let healthy walks deadlock each
    // other on admission). Its caller's deadline must be no later than ours.
    if HELPER_DEADLINE.with(|outer| outer.get().is_some_and(|outer| outer <= at)) {
        return work(path.to_path_buf());
    }
    let Some(permit) = limiter.acquire(at) else {
        return Err(failure(path, "read_io_queue_timeout", "no filesystem helper slot became available before the deadline; this call did not start"));
    };
    let owned = path.to_path_buf();
    let budget = current();
    #[cfg(test)]
    let measurements = budget.as_ref().map(|b| Arc::clone(&b.measurements));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let running = Arc::new(Mutex::new(false));
    let helper_running = Arc::clone(&running);
    #[cfg(test)]
    if let Some(counts) = &measurements {
        counts.round_trips.fetch_add(1, Ordering::Relaxed);
    }
    std::thread::Builder::new()
        .name("aft-filesystem-probe".into())
        .spawn(move || {
            let _permit = permit;
            #[cfg(test)]
            regression_tests::before_helper(&owned);
            HELPER_DEADLINE.with(|deadline| deadline.set(Some(at)));
            let result = with_budget(budget, || {
                // Owning a slot does not mean the OS has scheduled this helper.
                // Never start expired work or blame its scheduling delay on I/O.
                let mut running = helper_running.lock().unwrap_or_else(|e| e.into_inner());
                if Instant::now() >= at {
                    return Err(failure(&owned, "read_io_queue_timeout", "filesystem helper did not start before the deadline; this call did not start"));
                }
                *running = true;
                drop(running);
                work(owned)
            });
            let _ = tx.send(result);
        })?;
    #[cfg(test)]
    if let Some(counts) = &measurements {
        counts.spawns.fetch_add(1, Ordering::Relaxed);
    }
    match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            if !*running.lock().unwrap_or_else(|e| e.into_inner()) {
                return Err(failure(
                    path,
                    "read_io_queue_timeout",
                    "filesystem helper did not start before the deadline; this call did not start",
                ));
            }
            Err(failure(
                path,
                "read_blocked",
                &blocked_reason(at.saturating_duration_since(started)),
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("filesystem helper stopped"))
        }
    }
}

/// Reply time a walk leaves inside the request budget for the work after it
/// (sorting, presence checks, formatting).
const WALK_REPLY_MARGIN: Duration = Duration::from_millis(500);

/// State shared by a walk's helper thread and the request thread waiting on it.
///
/// The request thread does not time the walk as a whole. The walker stops
/// itself at `deadline` and reports a truncated walk. The waiting thread
/// reports `read_blocked` only when the walk makes no progress for the stall
/// window, which means one filesystem call inside it has not returned.
pub(crate) struct WalkControl {
    started: Instant,
    deadline: Instant,
    /// Set by the request thread when it stops waiting, so an abandoned walk
    /// exits at its next entry and frees its helper slot.
    stop: AtomicBool,
    /// Set when a call inside the walk was refused because the walk's
    /// deadline had passed; the walk did not finish.
    cut: AtomicBool,
    entries: AtomicUsize,
    /// Time of the last progress, in nanoseconds after `started`.
    progress: AtomicU64,
    /// The path the walk last started work on, named if the walk stalls.
    current: Mutex<PathBuf>,
}
impl WalkControl {
    fn new(deadline: Instant, path: &Path) -> Self {
        Self {
            started: Instant::now(),
            deadline,
            stop: AtomicBool::new(false),
            cut: AtomicBool::new(false),
            entries: AtomicUsize::new(0),
            progress: AtomicU64::new(0),
            current: Mutex::new(path.to_path_buf()),
        }
    }
    /// Records one more walked entry. Returns true when the walk must stop now:
    /// its deadline passed or the request stopped waiting for it.
    pub(crate) fn advance(&self) -> bool {
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.step();
        self.expired()
    }
    pub(crate) fn expired(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }
    /// Names the path the walk is about to work on, such as a directory it is
    /// about to open.
    pub(crate) fn reached(&self, path: &Path) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        current.clear();
        current.push(path);
        drop(current);
        self.step();
        #[cfg(test)]
        regression_tests::before_walk_access(path);
    }
    /// True when a call inside the walk was refused at the walk's deadline.
    pub(crate) fn cut_short(&self) -> bool {
        self.cut.load(Ordering::Relaxed)
    }
    pub(crate) fn entries(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    /// A control for calling a walker directly in a test, outside `run_walk`.
    #[cfg(test)]
    pub(crate) fn unbounded_for_test(path: &Path) -> Self {
        Self::new(Instant::now() + Duration::from_secs(3600), path)
    }
    fn step(&self) {
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.progress.fetch_max(elapsed, Ordering::Relaxed);
    }
    fn last_progress(&self) -> Instant {
        self.started + Duration::from_nanos(self.progress.load(Ordering::Relaxed))
    }
}

/// How a walk ended, as seen by the request thread.
pub(crate) enum WalkEnd<T> {
    /// The walk returned, complete or stopped at its own bounds.
    Finished(T),
    /// The request stopped waiting and told the walk to stop. The caller keeps
    /// whatever the walk had already shared with it, and reports the walk
    /// truncated. `stall` is set when one call made no progress for the stall
    /// window.
    Stopped {
        stall: Option<WalkStall>,
        entries_visited: usize,
    },
}

/// One filesystem call inside a walk that stopped making progress.
pub(crate) struct WalkStall {
    path: PathBuf,
    waited: Duration,
}
impl WalkStall {
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    /// Records the stall as the request's `read_blocked` failure. With
    /// `kept_partial`, the caller keeps results the walk found before it
    /// stalled, and the reply stays a partial success that names the stalled
    /// path; otherwise the reply becomes the error.
    pub(crate) fn record(&self, kept_partial: bool) {
        let reason = blocked_reason(self.waited);
        if !kept_partial {
            let _ = failure(&self.path, "read_blocked", &reason);
            return;
        }
        if let Some(budget) = current() {
            // `failed` stays clear: later calls in this request still run,
            // and any of them that fails turns the reply into its error.
            budget
                .failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| Failure {
                    code: "read_blocked",
                    message: failure_message("read_blocked", &self.path, &reason),
                    partial: Some(self.path.clone()),
                });
        }
    }
}

/// Inside a walk's helper, names the path the walk is about to work on, so a
/// stall there is reported under that path. Does nothing outside a walk.
pub(crate) fn walk_reached(path: &Path) {
    WALK.with(|walk| {
        if let Some(walk) = walk.borrow().as_ref() {
            walk.reached(path);
        }
    });
}

/// Runs a whole directory walk on a helper thread.
///
/// The walk is bounded at its iterator: `work` must call
/// `WalkControl::advance` for each entry and stop when it returns true. Its
/// deadline is `deadline`, moved earlier only to leave reply time inside the
/// request budget, never cut to the single-call limit. The request thread
/// waits until the walk returns, until the request budget runs out, or until
/// the walk makes no progress for the stall window. A walk that is slow but
/// still moving is never reported as blocked.
///
/// `Err` means the walk did not run (no helper slot or the deadline already
/// passed); that failure is already recorded on the request.
pub(crate) fn run_walk<T: Send + 'static>(
    path: &Path,
    deadline: Instant,
    work: impl FnOnce(PathBuf, &WalkControl) -> T + Send + 'static,
) -> io::Result<WalkEnd<T>> {
    run_walk_with_limiter(path, deadline, Arc::clone(&HELPERS), work)
}

/// The walk's own deadline and the time the request thread stops waiting.
fn walk_deadlines(requested: Instant) -> (Instant, Instant) {
    let now = Instant::now();
    let window = single_access_budget();
    let request = CURRENT.with(|budget| budget.borrow().as_ref().map(|b| b.deadline));
    let walk = match request {
        Some(request) => {
            let margin = WALK_REPLY_MARGIN.min(request.saturating_duration_since(now) / 2);
            requested.min(request.checked_sub(margin).unwrap_or(request))
        }
        None => requested,
    };
    let wait = request.map_or(walk + window, |request| request.min(walk + window));
    (walk, wait)
}

fn run_walk_with_limiter<T: Send + 'static>(
    path: &Path,
    deadline: Instant,
    limiter: Arc<Limiter>,
    work: impl FnOnce(PathBuf, &WalkControl) -> T + Send + 'static,
) -> io::Result<WalkEnd<T>> {
    if let Some(error) = prior_failure() {
        return Err(error);
    }
    // A walk started from inside another walk shares the outer walk's bounds.
    if let Some(outer) = WALK.with(|walk| walk.borrow().clone()) {
        return Ok(WalkEnd::Finished(work(path.to_path_buf(), &outer)));
    }
    let (walk_deadline, wait_deadline) = walk_deadlines(deadline);
    if Instant::now() >= walk_deadline {
        return Err(failure(
            path,
            "read_deadline_exceeded",
            "filesystem deadline elapsed before this call started",
        ));
    }
    let Some(permit) = limiter.acquire(walk_deadline) else {
        return Err(failure(path, "read_io_queue_timeout", "no filesystem helper slot became available before the deadline; this call did not start"));
    };
    let control = Arc::new(WalkControl::new(walk_deadline, path));
    let owned = path.to_path_buf();
    let budget = current();
    #[cfg(test)]
    if let Some(counts) = budget.as_ref().map(|b| Arc::clone(&b.measurements)) {
        counts.round_trips.fetch_add(1, Ordering::Relaxed);
        counts.spawns.fetch_add(1, Ordering::Relaxed);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let running = Arc::new(AtomicBool::new(false));
    let helper_running = Arc::clone(&running);
    let helper_control = Arc::clone(&control);
    std::thread::Builder::new()
        .name("aft-filesystem-walk".into())
        .spawn(move || {
            let _permit = permit;
            #[cfg(test)]
            regression_tests::before_helper(&owned);
            // Never start a walk whose deadline passed while it was scheduled.
            if helper_control.expired() {
                return;
            }
            helper_running.store(true, Ordering::Release);
            helper_control.step();
            WALK.with(|walk| *walk.borrow_mut() = Some(Arc::clone(&helper_control)));
            let result = with_budget(budget, || work(owned, &helper_control));
            WALK.with(|walk| *walk.borrow_mut() = None);
            let _ = tx.send(result);
        })?;
    let window = single_access_budget();
    loop {
        let wake = wait_deadline.min(control.last_progress() + window);
        match rx.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(result) => return Ok(WalkEnd::Finished(result)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                control.stop.store(true, Ordering::Relaxed);
                if !running.load(Ordering::Acquire) {
                    return Err(failure(
                        path,
                        "read_io_queue_timeout",
                        "filesystem helper did not start before the deadline; this call did not start",
                    ));
                }
                return Err(io::Error::other("filesystem helper stopped"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if !running.load(Ordering::Acquire) {
                    // Not yet scheduled: a scheduling delay is not a stall.
                    if now >= wait_deadline {
                        control.stop.store(true, Ordering::Relaxed);
                        return Err(failure(
                            path,
                            "read_io_queue_timeout",
                            "filesystem helper did not start before the deadline; this call did not start",
                        ));
                    }
                    continue;
                }
                let last = control.last_progress();
                let waited = now.saturating_duration_since(last);
                // At the request's end, a call that began before the walk's
                // own deadline and is still running has outlived the walk.
                let stalled = waited >= window || (now >= wait_deadline && last < walk_deadline);
                if stalled || now >= wait_deadline {
                    control.stop.store(true, Ordering::Relaxed);
                    let stall = stalled.then(|| WalkStall {
                        path: control
                            .current
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone(),
                        waited,
                    });
                    return Ok(WalkEnd::Stopped {
                        stall,
                        entries_visited: control.entries(),
                    });
                }
            }
        }
    }
}
fn directory(path: &Path, at: Option<Instant>) -> io::Result<Access> {
    if let Some(error) = prior_failure() {
        return Err(error);
    }
    let id = CURRENT.with(|current| current.borrow().as_ref().map(|b| b.id));
    if let Some(access) = DIRECTORIES.with(|dirs| {
        let dirs = dirs.borrow();
        (Some(dirs.0) == id)
            .then(|| dirs.1.get(path).copied())
            .flatten()
    }) {
        return Ok(access);
    }
    let budget = current().unwrap_or_else(|| Budget::after(IO_BUDGET));
    let (probe, owner) = {
        let mut dirs = budget.directories.lock().unwrap_or_else(|e| e.into_inner());
        match dirs.entry(path.to_path_buf()) {
            std::collections::hash_map::Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let probe = Arc::new(Probe {
                    result: Mutex::new(None),
                    ready: Condvar::new(),
                });
                entry.insert(Arc::clone(&probe));
                (probe, true)
            }
        }
    };
    let access = if owner {
        let result = run(path, at, |path| {
            #[cfg(test)]
            regression_tests::before_directory_probe(&path)?;
            #[cfg(all(test, unix))]
            tests::before_directory_probe(&path)?;
            // Opening and advancing ReadDir probes consent, not just existence.
            let mut entries = std::fs::read_dir(&path)?;
            let _ = entries.next().transpose()?;
            let access = Access {
                direct: local_filesystem(&path)?,
            };
            #[cfg(all(test, unix))]
            tests::after_directory_probe(&path);
            Ok(access)
        });
        let macos = cfg!(target_os = "macos");
        #[cfg(all(test, unix))]
        let macos = macos || tests::macos_directory(path);
        observe_permission(path, &result, macos);
        let result = result.map_err(|error| ProbeError::from(&error));
        *probe.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result.clone());
        probe.ready.notify_all();
        result.map_err(|error| error.error())?
    } else {
        let at = deadline(at);
        let mut result = probe.result.lock().unwrap_or_else(|e| e.into_inner());
        while result.is_none() {
            let remaining = at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(failure(
                    path,
                    "read_io_queue_timeout",
                    "deadline elapsed waiting for this directory's already-running probe",
                ));
            }
            result = probe
                .ready
                .wait_timeout(result, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        result
            .as_ref()
            .unwrap()
            .clone()
            .map_err(|error| error.error())?
    };
    DIRECTORIES.with(|dirs| {
        let mut dirs = dirs.borrow_mut();
        if dirs.0 != budget.id {
            dirs.0 = budget.id;
            dirs.1.clear();
        }
        dirs.1.insert(path.to_path_buf(), access);
    });
    Ok(access)
}
pub(crate) fn probe_directory(path: &Path, at: Option<Instant>) -> io::Result<()> {
    directory(path, at).map(|_| ())
}
fn parent(path: &Path, at: Option<Instant>) -> io::Result<Access> {
    directory(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
        at,
    )
}
fn cached_directory(path: &Path) -> Option<Access> {
    let id = CURRENT.with(|budget| budget.borrow().as_ref().map(|b| b.id))?;
    DIRECTORIES.with(|dirs| {
        let dirs = dirs.borrow();
        (dirs.0 == id).then(|| dirs.1.get(path).copied()).flatten()
    })
}
fn traversal_parent(path: &Path) -> Option<&Path> {
    path.parent().map(|parent| {
        if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        }
    })
}
// A successful stat on a local filesystem permits sibling stats, not directory
// enumeration or file reads. Keep this weaker evidence out of the listing cache.
fn cached_traversal(path: &Path) -> bool {
    cached_directory(path).is_some_and(|access| access.direct)
        || CURRENT.with(|current| {
            current.borrow().as_ref().is_some_and(|budget| {
                budget
                    .traversable
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(path)
            })
        })
}
fn run_traversal<T: Send + 'static>(
    path: &Path,
    work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let (result, traversable) = run(path, None, move |path| {
        let parent = traversal_parent(&path).map(Path::to_path_buf);
        let result = work(path)?;
        // Filesystem identification can itself block, so keep it in the helper.
        let traversable = parent.filter(|parent| local_filesystem(parent).unwrap_or(false));
        Ok((result, traversable))
    })?;
    // Publish only after the caller receives success, never from a late helper.
    if let (Some(parent), Some(budget)) = (traversable, current()) {
        budget
            .traversable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(parent);
    }
    Ok(result)
}
fn probe_target(path: &Path) -> io::Result<()> {
    if cached_directory(path).is_some() {
        return Ok(());
    }
    let metadata = if path
        .parent()
        .and_then(cached_directory)
        .is_some_and(|access| access.direct)
    {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            run(path, None, stat)?
        } else {
            metadata
        }
    } else {
        match run(path, None, stat) {
            Ok(metadata) => metadata,
            Err(error) => {
                if error.raw_os_error() == Some(1) {
                    let folder = permission_folder(path);
                    observe_permission(
                        &folder,
                        &Err(io::Error::from_raw_os_error(1)),
                        cfg!(target_os = "macos"),
                    );
                }
                return Err(error);
            }
        }
    };
    if metadata.is_dir() {
        directory(path, None)?;
    } else {
        parent(path, None)?;
    }
    Ok(())
}
// Only positively identified local filesystems are eligible for direct reads.
// Network/FUSE/unknown mounts stay bounded per file even after a successful
// directory probe; a mount that stops responding mid-request cannot park a root.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn local_filesystem(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let resolved = std::fs::canonicalize(path)?;
        let text = resolved.to_string_lossy();
        // Local APFS directory access does not guarantee a cloud placeholder's
        // contents are resident. Hydration may still block on first file read.
        if text.contains("/Library/Mobile Documents/") || text.contains("/Library/CloudStorage/") {
            return Ok(false);
        }
    }
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    #[cfg(target_os = "macos")]
    {
        let name = unsafe { std::ffi::CStr::from_ptr(stat.f_mntonname.as_ptr()) };
        Ok(
            stat.f_flags & libc::MNT_LOCAL as u32 != 0
                && !name.to_bytes().starts_with(b"/Volumes/"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        Ok(matches!(
            stat.f_type as u64,
            0xEF53 | 0x58465342 | 0x9123683E | 0x01021994 | 0x794c7630 | 0x2FC12FC1
        ))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn local_filesystem(_: &Path) -> io::Result<bool> {
    Ok(false)
}

/// One `stat` that follows symlinks, as a single bounded access.
fn stat(path: PathBuf) -> io::Result<std::fs::Metadata> {
    #[cfg(test)]
    regression_tests::before_stat(&path);
    std::fs::metadata(path)
}
pub(crate) fn read(path: &Path, at: Option<Instant>) -> io::Result<Vec<u8>> {
    if Instant::now() >= deadline(at) {
        return Err(failure(
            path,
            "read_deadline_exceeded",
            "scan deadline elapsed before file access",
        ));
    }
    let access = parent(path, at)?;
    if !access.direct {
        return run(path, at, |path| regular_read(&path, false));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return run(path, at, |path| regular_read(&path, false));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    match regular_read(path, true) {
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            run(path, at, |path| regular_read(&path, false))
        }
        result => result,
    }
}
fn regular_read(path: &Path, _no_follow: bool) -> io::Result<Vec<u8>> {
    if !_no_follow && !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(all(test, unix))]
    tests::before_regular_open(path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A file can become a FIFO between admission and open. Never block on that
    // replacement, and check the opened descriptor before reading any bytes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | if _no_follow { libc::O_NOFOLLOW } else { 0 });
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve(usize::try_from(metadata.len()).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
pub(crate) fn read_to_string(path: &Path) -> io::Result<String> {
    String::from_utf8(read(path, None)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
pub(crate) fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    if let Some(error) = prior_failure() {
        return Err(error);
    }
    if cached_directory(path).is_some_and(|access| access.direct) {
        return std::fs::canonicalize(path);
    }
    // Resolving a path needs traversal, not permission to enumerate its parent.
    // Reuse a known-safe parent, otherwise isolate only canonicalize itself.
    if traversal_parent(path).is_some_and(cached_traversal)
        && std::fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink())
    {
        std::fs::canonicalize(path)
    } else {
        run_traversal(path, std::fs::canonicalize)
    }
}
pub(crate) fn metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    if let Some(error) = prior_failure() {
        return Err(error);
    }
    // A stat must not turn into an unrelated ancestor's directory listing.
    let direct = cached_directory(path).map_or_else(
        || cached_traversal(path) || traversal_parent(path).is_some_and(cached_traversal),
        |access| access.direct,
    );
    if direct {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_symlink() {
            return Ok(metadata);
        }
    }
    run_traversal(path, stat)
}

#[cfg(test)]
#[path = "bounded_io_tests.rs"]
mod regression_tests;
#[cfg(all(test, unix))]
pub(crate) use regression_tests::stall_helper;
#[cfg(test)]
pub(crate) use regression_tests::{
    block_stat, slow_walk, stall_walk_at, with_single_access_budget,
};
#[cfg(all(test, unix))]
pub(crate) fn stall_helper_if_requested(path: &Path) {
    regression_tests::before_helper(path);
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;
    static MACOS_DIRECTORIES: LazyLock<Mutex<HashMap<PathBuf, bool>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    pub(crate) struct MacosProbe(PathBuf);
    impl MacosProbe {
        pub(crate) fn allow(&self) {
            MACOS_DIRECTORIES
                .lock()
                .unwrap()
                .insert(self.0.clone(), true);
        }
    }
    impl Drop for MacosProbe {
        fn drop(&mut self) {
            MACOS_DIRECTORIES.lock().unwrap().remove(&self.0);
        }
    }
    pub(crate) fn deny_macos_directory(path: &Path) -> MacosProbe {
        let path = std::fs::canonicalize(path).unwrap();
        MACOS_DIRECTORIES
            .lock()
            .unwrap()
            .insert(path.clone(), false);
        MacosProbe(path)
    }
    pub(super) fn macos_directory(path: &Path) -> bool {
        MACOS_DIRECTORIES.lock().unwrap().contains_key(path)
    }
    static BLOCKERS: LazyLock<Mutex<HashMap<PathBuf, (PathBuf, Arc<AtomicUsize>)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static PROBE_FINISHERS: LazyLock<Mutex<HashMap<PathBuf, std::sync::mpsc::Sender<()>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static FIFO_REPLACEMENTS: LazyLock<
        Mutex<HashMap<PathBuf, (Arc<AtomicUsize>, std::sync::mpsc::Sender<()>)>>,
    > = LazyLock::new(|| Mutex::new(HashMap::new()));
    pub(crate) struct BlockProbe {
        keys: Vec<PathBuf>,
        pub(crate) fired: Arc<AtomicUsize>,
        pub(crate) finished: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for BlockProbe {
        fn drop(&mut self) {
            let mut blockers = BLOCKERS.lock().unwrap();
            for key in &self.keys {
                blockers.remove(key);
                PROBE_FINISHERS.lock().unwrap().remove(key);
            }
        }
    }
    pub(crate) fn block_directory_probe(directory: &Path, fifo: &Path) -> BlockProbe {
        let mut keys = vec![
            directory.to_path_buf(),
            std::fs::canonicalize(directory).unwrap(),
        ];
        keys.dedup();
        let fired = Arc::new(AtomicUsize::new(0));
        let (sender, finished) = std::sync::mpsc::channel();
        let mut blockers = BLOCKERS.lock().unwrap();
        for key in &keys {
            blockers.insert(key.clone(), (fifo.to_path_buf(), Arc::clone(&fired)));
            PROBE_FINISHERS
                .lock()
                .unwrap()
                .insert(key.clone(), sender.clone());
        }
        BlockProbe {
            keys,
            fired,
            finished,
        }
    }
    pub(super) fn after_directory_probe(path: &Path) {
        if let Some(sender) = PROBE_FINISHERS.lock().unwrap().remove(path) {
            let _ = sender.send(());
        }
    }
    pub(super) fn before_directory_probe(directory: &Path) -> io::Result<()> {
        if MACOS_DIRECTORIES.lock().unwrap().get(directory) == Some(&false) {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let blocker = BLOCKERS.lock().unwrap().get(directory).cloned();
        if let Some((path, fired)) = blocker {
            fired.fetch_add(1, Ordering::Relaxed);
            let _file = std::fs::File::open(path)?;
        }
        Ok(())
    }
    pub(super) fn before_regular_open(path: &Path) -> io::Result<()> {
        let fired = FIFO_REPLACEMENTS.lock().unwrap().remove(path);
        if let Some((fired, reached)) = fired {
            std::fs::remove_file(path)?;
            fifo(path);
            fired.fetch_add(1, Ordering::Relaxed);
            let _ = reached.send(());
        }
        Ok(())
    }
    pub(crate) fn fifo(path: &Path) {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    pub(crate) fn release_fifo(path: &Path) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .unwrap();
        let _ = file.write_all(b"released\n");
    }
    #[test]
    fn blocked_open_returns_before_fifo_writer_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked");
        fifo(&path);
        let (tx, rx) = std::sync::mpsc::channel();
        let input = path.clone();
        let thread = std::thread::spawn(move || {
            tx.send(run(
                &input,
                Some(Instant::now() + Duration::from_millis(80)),
                std::fs::read,
            ))
            .unwrap()
        });
        let before_writer = rx.recv_timeout(Duration::from_millis(800));
        release_fifo(&path);
        thread.join().unwrap();
        let error = before_writer
            .expect("read waited for a FIFO writer instead of its deadline")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("read_blocked"));
    }
    #[test]
    fn saturation_waits_for_a_healthy_helper_instead_of_claiming_blocked_access() {
        let limiter = Arc::new(Limiter::new(1));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_limiter = Arc::clone(&limiter);
        let first = std::thread::spawn(move || {
            run_with_limiter(Path::new("first"), None, first_limiter, move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(1)
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            tx.send(run_with_limiter(Path::new("second"), None, limiter, |_| {
                Ok(2)
            }))
            .unwrap()
        });
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), 1);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), 2);
        second.join().unwrap();
    }
    #[test]
    fn saturation_deadline_is_queue_timeout_not_read_blocked() {
        let limiter = Arc::new(Limiter::new(1));
        let permit = limiter
            .acquire(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let work_fired = Arc::clone(&fired);
        let budget = Budget::after(Duration::from_millis(80));
        let error = with_budget(Some(budget.clone()), || {
            run_with_limiter(Path::new("queued"), None, limiter, move |_| {
                work_fired.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        })
        .unwrap_err();
        drop(permit);
        assert!(error.to_string().contains("read_io_queue_timeout"));
        assert!(!error.to_string().contains("read_blocked"));
        assert_eq!(fired.load(Ordering::Relaxed), 0);
        assert_eq!(
            budget.failure.lock().unwrap().as_ref().unwrap().code,
            "read_io_queue_timeout"
        );
    }
    #[test]
    fn accessible_directory_never_opens_a_fifo_as_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        fifo(&path);
        let error = with_budget(Some(Budget::after(IO_BUDGET)), || read(&path, None)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
    #[test]
    fn fifo_replacement_after_admission_cannot_block_direct_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regular.txt");
        std::fs::write(&path, "regular\n").unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        FIFO_REPLACEMENTS
            .lock()
            .unwrap()
            .insert(path.clone(), (Arc::clone(&fired), reached_tx));
        let input = path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || tx.send(read(&input, None)).unwrap());
        let reached = reached_rx.recv_timeout(IO_BUDGET + Duration::from_secs(1));
        let before_writer = rx.recv_timeout(Duration::from_millis(800));
        if fired.load(Ordering::Relaxed) > 0 {
            release_fifo(&path);
        }
        thread.join().unwrap();
        reached.expect("the replacement must happen after file admission");
        let error = before_writer
            .expect("a FIFO replacement blocked the direct open")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fired.load(Ordering::Relaxed), 1);
    }
}

#[cfg(all(test, unix))]
mod modes_macos_tests {
    use super::*;

    #[test]
    fn macos_permission_observation_preserves_eperm_and_named_folder() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget::after(IO_BUDGET);
        let _scope = enter(Some(budget.clone()));
        let error = ProbeError::from(&io::Error::from_raw_os_error(libc::EPERM));
        assert_eq!(error.error().raw_os_error(), Some(libc::EPERM));
        observe_permission(dir.path(), &Err(error.error()), true);
        let response = finish_response(Response::success("probe", serde_json::json!({})));
        assert_eq!(response.data["code"], "macos_permission_denied");
        assert_eq!(response.data["folder"], serde_json::json!(dir.path()));
        assert_eq!(response.data["program"], "ck-aft");
        assert_eq!(budget.permissions().len(), 1);
        let non_macos = Budget::after(IO_BUDGET);
        let _scope = enter(Some(non_macos.clone()));
        observe_permission(dir.path(), &Err(error.error()), false);
        assert!(non_macos.permissions().is_empty());
    }

    #[test]
    fn macos_first_touch_timeout_never_mutates_after_probe_release() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("protected");
        std::fs::create_dir(&root).unwrap();
        let target = root.join("file.txt");
        std::fs::write(&target, "before").unwrap();
        let fifo = dir.path().join("answer");
        tests::fifo(&fifo);
        let blocker = tests::block_directory_probe(&root, &fifo);
        let mut config = crate::config::Config::default();
        config.project_root = Some(root);
        config.storage_dir = Some(dir.path().join("store"));
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let request: RawRequest = serde_json::from_value(
            serde_json::json!({"id":"write","command":"write","file":target,"content":"after"}),
        )
        .unwrap();
        let started = Instant::now();
        let budget = Budget::after(Duration::from_millis(100));
        let response = with_budget(Some(budget.clone()), || {
            crate::commands::write::handle_write(&request, &ctx)
        });
        assert_eq!(response.data["code"], "read_blocked", "{response:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "remaining reply budget must cap the five-second I/O helper"
        );
        assert_eq!(blocker.fired.load(Ordering::Relaxed), 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        tests::release_fifo(&fifo);
        blocker
            .finished
            .recv_timeout(Duration::from_secs(2))
            .expect("the released helper finishes its read-only probe");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        assert!(!response.success);
    }
}

#[cfg(test)]
mod probe_target_tests {
    use super::*;

    #[test]
    fn windows_invalid_glob_name_probes_ancestors_but_access_denial_does_not() {
        let invalid_name = io::Error::from_raw_os_error(123);
        let glob = Path::new(r"C:\project\*.txt");
        assert!(missing_probe_target(&invalid_name, glob, true));
        assert!(!missing_probe_target(&invalid_name, glob, false));
        assert!(!missing_probe_target(
            &invalid_name,
            Path::new(r"C:\project\file.txt"),
            true
        ));
        assert!(!missing_probe_target(
            &io::Error::from_raw_os_error(5),
            glob,
            true
        ));
        assert!(missing_probe_target(
            &io::Error::new(io::ErrorKind::NotFound, "missing"),
            glob,
            true
        ));
    }
}
