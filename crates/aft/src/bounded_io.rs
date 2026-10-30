//! Bound first access to a protection/filesystem boundary, not every file read.
//!
//! A request probes each directory once. Ordinary files in accessible local
//! directories stay on the caller's hot path. Network/removable/unknown volumes
//! and symlinks retain per-file isolation. Helpers own no actor or epoch guard.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::LazyLock;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub(crate) const IO_BUDGET: Duration = Duration::from_secs(5);
pub(crate) const WALK_PARTIAL_GAP: &str =
    "Directory walk reached its time budget; only examined entries are reported.";
/// Request budget for a mutation's path checks when no caller budget exists.
/// It matches the budget agent tool calls already run under, so a native
/// request and a tool call wait equally long for a helper to be scheduled.
pub(crate) const MUTATION_BUDGET: Duration = Duration::from_secs(25);
const MAX_IO_THREADS: usize = 64;
const IO_POOL_SIZE: usize = 8;
static NEXT_BUDGET: AtomicU64 = AtomicU64::new(1);
static HELPERS: OnceLock<Arc<Limiter>> = OnceLock::new();
#[cfg(test)]
static THREAD_SPAWNS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
#[derive(Default)]
struct Measurements {
    dispatches: AtomicUsize,
    round_trips: AtomicUsize,
    directory_probes: AtomicUsize,
    traversal_probes: AtomicUsize,
}

#[derive(Clone, Debug)]
struct Failure {
    code: &'static str,
    message: String,
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
    state: Mutex<ProbeState>,
    ready: Condvar,
    activity: Mutex<ProbeActivity>,
}
#[derive(Default)]
struct ProbeActivity {
    path: Option<PathBuf>,
    progress: Option<Instant>,
    walking: bool,
    cancelled: Option<Arc<AtomicBool>>,
}
struct ProbeState {
    result: Option<Result<Access, ProbeError>>,
    deadline: Instant,
    started: Option<Instant>,
}
impl Probe {
    fn queued(deadline: Instant) -> Self {
        Self {
            state: Mutex::new(ProbeState {
                result: None,
                deadline,
                started: None,
            }),
            ready: Condvar::new(),
            activity: Mutex::new(ProbeActivity::default()),
        }
    }
    fn set_deadline(&self, deadline: Instant, started: Option<Instant>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.deadline = deadline;
        state.started = started;
        self.ready.notify_all();
    }
}
#[derive(Clone)]
pub(crate) struct Budget {
    id: u64,
    pub(crate) deadline: Instant,
    failure: Arc<Mutex<Option<Failure>>>,
    failed: Arc<AtomicBool>,
    walk_partial: Arc<AtomicBool>,
    /// A read stall alone does not mean the walk reached its own deadline.
    walk_time_limit_reached: Arc<AtomicBool>,
    walk_gap: Arc<Mutex<Option<String>>>,
    directories: Arc<Mutex<HashMap<PathBuf, Arc<Probe>>>>,
    traversable: Arc<Mutex<HashSet<PathBuf>>>,
    permissions: Arc<Mutex<Vec<PermissionObservation>>>,
    /// Set once the end of the budget has been kept back for the reply.
    reply_margin_kept: bool,
    #[cfg(test)]
    measurements: Arc<Measurements>,
    #[cfg(test)]
    access_budget: Duration,
    #[cfg(test)]
    access_budget_by_path: Arc<HashMap<PathBuf, Duration>>,
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
            walk_partial: Arc::new(AtomicBool::new(false)),
            walk_time_limit_reached: Arc::new(AtomicBool::new(false)),
            walk_gap: Arc::new(Mutex::new(None)),
            directories: Arc::new(Mutex::new(HashMap::new())),
            traversable: Arc::new(Mutex::new(HashSet::new())),
            permissions: Arc::new(Mutex::new(Vec::new())),
            reply_margin_kept: false,
            #[cfg(test)]
            measurements: Arc::new(Measurements::default()),
            #[cfg(test)]
            access_budget: IO_BUDGET,
            #[cfg(test)]
            access_budget_by_path: Arc::new(HashMap::new()),
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
            self.measurements.dispatches.load(Ordering::Relaxed),
            self.measurements.round_trips.load(Ordering::Relaxed),
        )
    }
    #[cfg(test)]
    pub(crate) fn with_access_budget(mut self, duration: Duration) -> Self {
        self.access_budget = duration;
        self
    }
    #[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
    pub(crate) fn with_path_access_budget(mut self, path: &Path, duration: Duration) -> Self {
        Arc::make_mut(&mut self.access_budget_by_path).insert(path.to_path_buf(), duration);
        self
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

pub(crate) fn finish_response(mut response: Response) -> Response {
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
    if response.success && current().is_some_and(|b| b.walk_partial.load(Ordering::Acquire)) {
        let named_gap = error.map(|error| error.message).or_else(|| {
            current().and_then(|budget| {
                budget
                    .walk_gap
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
        });
        let mut gaps = Vec::new();
        if current().is_some_and(|b| b.walk_time_limit_reached.load(Ordering::Acquire)) {
            gaps.push(WALK_PARTIAL_GAP.to_string());
        }
        if let Some(gap) = named_gap {
            if gap != WALK_PARTIAL_GAP {
                gaps.push(gap);
            }
        }
        let gap = gaps.join("\n");
        response.data["complete"] = serde_json::json!(false);
        if let Some(existing) = response.data["gaps"].as_array_mut() {
            existing.extend(gaps.into_iter().map(serde_json::Value::String));
        } else {
            response.data["gaps"] = serde_json::json!(gaps);
        }
        let text = response.data["text"].as_str().unwrap_or_default();
        // Coverage details precede the shared trailer so the last line remains
        // the list's machine-readable count and narrowing advice.
        let text = match text.rsplit_once("\n\n") {
            Some((body, trailer)) if trailer.starts_with("shown ") => {
                format!("{body}\n\n{gap}\n\n{trailer}")
            }
            _ => format!("{text}\n\n{gap}"),
        };
        response.data["text"] = serde_json::json!(text);
        return response;
    }
    match error {
        Some(error) => Response::error(&response.id, error.code, error.message),
        None => response,
    }
}
thread_local! {
    static CURRENT: RefCell<Option<Budget>> = const { RefCell::new(None) };
    static ACTIVE_PROBE: RefCell<Option<Arc<Probe>>> = const { RefCell::new(None) };
    // Rayon shares the request's single-flight probes, but does not contend on
    // that map per file. Its workers remember successful lookups locally.
    static DIRECTORIES: RefCell<(u64, HashMap<PathBuf, Access>)> = RefCell::new((0, HashMap::new()));
    static HELPER_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static MUTATION_ADMISSION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
/// Scope for the path checks of a mutation the user asked for (delete, edit,
/// write, ...). A helper that has not started yet has touched nothing, and a
/// mutation has no partial answer to return, so a slow-to-schedule helper (a
/// loaded machine, or a Windows process whose thread start waits on a
/// long-running DLL load) may wait for admission until the request deadline
/// instead of the per-access cap. Once the helper starts, the usual access
/// cap applies unchanged, so a blocked filesystem still fails as
/// `read_blocked` within that cap.
pub(crate) struct MutationScope {
    previous: bool,
    _scope: Scope,
}
impl Drop for MutationScope {
    fn drop(&mut self) {
        MUTATION_ADMISSION.with(|flag| flag.set(self.previous));
    }
}
pub(crate) fn enter_mutation() -> MutationScope {
    let scope = enter_if_absent(MUTATION_BUDGET);
    MutationScope {
        previous: MUTATION_ADMISSION.with(|flag| flag.replace(true)),
        _scope: scope,
    }
}
pub(crate) fn with_budget<T>(budget: Option<Budget>, work: impl FnOnce() -> T) -> T {
    let _scope = enter(budget);
    work()
}
pub(crate) fn enter(budget: Option<Budget>) -> Scope {
    Scope(CURRENT.with(|current| current.replace(budget)))
}
/// The deadline bounded calls see while [`excluding_wait`] runs, far enough
/// out that only their per-access cap limits them, so a stalled filesystem
/// still fails within that cap. Afterwards the request's own deadline moves
/// later by the time the wait actually took, not by this amount.
const WAIT_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
/// Runs `wait`, work that waits for or brings up shared state the request then
/// reads (binding the checkout's lexical view, for example), without spending
/// the request's filesystem deadline on it.
///
/// A request's deadline starts when the request arrives. A request that first
/// waits behind such work on a loaded machine could otherwise find its
/// deadline already spent when its first read starts, and fail with
/// `read_deadline_exceeded` before reading anything. Inside `wait`, bounded
/// calls keep the request's failure, permission and probe records but are
/// limited only by their per-access cap; afterwards the request's deadline
/// moves later by however long `wait` took, so its reads get the time the
/// request had left when the wait began.
pub(crate) fn excluding_wait<T>(wait: impl FnOnce() -> T) -> T {
    let Some(budget) = current() else {
        return wait();
    };
    let started = Instant::now();
    let result = {
        let mut held = budget;
        held.deadline = started + WAIT_DEADLINE;
        let _scope = enter(Some(held));
        wait()
    };
    let waited = started.elapsed();
    CURRENT.with(|current| {
        if let Some(budget) = current.borrow_mut().as_mut() {
            budget.deadline += waited;
        }
    });
    result
}
/// Keep the last `margin` of the current request's budget for its reply:
/// move the filesystem deadline `margin` earlier for the rest of the
/// command. A request calls this before its last filesystem work, so a
/// stalled read fails while the reply can still reach the caller in time.
/// Only the first call moves the deadline, so a command that reads several
/// documents keeps one margin, not one per document.
pub(crate) fn keep_reply_margin(margin: Duration) {
    CURRENT.with(|current| {
        if let Some(budget) = current.borrow_mut().as_mut() {
            if !budget.reply_margin_kept {
                budget.reply_margin_kept = true;
                budget.deadline = budget
                    .deadline
                    .checked_sub(margin)
                    .unwrap_or(budget.deadline);
            }
        }
    });
}
fn deadline(at: Option<Instant>) -> Instant {
    request_deadline(at)
        .into_iter()
        .fold(Instant::now() + IO_BUDGET, Instant::min)
}
fn request_deadline(at: Option<Instant>) -> Option<Instant> {
    at.into_iter()
        .chain(CURRENT.with(|budget| budget.borrow().as_ref().map(|b| b.deadline)))
        .min()
}
fn failure(path: &Path, code: &'static str, reason: &str) -> io::Error {
    let message = format!("{code}: {}: {reason}", path.display());
    if let Some(budget) = current() {
        budget
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(|| Failure {
                code,
                message: message.clone(),
            });
        budget.failed.store(true, Ordering::Release);
    }
    io::Error::new(io::ErrorKind::TimedOut, Failure { code, message })
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
#[cfg(test)]
thread_local! {
    /// Inputs [`command`] probed before running its handler, so tests can
    /// check that a long `files` list is not probed past what is examined.
    pub(crate) static COMMAND_INPUT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
            // An outline reads at most the first entries of its `files` list,
            // and none of them for a directory, files-mode or revision
            // outline; probing the rest would cost one access check each
            // for entries the handler never opens.
            Some(serde_json::Value::Array(_)) if key == "files" && req.command == "outline" => {
                inputs.extend(crate::commands::outline::outline_examined_files(req));
            }
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
        #[cfg(test)]
        COMMAND_INPUT_PROBES.with(|probes| probes.set(probes.get() + 1));
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

type IoJob = Box<dyn FnOnce() + Send + 'static>;

/// A published walk snapshot never holds its lock while accessing the filesystem.
/// The caller can take the last examined prefix even if the helper is stuck in `next`.
pub(crate) struct WalkProgress<T> {
    snapshot: Arc<Mutex<T>>,
    stopped: Arc<AtomicBool>,
    deadline: Instant,
    published: Arc<AtomicBool>,
}
pub(crate) struct WalkCompletion {
    pub(crate) incomplete: bool,
    pub(crate) time_limit: bool,
}
impl<T> WalkProgress<T> {
    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
            || Instant::now() >= self.deadline
            || prior_failure().is_some()
    }
    pub(crate) fn update(&self, update: impl FnOnce(&mut T)) {
        let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        update(&mut snapshot);
        drop(snapshot);
        self.published.store(true, Ordering::Release);
        ACTIVE_PROBE.with(|probe| {
            if let Some(probe) = probe.borrow().as_ref() {
                probe
                    .activity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .progress = Some(Instant::now());
            }
        });
    }
}

pub(crate) fn run_walk<T: Clone + Send + 'static>(
    path: &Path,
    at: Option<Instant>,
    initial: T,
    work: impl FnOnce(PathBuf, WalkProgress<T>) -> io::Result<T> + Send + 'static,
) -> (T, WalkCompletion) {
    if prior_failure().is_some() {
        return (
            initial,
            WalkCompletion {
                incomplete: true,
                time_limit: false,
            },
        );
    }
    let deadline = request_deadline(at).unwrap_or_else(|| Instant::now() + IO_BUDGET);
    let snapshot = Arc::new(Mutex::new(initial));
    let stopped = Arc::new(AtomicBool::new(false));
    let published = Arc::new(AtomicBool::new(false));
    let progress = WalkProgress {
        snapshot: snapshot.clone(),
        stopped: stopped.clone(),
        deadline,
        published: published.clone(),
    };
    let probe = Arc::new(Probe::queued(deadline));
    {
        let mut activity = probe.activity.lock().unwrap_or_else(|e| e.into_inner());
        activity.walking = true;
        activity.cancelled = Some(stopped.clone());
    }
    // A stalled descendant is a gap in this walk, not a reason to discard
    // already-discovered paths when the caller reads or admits them afterward.
    let parent = current();
    let local = parent.clone().map(|mut budget| {
        budget.failure = Arc::new(Mutex::new(None));
        budget.failed = Arc::new(AtomicBool::new(false));
        budget
    });
    let (result, failure) = with_budget(local, || {
        let result = run_with_timeout(
            path,
            Some(deadline),
            Arc::clone(HELPERS.get_or_init(|| Limiter::new(IO_POOL_SIZE))),
            access_budget(path),
            Some(probe),
            move |path| work(path, progress),
        );
        let failure = current()
            .and_then(|budget| {
                budget
                    .failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .or_else(|| {
                result
                    .as_ref()
                    .err()
                    .and_then(|error| error.get_ref()?.downcast_ref::<Failure>().cloned())
            });
        (result, failure)
    });
    stopped.store(true, Ordering::Release);
    let failed = failure.is_some();
    let time_limit = Instant::now() >= deadline;
    let partial = result.is_err() || failed || time_limit;
    let named_gap = failure
        .as_ref()
        .map(|failure| failure.message.clone())
        .or_else(|| {
            result
                .as_ref()
                .err()
                .filter(|_| !time_limit)
                .map(ToString::to_string)
        });
    if partial && (published.load(Ordering::Acquire) || !failed) {
        if let Some(budget) = &parent {
            budget.walk_partial.store(true, Ordering::Release);
            if time_limit {
                budget
                    .walk_time_limit_reached
                    .store(true, Ordering::Release);
            }
            if let Some(gap) = named_gap {
                budget
                    .walk_gap
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(gap);
            }
        }
    } else if let (Some(budget), Some(failure)) = (parent, failure) {
        budget
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(failure);
        budget.failed.store(true, Ordering::Release);
    }
    (
        result.unwrap_or_else(|_| snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()),
        WalkCompletion {
            incomplete: partial,
            time_limit,
        },
    )
}

/// Attribute nested access to the entry being checked, not the walk's root.
pub(crate) fn reading_path(path: &Path) -> impl Drop {
    struct ReadingPath(Option<(Arc<Probe>, Option<PathBuf>)>);
    impl Drop for ReadingPath {
        fn drop(&mut self) {
            if let Some((probe, previous)) = self.0.take() {
                probe
                    .activity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .path = previous;
            }
        }
    }
    ReadingPath(ACTIVE_PROBE.with(|probe| {
        probe.borrow().as_ref().map(|probe| {
            let previous = probe
                .activity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .path
                .replace(path.to_path_buf());
            (probe.clone(), previous)
        })
    }))
}

pub(crate) fn walk_path(path: &Path) -> bool {
    let keep_going = ACTIVE_PROBE.with(|probe| {
        if let Some(probe) = probe.borrow().as_ref() {
            let mut activity = probe.activity.lock().unwrap_or_else(|e| e.into_inner());
            if activity
                .cancelled
                .as_ref()
                .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
            {
                return false;
            }
            activity.path = Some(path.to_path_buf());
            activity.progress = Some(Instant::now());
        }
        true
    });
    #[cfg(all(test, unix))]
    stall_running_helper_if_requested(path);
    keep_going
}

struct Worker {
    sender: std::sync::mpsc::Sender<IoJob>,
    retired: Arc<AtomicBool>,
}
#[derive(Default)]
struct PoolState {
    idle: Vec<Worker>,
    usable: usize,
    live: usize,
    abandoned: usize,
    spawned: u64,
    retired_total: u64,
    replacements: u64,
    queue_timeouts: u64,
    cap_hits: u64,
}
struct Limiter {
    state: Mutex<PoolState>,
    available: Condvar,
    max: usize,
    max_live: usize,
    create: std::sync::mpsc::SyncSender<()>,
}
impl Limiter {
    fn new(max: usize) -> Arc<Self> {
        Self::with_limits(max, MAX_IO_THREADS)
    }
    fn with_limits(max: usize, max_live: usize) -> Arc<Self> {
        assert!(max > 0 && max <= max_live);
        let (create, requests) = std::sync::mpsc::sync_channel(1);
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let pool = weak.clone();
            // Creating a worker can stall, for example on Windows DLL
            // initialization. After this one-time factory startup, admission
            // callers only enqueue creation requests and wait to their deadline;
            // worker creation never holds the pool state lock.
            let _ = std::thread::Builder::new()
                .name("aft-filesystem-probe-factory".into())
                .spawn(move || {
                    while requests.recv().is_ok() {
                        let Some(pool) = pool.upgrade() else { break };
                        pool.create_worker();
                    }
                });
            Self {
                state: Mutex::new(PoolState::default()),
                available: Condvar::new(),
                max,
                max_live,
                create,
            }
        })
    }
    fn create_worker(self: &Arc<Self>) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.idle.is_empty() || state.usable >= self.max || state.live >= self.max_live {
                return;
            }
            state.live += 1;
            state.usable += 1;
        }
        let (sender, receiver) = std::sync::mpsc::channel::<IoJob>();
        let retired = Arc::new(AtomicBool::new(false));
        let worker_retired = Arc::clone(&retired);
        let pool = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("aft-filesystem-probe".into())
            .spawn(move || {
                let _exit = WorkerExit {
                    pool,
                    retired: worker_retired,
                };
                while let Ok(job) = receiver.recv() {
                    // A panicking job disconnects its result channel. Keep this
                    // worker alive until its caller retires it and closes sender.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _state = HelperJobScope::enter();
                        job();
                    }));
                }
            });
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if spawned.is_ok() {
            #[cfg(test)]
            THREAD_SPAWNS.fetch_add(1, Ordering::Relaxed);
            state.replacements += u64::from(state.retired_total > state.replacements);
            state.spawned += 1;
            state.idle.push(Worker { sender, retired });
        } else {
            state.live -= 1;
            state.usable -= 1;
        }
        self.available.notify_all();
    }
    fn acquire(self: &Arc<Self>, at: Instant) -> Option<Permit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut counted_cap = false;
        loop {
            let remaining = at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.queue_timeouts += 1;
                return None;
            }
            if let Some(worker) = state.idle.pop() {
                return Some(Permit {
                    pool: Arc::clone(self),
                    worker: Some(worker),
                    reusable: true,
                });
            }
            if state.usable < self.max && state.live < self.max_live {
                if matches!(
                    self.create.try_send(()),
                    Err(std::sync::mpsc::TrySendError::Disconnected(_))
                ) {
                    return None;
                }
            }
            if state.live >= self.max_live && !counted_cap {
                state.cap_hits += 1;
                counted_cap = true;
            }
            state = self
                .available
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
    fn health(&self) -> serde_json::Value {
        match self.state.try_lock() {
            Ok(state) => pool_health_snapshot(&state, self.max, self.max_live, "ready"),
            Err(error) => serde_json::json!({
                "pool_size": self.max,
                "thread_cap": self.max_live,
                "status": "unavailable",
                "reason": match error {
                    std::sync::TryLockError::WouldBlock => "state_busy",
                    std::sync::TryLockError::Poisoned(_) => "state_poisoned",
                },
            }),
        }
    }
}
struct HelperJobScope {
    directories: (u64, HashMap<PathBuf, Access>),
    mutation_admission: bool,
    #[cfg(test)]
    canonicalize_calls: Option<Vec<PathBuf>>,
    #[cfg(test)]
    _read: crate::commands::read::HelperStateScope,
    #[cfg(test)]
    _snapshot: crate::hashline::snapshot::HelperStateScope,
    #[cfg(test)]
    _search: crate::search_index::HelperStateScope,
}
impl HelperJobScope {
    fn enter() -> Self {
        // Reset only at the dispatched job boundary. Nested probes run inline
        // and must keep the enclosing job's cache, counters and test hooks.
        Self {
            directories: DIRECTORIES.with(|cache| cache.replace(Default::default())),
            mutation_admission: MUTATION_ADMISSION.with(|flag| flag.replace(false)),
            #[cfg(test)]
            canonicalize_calls: crate::inspect::job::CANONICALIZE_CALLS
                .with(|calls| calls.replace(None)),
            #[cfg(test)]
            _read: crate::commands::read::helper_state_scope(),
            #[cfg(test)]
            _snapshot: crate::hashline::snapshot::helper_state_scope(),
            #[cfg(test)]
            _search: crate::search_index::helper_state_scope(),
        }
    }
}
impl Drop for HelperJobScope {
    fn drop(&mut self) {
        DIRECTORIES.with(|cache| cache.replace(std::mem::take(&mut self.directories)));
        MUTATION_ADMISSION.with(|flag| flag.set(self.mutation_admission));
        #[cfg(test)]
        crate::inspect::job::CANONICALIZE_CALLS
            .with(|calls| *calls.borrow_mut() = self.canonicalize_calls.take());
    }
}
fn pool_health_snapshot(
    state: &PoolState,
    max: usize,
    max_live: usize,
    status: &str,
) -> serde_json::Value {
    serde_json::json!({
        "pool_size": max,
        "thread_cap": max_live,
        "status": status,
        "live": state.live,
        "idle": state.idle.len(),
        "usable": state.usable,
        "abandoned": state.abandoned,
        "spawned_total": state.spawned,
        "abandoned_total": state.retired_total,
        "replacements_total": state.replacements,
        "cap_hits_total": state.cap_hits,
        "queue_timeouts_total": state.queue_timeouts,
    })
}
pub(crate) fn helper_health() -> serde_json::Value {
    helper_health_from(&HELPERS)
}

/// Start every worker the shared filesystem helper pool keeps. The pool
/// starts workers on demand, up to its configured size, and keeps reusable
/// workers idle for later calls (only a worker abandoned after a timeout
/// exits). A test that compares this process's threads before and after some
/// work fills it first; otherwise the pool growing to meet that work reads as
/// threads the work kept.
#[cfg(test)]
pub(crate) fn fill_helper_pool_for_test() {
    let pool = Arc::clone(HELPERS.get_or_init(|| Limiter::new(IO_POOL_SIZE)));
    let deadline = Instant::now() + Duration::from_secs(30);
    // Holding every permit at once leaves no idle worker to reuse, so each
    // acquire starts a new one until the pool is at its size.
    let permits: Vec<Permit> = (0..pool.max)
        .map(|_| {
            pool.acquire(deadline)
                .expect("the filesystem helper pool did not fill")
        })
        .collect();
    drop(permits);
}
fn helper_health_from(helpers: &OnceLock<Arc<Limiter>>) -> serde_json::Value {
    // Health runs on the frame loop: observing an unused pool must not start
    // its factory, and observing a busy pool must not wait for its state lock.
    match helpers.get() {
        Some(pool) => pool.health(),
        None => pool_health_snapshot(
            &PoolState::default(),
            IO_POOL_SIZE,
            MAX_IO_THREADS,
            "not_started",
        ),
    }
}
struct WorkerExit {
    pool: std::sync::Weak<Limiter>,
    retired: Arc<AtomicBool>,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            state.live -= 1;
            if self.retired.load(Ordering::Relaxed) {
                state.abandoned -= 1;
            }
            pool.available.notify_all();
        }
    }
}
struct Permit {
    pool: Arc<Limiter>,
    worker: Option<Worker>,
    reusable: bool,
}
impl Permit {
    fn send(&mut self, job: IoJob) -> io::Result<()> {
        self.reusable = false;
        self.worker
            .as_ref()
            .unwrap()
            .sender
            .send(job)
            .map_err(|_| io::Error::other("filesystem helper stopped"))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let worker = self.worker.take().unwrap();
        let mut state = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.reusable {
            state.idle.push(worker);
        } else {
            // A timed-out job cannot be cancelled inside the OS. Remove its
            // healthy slot, but count its thread against the hard cap until it
            // actually exits. Closing the sender makes a late return exit.
            worker.retired.store(true, Ordering::Relaxed);
            state.usable -= 1;
            state.abandoned += 1;
            state.retired_total += 1;
        }
        self.pool.available.notify_all();
    }
}
/// Isolate boundary probes, whole walks, and reads on risky filesystems.
/// Stuck helpers retain the submitted job closure and its owned captures.
pub(crate) fn run<T: Send + 'static>(
    path: &Path,
    at: Option<Instant>,
    work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    run_with_limiter(
        path,
        at,
        Arc::clone(HELPERS.get_or_init(|| Limiter::new(IO_POOL_SIZE))),
        work,
    )
}

/// A separate helper pool whose stalled jobs cannot consume file-read workers.
#[derive(Clone)]
pub(crate) struct IsolatedPool(Arc<Limiter>);

impl IsolatedPool {
    pub(crate) fn new(thread_cap: usize) -> Self {
        Self(Limiter::with_limits(thread_cap, thread_cap))
    }

    pub(crate) fn run<T: Send + 'static>(
        &self,
        path: &Path,
        at: Option<Instant>,
        work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        run_with_limiter(path, at, Arc::clone(&self.0), work)
    }
}

/// Reserves time for a bounded walk to return partial results before the bounded I/O helper's
/// deadline; a helper still stuck in one OS call remains a `read_blocked`.
pub(crate) fn deadline_before_io_timeout(
    path: &Path,
    budget: Duration,
    return_margin: Duration,
) -> Instant {
    let now = Instant::now();
    let access_deadline = now + access_budget(path);
    let hard_deadline =
        request_deadline(None).map_or(access_deadline, |request| request.min(access_deadline));
    let walk_deadline = now + budget;
    let return_deadline = hard_deadline.checked_sub(return_margin).unwrap_or(now);
    walk_deadline.min(return_deadline)
}
fn run_with_limiter<T: Send + 'static>(
    path: &Path,
    at: Option<Instant>,
    limiter: Arc<Limiter>,
    work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    run_with_timeout(path, at, limiter, access_budget(path), None, work)
}
fn access_budget(path: &Path) -> Duration {
    #[cfg(test)]
    return current().map_or(IO_BUDGET, |budget| {
        budget
            .access_budget_by_path
            .get(path)
            .copied()
            .unwrap_or(budget.access_budget)
    });
    #[cfg(not(test))]
    {
        let _ = path;
        IO_BUDGET
    }
}
fn run_with_timeout<T: Send + 'static>(
    path: &Path,
    at: Option<Instant>,
    limiter: Arc<Limiter>,
    access_budget: Duration,
    probe: Option<Arc<Probe>>,
    work: impl FnOnce(PathBuf) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let _reading = reading_path(path);
    if let Some(error) = prior_failure() {
        return Err(error);
    }
    let queued = Instant::now();
    let request_deadline = request_deadline(at);
    // Without a request deadline, queueing and access each get their own cap.
    // Inside a mutation, a helper may wait to be admitted and scheduled until
    // the request deadline, because nothing has run yet; see `MutationScope`.
    let queue_deadline = match request_deadline {
        Some(at) if MUTATION_ADMISSION.with(std::cell::Cell::get) => at,
        Some(at) => at.min(queued + access_budget),
        None => queued + access_budget,
    };
    if let Some(probe) = &probe {
        probe.set_deadline(queue_deadline, None);
    }
    if queued >= queue_deadline {
        return Err(failure(
            path,
            "read_deadline_exceeded",
            "filesystem deadline elapsed before this call started",
        ));
    }
    // A bounded walk already owns an isolated helper. Do not consume another
    // slot per nested directory probe (or let healthy walks deadlock each
    // other on admission). Its caller's deadline must be no later than ours.
    let walking = ACTIVE_PROBE.with(|probe| {
        probe.borrow().as_ref().is_some_and(|probe| {
            probe
                .activity
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .walking
        })
    });
    if let Some(outer) = HELPER_DEADLINE.with(|outer| {
        outer.get().filter(|outer| {
            *outer <= queue_deadline || (walking && request_deadline.is_some_and(|at| at >= *outer))
        })
    }) {
        if let Some(probe) = &probe {
            probe.set_deadline(outer, Some(Instant::now()));
        }
        return work(path.to_path_buf());
    }
    let Some(mut permit) = limiter.acquire(queue_deadline) else {
        return Err(failure(path, "read_io_queue_timeout", "no filesystem helper slot became available before the deadline; this call did not start"));
    };
    let owned = path.to_path_buf();
    let budget = current();
    let confinement = crate::context::confinement::capture();
    #[cfg(test)]
    let measurements = budget.as_ref().map(|b| Arc::clone(&b.measurements));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let probe = probe.unwrap_or_else(|| Arc::new(Probe::queued(queue_deadline)));
    let helper_probe = Arc::clone(&probe);
    #[cfg(test)]
    if let Some(counts) = &measurements {
        counts.round_trips.fetch_add(1, Ordering::Relaxed);
    }
    permit.send(Box::new(move || {
            #[cfg(test)]
            regression_tests::before_helper(&owned);
            let result = crate::context::confinement::inherit(confinement, || with_budget(budget, || {
                // Owning a slot does not mean the OS has scheduled this helper.
                // Never start expired work or blame its scheduling delay on I/O.
                // Publish the phase under the same lock used by the owner and
                // shared waiters, so neither can mistake a started access for
                // an expired queue while the helper is being scheduled.
                let mut state = helper_probe.state.lock().unwrap_or_else(|e| e.into_inner());
                let started = Instant::now();
                if started >= queue_deadline {
                    return Err(failure(&owned, "read_io_queue_timeout", "filesystem helper did not start before the deadline; this call did not start"));
                }
                // Admission and scheduling are not OS access. Start the access
                // cap here, while retaining the caller's hard request deadline.
                let at = request_deadline
                    .map_or(started + access_budget, |at| at.min(started + access_budget));
                state.started = Some(started);
                state.deadline = at;
                helper_probe.ready.notify_all();
                drop(state);
                struct DeadlineScope(Option<Instant>);
                impl Drop for DeadlineScope {
                    fn drop(&mut self) {
                        HELPER_DEADLINE.with(|deadline| deadline.set(self.0));
                    }
                }
                let helper_at = if helper_probe.activity.lock().unwrap_or_else(|e| e.into_inner()).walking {
                    request_deadline.unwrap_or(at)
                } else { at };
                let _deadline = DeadlineScope(HELPER_DEADLINE.with(|deadline| deadline.replace(Some(helper_at))));
                struct ActiveProbeScope(Option<Arc<Probe>>);
                impl Drop for ActiveProbeScope {
                    fn drop(&mut self) {
                        ACTIVE_PROBE.with(|probe| *probe.borrow_mut() = self.0.take());
                    }
                }
                let _active = ActiveProbeScope(ACTIVE_PROBE.with(|probe| probe.replace(Some(helper_probe.clone()))));
                helper_probe.activity.lock().unwrap_or_else(|e| e.into_inner()).progress = Some(started);
                #[cfg(test)]
                regression_tests::after_helper_start(&owned);
                work(owned)
            }));
            let _ = tx.send(result);
        }))?;
    #[cfg(test)]
    if let Some(counts) = &measurements {
        counts.dispatches.fetch_add(1, Ordering::Relaxed);
    }
    let mut wait_until = queue_deadline.min(queued + access_budget);
    loop {
        match rx.recv_timeout(wait_until.saturating_duration_since(Instant::now())) {
            Ok(result) => {
                permit.reusable = true;
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let state = probe.state.lock().unwrap_or_else(|e| e.into_inner());
                let Some(started) = state.started else {
                    // A mutation may wait for admission past one access cap.
                    // Look again at least once per cap, so a helper that
                    // starts late and then hangs still fails at its own
                    // access deadline instead of at the request deadline.
                    let now = Instant::now();
                    if now < queue_deadline {
                        wait_until = queue_deadline.min(now + access_budget);
                        continue;
                    }
                    return Err(failure(
                    path,
                    "read_io_queue_timeout",
                    "filesystem helper did not start before the deadline; this call did not start",
                ));
                };
                let mut at = state.deadline;
                drop(state);
                let activity = probe.activity.lock().unwrap_or_else(|e| e.into_inner());
                let access_started = if activity.walking {
                    activity.progress.unwrap_or(started)
                } else {
                    started
                };
                let stalled_at = access_started + access_budget;
                if activity.walking {
                    at = request_deadline.map_or(stalled_at, |deadline| deadline.min(stalled_at));
                }
                let blocked_path = activity.path.clone().unwrap_or_else(|| path.to_path_buf());
                let walk_deadline = activity.walking
                    && request_deadline.is_some_and(|deadline| deadline <= stalled_at);
                drop(activity);
                if Instant::now() < at {
                    wait_until = at;
                    continue;
                }
                if walk_deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "walk deadline elapsed",
                    ));
                }
                let hint = if cfg!(target_os = "macos") {
                    "; on macOS, check ck-aft access in Privacy & Security, then retry"
                } else {
                    ""
                };
                return Err(failure(
                    &blocked_path,
                    "read_blocked",
                    &format!(
                        "OS access did not finish within {:.3} s{hint}",
                        at.saturating_duration_since(access_started).as_secs_f64()
                    ),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("filesystem helper stopped"));
            }
        }
    }
}
fn directory(path: &Path, at: Option<Instant>) -> io::Result<Access> {
    let _reading = reading_path(path);
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
                let probe = Arc::new(Probe::queued(
                    at.map_or(budget.deadline, |at| at.min(budget.deadline)),
                ));
                entry.insert(Arc::clone(&probe));
                (probe, true)
            }
        }
    };
    let access = if owner {
        #[cfg(test)]
        budget
            .measurements
            .directory_probes
            .fetch_add(1, Ordering::Relaxed);
        let result = run_with_timeout(
            path,
            at,
            Arc::clone(HELPERS.get_or_init(|| Limiter::new(IO_POOL_SIZE))),
            access_budget(path),
            Some(Arc::clone(&probe)),
            |path| {
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
            },
        );
        let macos = cfg!(target_os = "macos");
        #[cfg(all(test, unix))]
        let macos = macos || tests::macos_directory(path);
        observe_permission(path, &result, macos);
        let result = result.map_err(|error| ProbeError::from(&error));
        probe.state.lock().unwrap_or_else(|e| e.into_inner()).result = Some(result.clone());
        probe.ready.notify_all();
        result.map_err(|error| error.error())?
    } else {
        let request_deadline = request_deadline(at);
        let mut state = probe.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.result.is_none() {
            // The owner may still be queued. Once it starts, all waiters follow
            // its new access deadline, without extending their own request.
            let at = request_deadline.map_or(state.deadline, |at| at.min(state.deadline));
            let remaining = at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let blocked = state.started.is_some() && at == state.deadline;
                return Err(failure(
                    path,
                    if blocked {
                        "read_blocked"
                    } else {
                        "read_io_queue_timeout"
                    },
                    if blocked {
                        "OS access deadline elapsed waiting for this directory's running probe"
                    } else {
                        "deadline elapsed waiting for this directory's shared probe"
                    },
                ));
            }
            state = probe
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state
            .result
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
    let local = DIRECTORIES.with(|dirs| {
        let dirs = dirs.borrow();
        (dirs.0 == id).then(|| dirs.1.get(path).copied()).flatten()
    });
    if local.is_some() {
        return local;
    }
    // Another thread of this request may already have probed the directory:
    // grep reads files on rayon workers, then stats every match again on the
    // thread that sorts the results. Reuse that finished probe, as
    // `directory` would, instead of spawning a helper per directory this
    // thread did not read itself. A probe still running is not waited for
    // here; callers then take their normal bounded path.
    let budget = current()?;
    let probe = budget
        .directories
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned()?;
    let access = match probe.state.lock().unwrap_or_else(|e| e.into_inner()).result {
        Some(Ok(access)) => access,
        _ => return None,
    };
    // Remember it on this thread too, so later files under the same
    // directory do not take the request-wide lock again.
    DIRECTORIES.with(|dirs| {
        let mut dirs = dirs.borrow_mut();
        if dirs.0 != budget.id {
            dirs.0 = budget.id;
            dirs.1.clear();
        }
        dirs.1.insert(path.to_path_buf(), access);
    });
    Some(access)
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
    #[cfg(test)]
    if let Some(budget) = current() {
        budget
            .measurements
            .traversal_probes
            .fetch_add(1, Ordering::Relaxed);
    }
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
            run(path, None, std::fs::metadata)?
        } else {
            metadata
        }
    } else {
        match run(path, None, std::fs::metadata) {
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
    let scratch_check = crate::task_temp_reads::check_path(path)?;
    crate::views::io::WORK
        .content_reads
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut file = options.open(path)?;
    crate::task_temp_reads::check_opened(path, scratch_check.as_ref(), &file)?;
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
    crate::views::io::WORK
        .content_read_bytes
        .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
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
    run_traversal(path, std::fs::metadata)
}

#[cfg(test)]
#[path = "bounded_io_tests.rs"]
mod regression_tests;
#[cfg(all(test, unix))]
pub(crate) use regression_tests::{stall_helper, stall_running_helper};
#[cfg(all(test, unix))]
pub(crate) fn stall_helper_if_requested(path: &Path) {
    regression_tests::before_helper(path);
}
#[cfg(all(test, unix))]
pub(crate) fn stall_running_helper_if_requested(path: &Path) {
    regression_tests::after_helper_start(path);
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
        note_open(path);
        let fired = FIFO_REPLACEMENTS.lock().unwrap().remove(path);
        if let Some((fired, reached)) = fired {
            std::fs::remove_file(path)?;
            fifo(path);
            fired.fetch_add(1, Ordering::Relaxed);
            let _ = reached.send(());
        }
        Ok(())
    }
    static OPEN_WATCHES: LazyLock<Mutex<HashMap<PathBuf, Arc<AtomicUsize>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    /// Counts the opens of one file's contents while it is alive, so a test
    /// can prove a file was never read.
    pub(crate) struct OpenWatch {
        path: PathBuf,
        pub(crate) opens: Arc<AtomicUsize>,
    }
    impl Drop for OpenWatch {
        fn drop(&mut self) {
            OPEN_WATCHES.lock().unwrap().remove(&self.path);
        }
    }
    pub(crate) fn watch_opens(path: &Path) -> OpenWatch {
        let opens = Arc::new(AtomicUsize::new(0));
        OPEN_WATCHES
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), Arc::clone(&opens));
        OpenWatch {
            path: path.to_path_buf(),
            opens,
        }
    }
    /// Record an open of `path`'s contents for an [`OpenWatch`] on it.
    pub(crate) fn note_open(path: &Path) {
        if let Some(opens) = OPEN_WATCHES.lock().unwrap().get(path) {
            opens.fetch_add(1, Ordering::Relaxed);
        }
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
    fn sequential_probes_reuse_helper_threads() {
        let limiter = Limiter::new(4);
        let budget = Budget::after(Duration::from_secs(5));
        let mut threads = HashSet::new();
        with_budget(Some(budget.clone()), || {
            for _ in 0..40 {
                threads.insert(
                    run_with_limiter(Path::new("healthy"), None, limiter.clone(), |_| {
                        Ok(std::thread::current().id())
                    })
                    .unwrap(),
                );
            }
        });
        assert!(
            threads.len() <= 4,
            "spawned {} helpers for 40 sequential probes",
            threads.len()
        );
    }

    #[test]
    fn timed_out_helper_is_replaced_without_delaying_later_probes() {
        let limiter = Limiter::new(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let first_pool = limiter.clone();
        let first = std::thread::spawn(move || {
            run_with_limiter(
                Path::new("blocked"),
                Some(Instant::now() + Duration::from_millis(150)),
                first_pool,
                move |_| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let blocked = first.join().unwrap().unwrap_err();
        let before = Instant::now();
        let later = run_with_limiter(
            Path::new("healthy"),
            Some(before + Duration::from_millis(150)),
            limiter.clone(),
            |_| Ok(42),
        );
        let health = limiter.health();
        release_tx.send(()).unwrap();
        assert_eq!(timeout_code(&blocked), "read_blocked");
        assert_eq!(
            later.unwrap(),
            42,
            "a timed-out helper must not retain the healthy slot"
        );
        assert!(before.elapsed() < Duration::from_millis(300));
        assert_eq!(health["abandoned"], 1);
        assert_eq!(health["replacements_total"], 1);
        assert_eq!(health["spawned_total"], 2);
    }

    #[test]
    fn abandoned_helper_cap_bounds_threads_and_recovers_after_release() {
        let pool = Limiter::with_limits(1, 2);
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (release, blocked) = std::sync::mpsc::channel();
            releases.push(release);
            let error = run_with_limiter(
                Path::new("blocked"),
                Some(Instant::now() + Duration::from_millis(200)),
                pool.clone(),
                move |_| {
                    blocked.recv().unwrap();
                    Ok(())
                },
            )
            .unwrap_err();
            assert_eq!(timeout_code(&error), "read_blocked");
        }
        let fired = Arc::new(AtomicBool::new(false));
        let work_fired = fired.clone();
        let before = Instant::now();
        let error = run_with_limiter(
            Path::new("queued"),
            Some(before + Duration::from_millis(80)),
            pool.clone(),
            move |_| {
                work_fired.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .unwrap_err();
        let health = pool.health();
        for release in releases {
            release.send(()).unwrap();
        }
        assert_eq!(timeout_code(&error), "read_io_queue_timeout");
        assert!(!fired.load(Ordering::Relaxed));
        assert!(before.elapsed() < Duration::from_millis(300));
        assert_eq!(health["live"], 2);
        assert_eq!(health["abandoned"], 2);
        assert_eq!(health["spawned_total"], 2);
        assert_eq!(health["replacements_total"], 1);
        assert_eq!(health["cap_hits_total"], 1);
        assert_eq!(health["queue_timeouts_total"], 1);
        run_with_limiter(
            Path::new("recovered"),
            Some(Instant::now() + Duration::from_secs(2)),
            pool,
            |_| Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn reused_helper_resets_deadline_and_request_context() {
        let pool = Limiter::new(1);
        let first = Budget::after(IO_BUDGET);
        let first_id = first.id;
        with_budget(Some(first), || {
            run_with_limiter(Path::new("first"), None, pool.clone(), move |_| {
                assert_eq!(current().unwrap().id, first_id);
                assert!(HELPER_DEADLINE.with(|at| at.get()).is_some());
                Ok(())
            })
        })
        .unwrap();
        let mut permit = pool.acquire(Instant::now() + IO_BUDGET).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        permit
            .send(Box::new(move || {
                tx.send((
                    current().is_none(),
                    HELPER_DEADLINE.with(|at| at.get()).is_none(),
                ))
                .unwrap();
            }))
            .unwrap();
        let (request_cleared, deadline_cleared) = rx.recv_timeout(IO_BUDGET).unwrap();
        permit.reusable = true;
        assert!(
            request_cleared,
            "a reused helper retained the previous request"
        );
        assert!(
            deadline_cleared,
            "a reused helper retained the previous deadline"
        );
    }

    #[test]
    fn saturation_waits_for_a_healthy_helper_instead_of_claiming_blocked_access() {
        let limiter = Limiter::new(1);
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
        let limiter = Limiter::new(1);
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
        let protected = dir.path().join("protected");
        std::fs::create_dir(&protected).unwrap();
        // Exercise aliased roots on every Unix host, not only macOS's /var.
        let root = dir.path().join("alias");
        std::os::unix::fs::symlink(&protected, &root).unwrap();
        let target = root.join("file.txt");
        std::fs::write(&target, "before").unwrap();
        let fifo = dir.path().join("answer");
        tests::fifo(&fifo);
        let blocker = tests::block_directory_probe(&root, &fifo);
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(dir.path().join("store"));
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let request: RawRequest = serde_json::from_value(
            serde_json::json!({"id":"write","command":"write","file":target,"content":"after"}),
        )
        .unwrap();
        let started = Instant::now();
        // The access cap starts only after the helper is scheduled. A short
        // whole-request budget can expire during unrelated preflight I/O and
        // never reach the blocked directory, especially on a loaded test host.
        // Preflight probes the canonical directory, not the request's alias.
        let budget = Budget::after(Duration::from_secs(30))
            .with_path_access_budget(&root.canonicalize().unwrap(), Duration::from_millis(100));
        let response = with_budget(Some(budget.clone()), || {
            crate::commands::write::handle_write(&request, &ctx)
        });
        assert_eq!(response.data["code"], "read_blocked", "{response:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the blocked probe must hit its per-access cap, not the five-second default"
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
