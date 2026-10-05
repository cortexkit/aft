use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::writer::LspWriter;
use crate::lsp::child_registry::LspChildRegistry;
use crate::lsp::jsonrpc::{
    Notification, Request, RequestId, Response as JsonRpcResponse, ServerMessage,
};
use crate::lsp::position::path_to_uri;
use crate::lsp::registry::ServerKind;
use crate::lsp::{transport, LspError};

/// Default timeout for interactive LSP requests (hover, goto-def, references, rename).
pub(crate) const INTERACTIVE_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Longer budget for one-shot handshake requests (initialize, shutdown).
pub(crate) const HANDSHAKE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STDERR_TAIL_LINES: usize = 64;
const STDERR_LINE_BYTES: usize = 4 * 1024;
/// How long an exit report waits for the stderr reader to hit end-of-file,
/// so the last lines a dying server printed are in the report.
const STDERR_EOF_WAIT: Duration = Duration::from_millis(250);
/// Longest stderr excerpt, in bytes, carried by a single server-exit log line.
const EXIT_LOG_STDERR_BYTES: usize = 2 * 1024;
/// Longest first-stderr-line excerpt quoted in a short failure cause.
const CAUSE_STDERR_LINE_BYTES: usize = 200;
/// Longest command line quoted in exit reports.
const COMMAND_DISPLAY_BYTES: usize = 512;

/// rust-analyzer starts a `cargo check` of the whole workspace each time it
/// becomes quiescent after loading the workspace; 1.98 announces it about
/// 200 ms later, but a server short of CPU has taken over a second. Until it
/// begins, the published diagnostics lack every compiler error, so callers
/// that need them wait for it. When none has begun this long after
/// quiescence, the results stay unknown (never clean) until a check runs.
const WORKSPACE_CHECK_START_DEADLINE: Duration = Duration::from_secs(10);

/// rust-analyzer can report the end of a check run just before it publishes
/// the run's last diagnostics; callers keep reading events this long after
/// the end.
pub(crate) const FLYCHECK_PUBLISH_SETTLE: Duration = Duration::from_millis(300);

/// How long after a `textDocument/didSave` a rust-analyzer check run is
/// expected to begin. rust-analyzer 1.98 announces the run about 150 ms after
/// the save; the margin covers a busy server. When no run has begun this long
/// after the last of [`MAX_SAVE_SENDS`] sends, callers stop waiting and report
/// the compiler results as unknown: they still describe the files before the
/// save.
const SAVE_CHECK_START_GRACE: Duration = Duration::from_secs(3);

/// A save that started no check run within this long is sent again: a
/// watched-file change reaching rust-analyzer just before or after the save
/// makes it drop the check.
const SAVE_RESEND_AFTER: Duration = Duration::from_millis(1500);

/// How many times one save is sent before AFT stops waiting for its check.
const MAX_SAVE_SENDS: u8 = 3;

/// How long after the file watcher reports a Rust source change the save
/// for it is sent (see [`LspClient::owe_rust_save`]).
const EXTERNAL_SAVE_DELAY: Duration = Duration::from_secs(1);

/// A save AFT asked rust-analyzer to check.
#[derive(Debug, Clone)]
struct RustSaveRequest {
    /// The saved document.
    uri: lsp_types::Uri,
    /// Set while the save has not been sent: send it once this passes.
    due_at: Option<Instant>,
    /// When the save was last sent.
    last_sent_at: Option<Instant>,
    /// How many times it has been sent.
    sends: u8,
    /// The reader's check-begin count when the save was first sent (see
    /// `LspClient::rust_check_begins_read`); `None` until then. Only a begin
    /// read after that send is a run of the saved contents.
    begins_read_at_send: Option<u64>,
}

/// How a server asked to be told about saved documents
/// (`textDocumentSync.save` in its initialize response).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveNotification {
    /// Send `textDocument/didSave` without the document text.
    WithoutText,
    /// Send `textDocument/didSave` carrying the saved text.
    IncludeText,
}

/// Read `textDocumentSync.save` from an initialize response's capabilities.
/// A bare sync kind (a number) or a missing or `false` `save` means the
/// server does not want save notifications.
fn parse_save_notification(capabilities: &Value) -> Option<SaveNotification> {
    match capabilities.pointer("/textDocumentSync/save")? {
        Value::Bool(true) => Some(SaveNotification::WithoutText),
        Value::Object(options) => Some(
            if options.get("includeText").and_then(Value::as_bool) == Some(true) {
                SaveNotification::IncludeText
            } else {
                SaveNotification::WithoutText
            },
        ),
        _ => None,
    }
}

/// Whether a rust-analyzer `$/progress` notification is about a check run:
/// rust-analyzer names their token `rust-analyzer/flycheck/N` and titles them
/// with the check command (`cargo check`, `cargo clippy`).
fn is_rust_check_progress(token: &str, title: Option<&str>) -> bool {
    token.contains("flycheck")
        || title.is_some_and(|title| {
            title.starts_with("cargo check")
                || title.starts_with("cargo clippy")
                || title.contains("flycheck")
        })
}

/// Whether `$/progress` parameters announce the beginning of a check run.
fn is_rust_check_begin(params: Option<&Value>) -> bool {
    let Some(params) = params else {
        return false;
    };
    let token = match params.get("token") {
        Some(Value::String(token)) => token.clone(),
        Some(Value::Number(token)) => token.to_string(),
        _ => return false,
    };
    params.pointer("/value/kind").and_then(Value::as_str) == Some("begin")
        && is_rust_check_progress(
            &token,
            params.pointer("/value/title").and_then(Value::as_str),
        )
}

/// Which `cargo check` runs rust-analyzer will make, read from the
/// initialization options AFT starts it with. Returns `(on_save, on_load)`:
/// whether a save starts a check (`checkOnSave`, on unless set to `false`),
/// and whether a check of the whole workspace also runs each time the
/// workspace finishes loading (additionally needs `check.workspace`, on
/// unless `false`). Measured with rust-analyzer 1.98: with `checkOnSave`
/// off neither runs, and with `check.workspace` off only the on-save check
/// does. Both also need the server to accept save notifications.
fn rust_check_triggers(initialization_options: Option<&Value>) -> (bool, bool) {
    let disabled = |value: Option<&Value>| match value {
        Some(Value::Bool(enabled)) => !enabled,
        // Older rust-analyzer configurations spell it as an object:
        // `checkOnSave: { enable: false }`.
        Some(Value::Object(options)) => options.get("enable") == Some(&Value::Bool(false)),
        _ => false,
    };
    let on_save = !disabled(initialization_options.and_then(|options| options.get("checkOnSave")));
    let on_load = on_save
        && !disabled(
            initialization_options.and_then(|options| options.pointer("/check/workspace")),
        );
    (on_save, on_load)
}

/// Whether rust-analyzer's published diagnostics carry the compiler's
/// results for the files as they are now. See
/// [`LspClient::rust_check_state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RustCheckState {
    /// No check is running or expected: the published compiler results are
    /// current.
    Current,
    /// A check is running, has just ended, or is expected to begin soon:
    /// wait for it.
    Running,
    /// A check was expected and has not begun within its deadline. The
    /// published compiler results may describe older files, so they are
    /// unknown; waiting longer is unlikely to help.
    Unreported,
}

/// rust-analyzer's own notification (not part of the LSP specification)
/// that starts a `cargo check`; with no document it checks the workspace.
pub(crate) enum RustAnalyzerRunFlycheck {}

impl lsp_types::notification::Notification for RustAnalyzerRunFlycheck {
    type Params = Value;
    const METHOD: &'static str = "rust-analyzer/runFlycheck";
}

/// Spawn on a process-lifetime thread on Linux: PR_SET_PDEATHSIG observes the
/// creating thread's death, even when the rest of the parent process is alive.
/// Keep initialization outside this queue so slow handshakes do not serialize
/// other servers' starts. Registry locking still covers spawn and registration.
fn spawn_lsp_child(
    mut command: Command,
    registry: &LspChildRegistry,
    reclaim_root: Option<&Path>,
    root: &Path,
    kind: &ServerKind,
) -> io::Result<Child> {
    #[cfg(not(target_os = "linux"))]
    {
        registry.spawn_tracked_child(&mut command, reclaim_root, Some(root), Some(kind))
    }
    #[cfg(target_os = "linux")]
    {
        type SpawnJob = Box<dyn FnOnce() + Send>;
        static SPAWNER: std::sync::OnceLock<io::Result<std::sync::mpsc::Sender<SpawnJob>>> =
            std::sync::OnceLock::new();
        let spawner = SPAWNER
            .get_or_init(|| {
                let (tx, rx) = std::sync::mpsc::channel::<SpawnJob>();
                thread::Builder::new()
                    .name("aft-lsp-spawn".into())
                    .spawn(move || {
                        for spawn in rx {
                            spawn();
                        }
                    })?;
                // This static sender is never dropped, retaining the spawning
                // thread until process exit, independently of any LSP manager.
                Ok(tx)
            })
            .as_ref()
            .map_err(|err| io::Error::other(format!("cannot start LSP spawn thread: {err}")))?;
        let registry = registry.clone();
        let reclaim_root = reclaim_root.map(Path::to_path_buf);
        let root = root.to_path_buf();
        let kind = kind.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        spawner
            .send(Box::new(move || {
                let result = registry.spawn_tracked_child(
                    &mut command,
                    reclaim_root.as_deref(),
                    Some(&root),
                    Some(&kind),
                );
                let _ = tx.send(result);
            }))
            .map_err(|_| io::Error::other("LSP spawn thread stopped"))?;
        rx.recv()
            .map_err(|_| io::Error::other("LSP spawn thread dropped its result"))?
    }
}

type PendingMap = HashMap<RequestId, Sender<JsonRpcResponse>>;
/// Dynamic `workspace/didChangeWatchedFiles` registrations by registration
/// id. A registration whose watchers could not be parsed still counts as a
/// registration: the server asked for watched-file notifications, it just did
/// not say for which files in a form this client understands.
type WatchedFileRegistrations = Arc<Mutex<HashMap<String, Vec<RegisteredFileWatcher>>>>;

/// LSP `WatchKind` bits. A watcher that omits `kind` wants all three.
const WATCH_KIND_CREATE: u32 = 1;
const WATCH_KIND_CHANGE: u32 = 2;
const WATCH_KIND_DELETE: u32 = 4;
const WATCH_KIND_ALL: u32 = WATCH_KIND_CREATE | WATCH_KIND_CHANGE | WATCH_KIND_DELETE;

/// One file-system watcher a server registered through
/// `client/registerCapability`: the glob it wants changes for, the optional
/// base directory the glob is relative to, and which change kinds it wants.
#[derive(Debug, Clone)]
pub(crate) struct RegisteredFileWatcher {
    base: Option<PathBuf>,
    matcher: globset::GlobMatcher,
    kind: u32,
}

impl RegisteredFileWatcher {
    /// Parse one entry of `registerOptions.watchers`. Returns `None` for a
    /// shape this client does not understand, such as an invalid glob.
    fn parse(watcher: &Value) -> Option<Self> {
        let kind = watcher
            .get("kind")
            .and_then(Value::as_u64)
            .and_then(|kind| u32::try_from(kind).ok())
            .unwrap_or(WATCH_KIND_ALL);
        let glob_pattern = watcher.get("globPattern")?;
        let (base, pattern) = match glob_pattern {
            Value::String(pattern) => (None, pattern.as_str()),
            Value::Object(relative) => {
                // RelativePattern: `baseUri` is either a URI string or a
                // WorkspaceFolder object carrying one.
                let base_uri = relative.get("baseUri")?;
                let base_uri = base_uri
                    .as_str()
                    .or_else(|| base_uri.get("uri").and_then(Value::as_str))?;
                let base = url::Url::parse(base_uri).ok()?.to_file_path().ok()?;
                (Some(base), relative.get("pattern")?.as_str()?)
            }
            _ => return None,
        };
        // Globs are matched with `/` separators. Servers on Windows build
        // absolute globs from native paths, whose backslashes would otherwise
        // be read as escapes or literals that never match.
        #[cfg(windows)]
        let pattern = pattern.replace('\\', "/");
        let matcher = globset::GlobBuilder::new(pattern.as_ref())
            .literal_separator(true)
            .build()
            .ok()?
            .compile_matcher();
        Some(Self {
            base,
            matcher,
            kind,
        })
    }

    fn matches(&self, path: &Path, typ: lsp_types::FileChangeType) -> bool {
        let wanted = match typ {
            lsp_types::FileChangeType::CREATED => WATCH_KIND_CREATE,
            lsp_types::FileChangeType::DELETED => WATCH_KIND_DELETE,
            _ => WATCH_KIND_CHANGE,
        };
        if self.kind & wanted == 0 {
            return false;
        }
        match &self.base {
            Some(base) => path
                .strip_prefix(base)
                .is_ok_and(|relative| self.matcher.is_match(relative)),
            None => self.matcher.is_match(path),
        }
    }
}

/// A snapshot of every watcher a server has registered, for filtering a batch
/// of file-system events without holding the registration lock per event.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegisteredFileWatchers {
    watchers: Vec<RegisteredFileWatcher>,
}

impl RegisteredFileWatchers {
    /// Whether any registered watcher wants this change to `path`.
    pub(crate) fn matches(&self, path: &Path, typ: lsp_types::FileChangeType) -> bool {
        self.watchers
            .iter()
            .any(|watcher| watcher.matches(path, typ))
    }
}

/// Lifecycle state of a language server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Initializing,
    Ready,
    ShuttingDown,
    Exited,
}

/// Events sent from background reader threads into the main loop.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Server sent a notification (e.g. publishDiagnostics).
    Notification {
        server_kind: ServerKind,
        root: PathBuf,
        method: String,
        params: Option<Value>,
    },
    /// Server sent a request (e.g. workspace/configuration).
    ServerRequest {
        server_kind: ServerKind,
        root: PathBuf,
        id: RequestId,
        method: String,
        params: Option<Value>,
    },
    /// Server process exited or the transport stream closed.
    ServerExited {
        server_kind: ServerKind,
        root: PathBuf,
        /// PID of the process whose output stream ended. The manager uses it
        /// to tell a dead server apart from a newer one for the same root.
        pid: u32,
        reason: ServerExitReason,
    },
}

/// Why the background reader stopped.
///
/// A framing or I/O error on a still-running server used to be collapsed into
/// the same `ServerExited` event as a real EOF, so the manager dropped the
/// client without knowing whether the child was actually gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerExitReason {
    /// `read_message` returned `Ok(None)`: the stdout stream closed cleanly.
    Eof,
    /// `read_message` returned an I/O or framing error. The child may still be
    /// alive; the payload is the `Display` of that error so the next leak can
    /// be diagnosed from the log line.
    ReadError(String),
    /// The pending-response mutex was poisoned. The reader cannot continue.
    PendingLockPoisoned,
}

impl std::fmt::Display for ServerExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => write!(f, "eof"),
            Self::ReadError(err) => write!(f, "read error: {err}"),
            Self::PendingLockPoisoned => write!(f, "pending lock poisoned"),
        }
    }
}

impl ServerExitReason {
    /// Map a terminal `read_message` result onto the reason the reader stopped.
    pub(crate) fn from_read_result(
        result: io::Result<Option<crate::lsp::jsonrpc::ServerMessage>>,
    ) -> Self {
        match result {
            Ok(None) => Self::Eof,
            Err(err) => Self::ReadError(err.to_string()),
            Ok(Some(_)) => {
                debug_assert!(false, "from_read_result called on a live message");
                Self::ReadError("unexpected live message".to_string())
            }
        }
    }
}

/// Outcome of reaping a client after the reader thread stopped.
#[derive(Debug)]
pub(crate) enum ReaderExitReap {
    AlreadyExited(std::process::ExitStatus),
    KilledWhileAlive,
}

/// The part of a language server's life it was in when it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerPhase {
    /// The process never started, or stopped before `initialize` was sent.
    Spawn,
    /// The `initialize` handshake was in flight.
    Initialize,
    /// The server had completed `initialize` and was serving requests.
    Running,
}

impl std::fmt::Display for ServerPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Spawn => "spawn",
            Self::Initialize => "initialize",
            Self::Running => "running",
        })
    }
}

/// Everything AFT knows about why one language-server process stopped: which
/// process it was, how far it got, how it ended, and what it last printed.
#[derive(Debug, Clone)]
pub struct ServerExitReport {
    pub kind: ServerKind,
    pub root: PathBuf,
    pub pid: u32,
    pub phase: ServerPhase,
    /// How the process ended; `None` when that could not be observed.
    pub status: Option<std::process::ExitStatus>,
    /// True when AFT killed a server that was still running (its output
    /// closed, or its handshake failed), so `status` reflects AFT's kill, not
    /// the server's own exit.
    pub killed_by_aft: bool,
    /// Time from spawn until the exit was observed.
    pub elapsed: Duration,
    /// Resolved executable path plus arguments.
    pub command: String,
    /// Last lines the server wrote to stderr, newline-separated.
    pub stderr_tail: String,
}

impl ServerExitReport {
    /// Human-readable exit status: `exit 3`, `signal 15 (SIGTERM)`, or
    /// `status unavailable` when the process state could not be read.
    pub fn status_text(&self) -> String {
        let status = describe_exit_status(self.status);
        if self.killed_by_aft {
            format!("killed by aft ({status})")
        } else {
            status
        }
    }

    /// The first non-empty stderr line, trimmed to a short excerpt.
    pub fn first_stderr_line(&self) -> Option<String> {
        self.stderr_tail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| truncate_to_bytes(line, CAUSE_STDERR_LINE_BYTES))
    }

    /// Compact cause for error messages, e.g.
    /// `exit 101 after 0.4 s: error: could not load workspace`.
    pub fn short_cause(&self) -> String {
        let mut cause = format!(
            "{} after {:.1} s",
            self.status_text(),
            self.elapsed.as_secs_f64()
        );
        if let Some(line) = self.first_stderr_line() {
            cause.push_str(": ");
            cause.push_str(&line);
        }
        cause
    }

    /// The single log line recording this exit. `reason` says how AFT noticed
    /// (for example `eof` when the server's stdout closed).
    pub fn log_line(&self, reason: &str) -> String {
        format!(
            "exited {:?} {} ({reason}): pid={} phase={} status={} elapsed={:.1}s command={:?} stderr_tail={}",
            self.kind,
            self.root.display(),
            self.pid,
            self.phase,
            self.status_text(),
            self.elapsed.as_secs_f64(),
            self.command,
            stderr_tail_for_log(&self.stderr_tail),
        )
    }
}

/// Describe how a process ended, naming the signal when it was killed by one.
pub fn describe_exit_status(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "status unavailable".to_string();
    };
    if let Some(code) = status.code() {
        return format!("exit {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let mut text = match signal_name(signal) {
                Some(name) => format!("signal {signal} ({name})"),
                None => format!("signal {signal}"),
            };
            if status.core_dumped() {
                text.push_str(", core dumped");
            }
            return text;
        }
    }
    format!("status {status}")
}

#[cfg(unix)]
fn signal_name(signal: i32) -> Option<&'static str> {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => return None,
    };
    Some(name)
}

/// Keep at most `max_bytes` of `text` from its start, on a char boundary.
fn truncate_to_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Render a stderr tail as one quoted log field: lines joined by ` | `, and
/// only the last `EXIT_LOG_STDERR_BYTES` kept.
fn stderr_tail_for_log(stderr_tail: &str) -> String {
    let joined = stderr_tail
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    if joined.is_empty() {
        return "<empty>".to_string();
    }
    if joined.len() <= EXIT_LOG_STDERR_BYTES {
        return format!("{joined:?}");
    }
    let mut start = joined.len() - EXIT_LOG_STDERR_BYTES;
    while start < joined.len() && !joined.is_char_boundary(start) {
        start += 1;
    }
    format!("{:?}", format!("...{}", &joined[start..]))
}

/// What this server told us it can do during the LSP `initialize` handshake.
///
/// We capture this once and use it to route diagnostic requests:
/// - `pull_diagnostics` → use `textDocument/diagnostic` instead of waiting for push
/// - `workspace_diagnostics` → use `workspace/diagnostic` for directory mode
///
/// Defaults are conservative: `false` means "fall back to push semantics".
#[derive(Debug, Clone, Default)]
pub struct ServerDiagnosticCapabilities {
    /// Server supports `textDocument/diagnostic` (LSP 3.17 per-file pull).
    pub pull_diagnostics: bool,
    /// Server supports `workspace/diagnostic` (LSP 3.17 workspace-wide pull).
    pub workspace_diagnostics: bool,
    /// `identifier` field from server's diagnosticProvider, if any.
    /// Used to scope previousResultId tracking when multiple servers share a file.
    pub identifier: Option<String>,
    /// Whether the server requested workspace diagnostic refresh notifications.
    /// We declare `refreshSupport: false` in our client capabilities so this
    /// should always be false in practice — kept for completeness.
    pub refresh_support: bool,
}

/// rust-analyzer analysis state saved when AFT requests a workspace reload,
/// so it can be put back if the server rejects the request.
#[derive(Debug)]
pub(crate) struct RustWorkspaceState {
    quiescent: bool,
    workspace_check_owed_since: Option<Instant>,
    failure: Option<String>,
    warning: Option<String>,
    loaded_at: SystemTime,
    check_begins_at_load: u64,
}

/// A client connected to one language server process.
pub struct LspClient {
    pub(crate) runtime_note: Option<String>,
    kind: ServerKind,
    root: PathBuf,
    state: ServerState,
    child: Child,
    /// Child PID captured at spawn time. Used by Drop to untrack the
    /// PID from the shared registry; we capture once rather than reading
    /// `child.id()` later because Drop ordering with the Child can race.
    child_pid: u32,
    writer: LspWriter,

    /// Pending request responses, keyed by request ID.
    pending: Arc<Mutex<PendingMap>>,
    /// Next request ID counter.
    next_id: AtomicI64,
    /// Diagnostic capabilities reported by the server in its initialize response.
    /// `None` until `initialize()` succeeds; conservative defaults thereafter
    /// when the server doesn't advertise diagnosticProvider.
    diagnostic_caps: Option<ServerDiagnosticCapabilities>,
    /// Rust-analyzer's workspace analysis has reached quiescence. Other server
    /// kinds do not use the experimental server-status signal and start
    /// authoritative by default.
    rust_analyzer_quiescent: bool,
    /// Workspace-load or check failure reported by rust-analyzer itself.
    rust_analyzer_failure: Option<String>,
    /// Non-fatal analyzer health warning; published diagnostics remain usable.
    pub(crate) rust_analyzer_warning: Option<String>,
    /// Since when AFT has been expecting rust-analyzer to begin a `cargo
    /// check` of the whole workspace: the one it starts on becoming
    /// quiescent after loading the workspace, or one AFT asked for again with
    /// `rust-analyzer/runFlycheck` (see
    /// [`LspClient::rearm_unreported_rust_check`]). Cleared when any check
    /// begins. While set, the published diagnostics lack the compiler's
    /// errors.
    rust_workspace_check_owed_since: Option<Instant>,
    /// `$/progress` tokens of rust-analyzer check runs (`cargo check`,
    /// "flycheck") that have begun and not yet ended. Compiler errors reach
    /// the diagnostics store only when such a run finishes, so while one is
    /// running the published Rust diagnostics are missing those errors.
    rust_flycheck_running: HashMap<String, u64>,
    /// Begin ordinal of the latest completed check. A matching begin/end is
    /// evidence even when a clean compiler run publishes no diagnostic rows.
    rust_completed_check_begin: Option<u64>,
    /// A check begun before reloading the Cargo workspace cannot certify the
    /// reloaded manifests, even if its queued notification is processed later.
    rust_check_begins_at_load: u64,
    /// When the most recent rust-analyzer check run ended.
    rust_flycheck_finished_at: Option<Instant>,
    /// When the most recent rust-analyzer check run began.
    rust_flycheck_started_at: Option<Instant>,
    /// The latest save AFT asked rust-analyzer to check, until a check run
    /// begins for it. rust-analyzer re-runs `cargo check` only on a save, so
    /// while this is set the published compiler errors describe the files as
    /// they were before the save.
    rust_save: Option<RustSaveRequest>,
    /// How many check-run begin notifications the reader thread has read from
    /// the server so far, counted as they arrive rather than when events are
    /// drained. Events wait in the channel until a caller drains them, so a
    /// begin drained after a save was sent may have been sent by the server
    /// before the save, for a run of the files before the edit; comparing
    /// this count, taken when the save is sent, with
    /// `rust_check_begins_drained` tells the two apart.
    rust_check_begins_read: Arc<AtomicU64>,
    rust_check_begin_times: Arc<Mutex<HashMap<u64, SystemTime>>>,
    pub(crate) completed_rust_check: Option<super::completed_rust_check::CompletedRustCheck>,
    /// How many check-run begin notifications have been drained (see
    /// [`LspClient::record_rust_progress`]).
    rust_check_begins_drained: u64,
    /// How the server asked to hear about saves (`textDocumentSync.save` in
    /// its initialize response). `None` until `initialize` succeeds, and when
    /// the server did not ask.
    save_notification: Option<SaveNotification>,
    /// Whether rust-analyzer runs `cargo check` when told of a save, and
    /// whether it checks the whole workspace each time it finishes loading
    /// it (see [`rust_check_triggers`]). Both false until `initialize`
    /// succeeds, and for other servers.
    rust_checks_on_save: bool,
    rust_checks_on_load: bool,
    /// Wall-clock time at which this server last started reading the
    /// workspace's Cargo manifests: the spawn, or the latest workspace reload
    /// AFT requested. A manifest or lockfile modified after this moment is
    /// newer than what rust-analyzer loaded, so its workspace view (including
    /// a load that failed because `Cargo.lock` was stale) is out of date.
    /// Wall-clock rather than `Instant` because it is compared with file
    /// modification times.
    workspace_loaded_at: SystemTime,
    /// Whether the server advertised static `workspace.didChangeWatchedFiles`
    /// support during `initialize`. Dynamic registration is tracked separately
    /// in `watched_file_registrations`; either path permits notifications.
    /// Intentional default: `false` (conservative — requires server opt-in).
    supports_watched_files: bool,
    /// Dynamic `workspace/didChangeWatchedFiles` registrations requested by
    /// the server via `client/registerCapability`. Per LSP, the client must
    /// not send watched-file notifications merely because a server mentions
    /// dynamic registration during initialize; a real registration is required.
    watched_file_registrations: WatchedFileRegistrations,
    /// Shared registry that tracks live LSP child PIDs across the process
    /// so the signal handler can SIGKILL them on SIGTERM/SIGINT before
    /// aft exits. Cloned via `Arc` — multiple clients share the same set.
    child_registry: LspChildRegistry,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Set by the stderr reader when the pipe reaches end-of-file, meaning
    /// every line the server (and its descendants) wrote is in the tail.
    stderr_closed: Arc<AtomicBool>,
    /// When the process was spawned, for the elapsed time in exit reports.
    spawned_at: Instant,
    /// Resolved executable path and arguments, for exit reports.
    command_display: String,
    /// When true, `Drop` untracks but does not kill. Tests use this so a
    /// `ServerExited` handler's kill is the only thing that can reap the child.
    #[cfg(test)]
    suppress_kill_on_drop: bool,
}

impl LspClient {
    /// Spawn a new language server process and start the background reader thread.
    ///
    /// `child_registry` is a shared handle that records this child's PID so
    /// the signal handler can SIGKILL it on SIGTERM/SIGINT. Tests that don't
    /// care about signal cleanup can pass `LspChildRegistry::new()`.
    pub fn spawn(
        kind: ServerKind,
        root: PathBuf,
        binary: &Path,
        args: &[String],
        env: &HashMap<String, String>,
        event_tx: Sender<LspEvent>,
        child_registry: LspChildRegistry,
    ) -> io::Result<Self> {
        Self::spawn_with_reclaim_root(
            kind,
            root,
            binary,
            args,
            env,
            event_tx,
            child_registry,
            None,
        )
    }

    /// Spawn a language server and associate it with a reclaim-marker root.
    pub(crate) fn spawn_with_reclaim_root(
        kind: ServerKind,
        root: PathBuf,
        binary: &Path,
        args: &[String],
        env: &HashMap<String, String>,
        event_tx: Sender<LspEvent>,
        child_registry: LspChildRegistry,
        reclaim_root: Option<&Path>,
    ) -> io::Result<Self> {
        #[cfg(windows)]
        let is_batch_file = crate::windows_command::is_batch_file(binary);
        #[cfg(windows)]
        let mut command = if is_batch_file {
            crate::windows_command::batch_command(binary, args.iter())?
        } else {
            Command::new(binary)
        };
        #[cfg(not(windows))]
        let mut command = crate::effective_path::new_command(binary);
        #[cfg(windows)]
        if !is_batch_file {
            command.args(args);
        }
        #[cfg(not(windows))]
        command.args(args);
        command
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Drain stderr on a background thread so failed shims/crashes have
            // actionable diagnostics without risking pipe-buffer deadlock.
            .stderr(Stdio::piped())
            // Git status may run in server descendants such as build scripts.
            // Disable its optional index lock so killing a check cannot leave
            // a stale lock behind; per-server env entries below may override it.
            .env("GIT_OPTIONAL_LOCKS", "0");
        for (key, value) in env {
            #[cfg(windows)]
            if is_batch_file && crate::windows_command::is_batch_internal_env(key, args.len()) {
                crate::slog_warn!(
                    "ignoring reserved batch-shim environment variable {key} for LSP server"
                );
                continue;
            }
            command.env(key, value);
        }

        // Put each LSP child in its own process group so we can SIGKILL the
        // whole group on shutdown. Critical for npm-wrapped servers like
        // biome (`node biome lsp-proxy` spawns `cli-darwin-arm64 biome
        // lsp-proxy` as a child); killing just the wrapper PID leaves the
        // real server orphaned to PID 1.
        #[cfg(unix)]
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                #[cfg(target_os = "linux")]
                {
                    // SIGKILL bypasses Rust cleanup. Linux ties this signal to
                    // the spawning thread, so spawn_lsp_child uses a process-
                    // lifetime thread rather than the short-lived caller. The
                    // kernel kills the direct child when that thread dies;
                    // normal shutdown separately kills the whole process group.
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() == 1 {
                        return Err(io::Error::other("parent died before LSP spawn completed"));
                    }
                }
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let spawned_at = Instant::now();
        let workspace_loaded_at = SystemTime::now();
        let command_display = truncate_to_bytes(
            &std::iter::once(binary.display().to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
            COMMAND_DISPLAY_BYTES,
        );
        let mut child = spawn_lsp_child(command, &child_registry, reclaim_root, &root, &kind)
            .map_err(|err| {
                // A spawn error from the OS names neither the program nor the
                // directory; both are what a reader needs to fix it.
                io::Error::new(
                    err.kind(),
                    format!(
                        "failed to start `{command_display}` in {}: {err}",
                        root.display()
                    ),
                )
            })?;
        let child_pid = child.id();

        let (stdout, stdin, stderr) =
            match (child.stdout.take(), child.stdin.take(), child.stderr.take()) {
                (Some(stdout), Some(stdin), Some(stderr)) => (stdout, stdin, stderr),
                _ => {
                    // No client will own this child, so stop it here rather
                    // than leave an untracked process behind.
                    kill_lsp_child_group(&mut child);
                    let _ = child.wait();
                    child_registry.untrack(child_pid);
                    return Err(io::Error::other("language server is missing a stdio pipe"));
                }
            };
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
        let stderr_closed = Arc::new(AtomicBool::new(false));
        spawn_stderr_drain_thread(stderr, Arc::clone(&stderr_tail), Arc::clone(&stderr_closed));

        let writer = match LspWriter::spawn(stdin) {
            Ok(writer) => writer,
            Err(err) => {
                kill_lsp_child_group(&mut child);
                let _ = child.wait();
                child_registry.untrack(child_pid);
                return Err(err);
            }
        };
        let pending = Arc::new(Mutex::new(PendingMap::new()));
        let watched_file_registrations = Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_writer = writer.clone();
        let reader_watched_file_registrations = Arc::clone(&watched_file_registrations);
        let reader_kind = kind.clone();
        let reader_root = root.clone();
        let rust_check_begins_read = Arc::new(AtomicU64::new(0));
        let reader_check_begins = Arc::clone(&rust_check_begins_read);
        let rust_check_begin_times = Arc::new(Mutex::new(HashMap::new()));
        let reader_check_begin_times = Arc::clone(&rust_check_begin_times);

        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match transport::read_message(&mut reader) {
                    Ok(Some(ServerMessage::Response(response))) => {
                        reader_writer.note_received();
                        if let Ok(mut guard) = reader_pending.lock() {
                            if let Some(tx) = guard.remove(&response.id) {
                                if tx.send(response).is_err() {
                                    log::debug!("response channel closed");
                                }
                            }
                        } else {
                            let _ = event_tx.send(LspEvent::ServerExited {
                                server_kind: reader_kind.clone(),
                                root: reader_root.clone(),
                                pid: child_pid,
                                reason: ServerExitReason::PendingLockPoisoned,
                            });
                            break;
                        }
                    }
                    Ok(Some(ServerMessage::Notification { method, params })) => {
                        reader_writer.note_received();
                        if method == "$/progress" && is_rust_check_begin(params.as_ref()) {
                            let ordinal = reader_check_begins.fetch_add(1, Ordering::SeqCst) + 1;
                            if let Ok(mut times) = reader_check_begin_times.lock() {
                                times.insert(ordinal, SystemTime::now());
                            }
                        }
                        let _ = event_tx.send(LspEvent::Notification {
                            server_kind: reader_kind.clone(),
                            root: reader_root.clone(),
                            method,
                            params,
                        });
                    }
                    Ok(Some(ServerMessage::Request { id, method, params })) => {
                        reader_writer.note_received();
                        record_watched_file_registration(
                            &reader_watched_file_registrations,
                            &method,
                            params.as_ref(),
                        );
                        // Auto-respond to server requests to prevent deadlocks.
                        // Server requests (like client/registerCapability,
                        // window/workDoneProgress/create) block the server until
                        // we respond. If we don't respond, the server won't send
                        // responses to OUR pending requests → deadlock.
                        //
                        // Dispatch by method to return correct types:
                        // - workspace/configuration expects Vec<Value> (one per item)
                        // - Everything else gets null (safe default for registration/progress)
                        let response_value = if method == "workspace/configuration" {
                            workspace_configuration_response(
                                &reader_kind,
                                &reader_root,
                                params.as_ref(),
                            )
                        } else {
                            serde_json::Value::Null
                        };
                        let response =
                            super::jsonrpc::OutgoingResponse::success(id.clone(), response_value);
                        if let Ok(payload) = serde_json::to_string(&response) {
                            reader_writer.send_response(payload);
                        }
                        // Also forward as event for any interested handlers
                        let _ = event_tx.send(LspEvent::ServerRequest {
                            server_kind: reader_kind.clone(),
                            root: reader_root.clone(),
                            id,
                            method,
                            params,
                        });
                    }
                    terminal @ (Ok(None) | Err(_)) => {
                        if let Ok(mut guard) = reader_pending.lock() {
                            guard.clear();
                        }
                        let _ = event_tx.send(LspEvent::ServerExited {
                            server_kind: reader_kind.clone(),
                            root: reader_root.clone(),
                            pid: child_pid,
                            reason: ServerExitReason::from_read_result(terminal),
                        });
                        break;
                    }
                }
            }
        });

        let rust_analyzer_quiescent = !matches!(&kind, ServerKind::Rust);
        child_registry.mark_client_live(child_pid);
        Ok(Self {
            runtime_note: None,
            kind,
            root,
            state: ServerState::Starting,
            child,
            child_pid,
            writer,
            pending,
            next_id: AtomicI64::new(1),
            diagnostic_caps: None,
            rust_analyzer_quiescent,
            rust_analyzer_failure: None,
            rust_analyzer_warning: None,
            rust_workspace_check_owed_since: None,
            rust_flycheck_running: HashMap::new(),
            rust_completed_check_begin: None,
            rust_check_begins_at_load: 0,
            rust_flycheck_finished_at: None,
            rust_flycheck_started_at: None,
            rust_save: None,
            rust_check_begins_read,
            rust_check_begin_times,
            completed_rust_check: None,
            rust_check_begins_drained: 0,
            save_notification: None,
            rust_checks_on_save: false,
            rust_checks_on_load: false,
            workspace_loaded_at,
            supports_watched_files: false,
            watched_file_registrations,
            child_registry,
            stderr_tail,
            stderr_closed,
            spawned_at,
            command_display,
            #[cfg(test)]
            suppress_kill_on_drop: false,
        })
    }

    /// Send the initialize request and wait for response. Transition to Ready.
    pub fn initialize(
        &mut self,
        workspace_root: &Path,
        initialization_options: Option<serde_json::Value>,
    ) -> Result<lsp_types::InitializeResult, LspError> {
        self.initialize_with_timeout(
            workspace_root,
            initialization_options,
            HANDSHAKE_REQUEST_TIMEOUT,
        )
    }

    /// Initialize within a caller-owned deadline rather than extending it with
    /// the normal standalone handshake budget.
    pub(crate) fn initialize_with_timeout(
        &mut self,
        workspace_root: &Path,
        initialization_options: Option<serde_json::Value>,
        timeout: Duration,
    ) -> Result<lsp_types::InitializeResult, LspError> {
        self.ensure_can_send()?;
        self.state = ServerState::Initializing;

        let root_url = path_to_uri(workspace_root)?;
        let root_uri = lsp_types::Uri::from_str(root_url.as_str()).map_err(|_| {
            LspError::NotFound(format!(
                "failed to convert workspace root '{}' to file URI",
                workspace_root.display()
            ))
        })?;

        let mut params_value = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "experimental": {
                    "serverStatusNotification": true
                },
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "didChangeWatchedFiles": {
                        "dynamicRegistration": true
                    },
                    // LSP 3.17 workspace diagnostic pull. We declare refreshSupport=false
                    // because we drive diagnostics on-demand via pull/push and re-query
                    // when the agent calls lsp_diagnostics again — we don't need the
                    // server to proactively push refresh notifications.
                    "diagnostic": {
                        "refreshSupport": false
                    }
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": true,
                        "willSave": false,
                        "willSaveWaitUntil": false
                    },
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "versionSupport": true,
                        "codeDescriptionSupport": true,
                        "dataSupport": true
                    },
                    // LSP 3.17 textDocument diagnostic pull. dynamicRegistration=false
                    // because we use static capability discovery from the InitializeResult.
                    // relatedDocumentSupport=true to receive cascading diagnostics for
                    // files that became known while analyzing the requested one.
                    "diagnostic": {
                        "dynamicRegistration": false,
                        "relatedDocumentSupport": true
                    }
                }
            },
            "clientInfo": {
                "name": "aft",
                "version": env!("CARGO_PKG_VERSION")
            },
            "workspaceFolders": [
                {
                    "uri": root_uri,
                    "name": workspace_root
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("workspace")
                }
            ]
        });
        let (checks_on_save, checks_on_load) = rust_check_triggers(initialization_options.as_ref());
        if let Some(initialization_options) = initialization_options {
            params_value["initializationOptions"] = initialization_options;
        }
        if matches!(&self.kind, ServerKind::Rust) {
            // rust-analyzer reports its `cargo check` runs only to clients that
            // accept server-initiated progress. Inspect needs those begin/end
            // events to know when compiler errors have been published.
            params_value["capabilities"]["window"] = json!({ "workDoneProgress": true });
        }

        let params = serde_json::from_value::<lsp_types::InitializeParams>(params_value)?;

        let result_value = self.send_request_value_with_timeout(
            <lsp_types::request::Initialize as lsp_types::request::Request>::METHOD,
            params,
            timeout.min(HANDSHAKE_REQUEST_TIMEOUT),
        )?;
        let result: lsp_types::InitializeResult = serde_json::from_value(result_value.clone())?;

        // Capture diagnostic capabilities from the initialize response. We parse
        // from a re-serialized JSON Value because the lsp-types crate's
        // diagnostic_provider strict variants reject some shapes real servers
        // emit (e.g. bare `true`), and we want defensive Default fallback.
        let caps_value = result_value
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| serde_json::to_value(&result.capabilities).unwrap_or(Value::Null));
        self.diagnostic_caps = Some(parse_diagnostic_capabilities(&caps_value));
        self.save_notification = parse_save_notification(&caps_value);
        // rust-analyzer checks on save only when it hears of saves. Another
        // server run for Rust that does not ask for them runs no checks AFT
        // could wait for.
        let checks = matches!(&self.kind, ServerKind::Rust) && self.save_notification.is_some();
        self.rust_checks_on_save = checks && checks_on_save;
        self.rust_checks_on_load = checks && checks_on_load;

        // Capture initialize-time (static) workspace/didChangeWatchedFiles
        // support. Runtime client/registerCapability subscriptions are recorded
        // separately by the reader thread. Missing capability is unsupported by
        // default; callers must not send notifications unless one of those two
        // server opt-in paths is present.
        self.supports_watched_files = caps_value
            .pointer("/workspace/didChangeWatchedFiles/dynamicRegistration")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || caps_value
                .pointer("/workspace/didChangeWatchedFiles")
                .map(|v| v.is_object() || v.as_bool() == Some(true))
                .unwrap_or(false);

        self.send_notification::<lsp_types::notification::Initialized>(serde_json::from_value(
            json!({}),
        )?)?;
        self.state = ServerState::Ready;
        Ok(result)
    }

    /// Diagnostic capabilities advertised by the server. Returns `None` until
    /// `initialize()` has succeeded; returns `Some` with conservative defaults
    /// (all `false`) when the server didn't advertise diagnosticProvider.
    pub fn diagnostic_capabilities(&self) -> Option<&ServerDiagnosticCapabilities> {
        self.diagnostic_caps.as_ref()
    }

    /// Whether diagnostics from this server instance should be treated as
    /// provisional because rust-analyzer is warming or reported failed analysis.
    pub fn diagnostics_are_provisional(&self) -> bool {
        self.is_unresponsive()
            || (matches!(&self.kind, ServerKind::Rust)
                && (!self.rust_analyzer_quiescent || self.rust_analyzer_failure.is_some()))
    }

    pub(crate) fn diagnostic_failure(&self) -> Option<&str> {
        if self.is_unresponsive() {
            Some("server not responding")
        } else {
            self.rust_analyzer_failure.as_deref()
        }
    }

    /// A timed-out server stays alive but is not consulted until its reader
    /// observes another message. A suspended process may belong to the user.
    pub fn is_unresponsive(&self) -> bool {
        self.writer.is_unresponsive()
    }

    pub(crate) fn received_message_count(&self) -> u64 {
        self.writer.received_count()
    }

    pub(crate) fn mark_unresponsive_if_silent(&self, observed: u64) {
        self.writer.mark_unresponsive_if_silent(observed);
    }

    /// Recovery must reach healthy quiescence before reports regain authority.
    pub(crate) fn set_diagnostic_failure(&mut self, failure: Option<String>) {
        if failure.is_some() || self.rust_analyzer_failure.is_some() {
            self.rust_analyzer_quiescent = false;
        }
        self.rust_analyzer_failure = failure;
    }

    /// When this server last started loading the workspace manifests.
    pub(crate) fn workspace_loaded_at(&self) -> SystemTime {
        self.workspace_loaded_at
    }

    /// Enter the state of a workspace reload that was just requested at
    /// `requested_at`: the previous analysis result (a failure or warning, and
    /// quiescence) describes the old manifests, so it is dropped and the
    /// server counts as warming until rust-analyzer reports quiescence for the
    /// new load. Returns the dropped state so a reload the server refused can
    /// be undone with [`Self::restore_rust_workspace_state`].
    pub(crate) fn begin_rust_workspace_reload(
        &mut self,
        requested_at: SystemTime,
    ) -> RustWorkspaceState {
        let previous = RustWorkspaceState {
            quiescent: self.rust_analyzer_quiescent,
            workspace_check_owed_since: self.rust_workspace_check_owed_since,
            failure: self.rust_analyzer_failure.take(),
            warning: self.rust_analyzer_warning.take(),
            loaded_at: self.workspace_loaded_at,
            check_begins_at_load: self.rust_check_begins_at_load,
        };
        self.rust_analyzer_quiescent = false;
        // rust-analyzer checks the reloaded workspace once it is quiescent
        // again; that check is expected from then on.
        self.rust_workspace_check_owed_since = None;
        self.workspace_loaded_at = requested_at;
        self.rust_check_begins_at_load = self.rust_check_begins_read.load(Ordering::Acquire);
        previous
    }

    /// Put back the analysis state saved by
    /// [`Self::begin_rust_workspace_reload`] when the server rejected the
    /// reload request: no new load is running, so no quiescence report would
    /// ever end the warming state.
    pub(crate) fn restore_rust_workspace_state(&mut self, previous: RustWorkspaceState) {
        self.rust_analyzer_quiescent = previous.quiescent;
        self.rust_workspace_check_owed_since = previous.workspace_check_owed_since;
        self.rust_analyzer_failure = previous.failure;
        self.rust_analyzer_warning = previous.warning;
        self.workspace_loaded_at = previous.loaded_at;
        self.rust_check_begins_at_load = previous.check_begins_at_load;
    }

    /// Record a rust-analyzer server-status transition. Returns true only for
    /// first transition to quiescent after startup or failure, the boundary that
    /// makes each latest warming report authoritative.
    pub fn set_rust_analyzer_quiescent(&mut self, quiescent: bool) -> bool {
        if !matches!(&self.kind, ServerKind::Rust) || !quiescent || self.rust_analyzer_quiescent {
            return false;
        }
        self.rust_analyzer_quiescent = true;
        // rust-analyzer starts a check of the whole workspace on becoming
        // quiescent, after any check a save started during the load.
        if self.rust_checks_on_load {
            self.rust_workspace_check_owed_since = Some(Instant::now());
        }
        true
    }

    /// Record one rust-analyzer `$/progress` notification. Only check runs
    /// are tracked: rust-analyzer names their token `rust-analyzer/flycheck/N`
    /// and titles them with the check command (`cargo check`, `cargo clippy`).
    pub(crate) fn record_rust_progress(&mut self, token: &str, kind: &str, title: Option<&str>) {
        if !matches!(&self.kind, ServerKind::Rust) {
            return;
        }
        match kind {
            "begin" => {
                if is_rust_check_progress(token, title) {
                    self.rust_check_begins_drained += 1;
                    let ordinal = self.rust_check_begins_drained;
                    let began = self
                        .rust_check_begin_times
                        .lock()
                        .ok()
                        .and_then(|mut times| times.remove(&ordinal));
                    if let Some(cache) = self.completed_rust_check.as_mut() {
                        if let Some(began) = began {
                            cache.begin(began);
                        }
                    }
                    self.rust_flycheck_running
                        .insert(token.to_string(), ordinal);
                    self.rust_flycheck_started_at = Some(Instant::now());
                    // A check run reads the files from disk as they are now,
                    // so it gives the results the expected workspace check
                    // would have, whatever started it.
                    if ordinal > self.rust_check_begins_at_load {
                        self.rust_workspace_check_owed_since = None;
                    }
                    // For the same reason a run the server announced after a
                    // save was sent covers the saved contents, whatever
                    // started it. Order is judged by when the reader read the
                    // announcement, not by when it is drained here: a run
                    // announced before the save sits in the event channel
                    // until a caller drains it, and checked the files before
                    // the edit. A save deferred and not sent yet is still
                    // expected: this run may have begun before rust-analyzer
                    // received the watcher's change.
                    if self
                        .rust_save
                        .as_ref()
                        .and_then(|save| save.begins_read_at_send)
                        .is_some_and(|read_at_send| ordinal > read_at_send)
                    {
                        self.rust_save = None;
                    }
                }
            }
            "end" => {
                if let Some(begin) = self.rust_flycheck_running.remove(token) {
                    if let Some(cache) = self.completed_rust_check.as_mut() {
                        cache.finished = true;
                    }
                    self.rust_flycheck_finished_at = Some(Instant::now());
                    self.rust_completed_check_begin = Some(
                        self.rust_completed_check_begin
                            .map_or(begin, |previous| previous.max(begin)),
                    );
                }
            }
            _ => {}
        }
    }

    /// How this server wants to be told that a document was saved, or `None`
    /// when it did not ask to hear about saves.
    pub(crate) fn save_notification(&self) -> Option<SaveNotification> {
        self.save_notification
    }

    /// Whether rust-analyzer has begun at least one check run. Before that,
    /// nothing it pushed carries compiler results.
    pub(crate) fn rust_check_seen(&self) -> bool {
        self.rust_flycheck_started_at.is_some()
    }

    /// Record that a `textDocument/didSave` for `uri` was just sent. For
    /// rust-analyzer this starts a new `cargo check`, and the check results
    /// published so far describe the files before the save; see
    /// [`Self::rust_check_state`].
    pub(crate) fn record_save_sent(&mut self, uri: &lsp_types::Uri) {
        if self.rust_checks_on_save {
            let now = Instant::now();
            self.rust_save = Some(RustSaveRequest {
                uri: uri.clone(),
                due_at: None,
                last_sent_at: Some(now),
                sends: 1,
                begins_read_at_send: Some(self.rust_check_begins_read.load(Ordering::SeqCst)),
            });
        }
    }

    /// Ask for a `textDocument/didSave` of `uri` to be sent to rust-analyzer
    /// a little later instead of now, for a change reported by the file
    /// watcher. rust-analyzer 1.98 drops the check a save asks for when the
    /// save arrives right after a `workspace/didChangeWatchedFiles` for the
    /// same file (measured: at once, no check; one second later, a check), so
    /// the save is sent from [`Self::take_rust_save_to_send`] once
    /// [`EXTERNAL_SAVE_DELAY`] has passed. The check counts as requested from
    /// now on.
    pub(crate) fn owe_rust_save(&mut self, uri: &lsp_types::Uri) {
        if self.rust_checks_on_save {
            let now = Instant::now();
            self.rust_save = Some(RustSaveRequest {
                uri: uri.clone(),
                due_at: Some(now + EXTERNAL_SAVE_DELAY),
                last_sent_at: None,
                sends: 0,
                begins_read_at_send: None,
            });
        }
    }

    /// The document to send a `textDocument/didSave` for now, if any: a save
    /// that was deferred (see [`Self::owe_rust_save`]) and is due, or a save
    /// that started no check run within [`SAVE_RESEND_AFTER`] and may be sent
    /// again (at most [`MAX_SAVE_SENDS`] times in all). The caller sends it
    /// and then calls [`Self::mark_rust_save_sent`].
    pub(crate) fn take_rust_save_to_send(&self, now: Instant) -> Option<lsp_types::Uri> {
        let save = self.rust_save.as_ref()?;
        let due = match (save.due_at, save.last_sent_at) {
            (Some(due_at), _) => now >= due_at,
            (None, Some(sent)) => {
                save.sends < MAX_SAVE_SENDS
                    && now.saturating_duration_since(sent) >= SAVE_RESEND_AFTER
            }
            (None, None) => false,
        };
        due.then(|| save.uri.clone())
    }

    /// Record that the save returned by [`Self::take_rust_save_to_send`]
    /// was sent.
    pub(crate) fn mark_rust_save_sent(&mut self, now: Instant) {
        let begins_read = self.rust_check_begins_read.load(Ordering::SeqCst);
        if let Some(save) = self.rust_save.as_mut() {
            save.due_at = None;
            save.last_sent_at = Some(now);
            save.sends = save.sends.saturating_add(1);
            save.begins_read_at_send.get_or_insert(begins_read);
        }
    }

    /// Whether rust-analyzer's published diagnostics carry the compiler's
    /// results for the files as they are now.
    ///
    /// [`RustCheckState::Running`] while a check run is in progress, ended
    /// less than `publish_settle` ago (rust-analyzer can announce the end
    /// before it publishes the final batch), or is expected and still within
    /// its deadline: a save asked for one (until it begins, the published
    /// compiler errors describe the files before the save, including errors
    /// already fixed), or the server became quiescent and has not begun the
    /// workspace check it starts then.
    ///
    /// [`RustCheckState::Unreported`] once an expected check has not begun by
    /// its deadline: [`SAVE_CHECK_START_GRACE`] after the last of
    /// [`MAX_SAVE_SENDS`] sends of a save, or
    /// [`WORKSPACE_CHECK_START_DEADLINE`] after quiescence. A late start and
    /// a check that never comes look the same from here, and in neither case
    /// do the published results describe the current files, so they are
    /// never reported as current.
    ///
    /// [`RustCheckState::Current`] otherwise, for other servers, for a server
    /// still warming (that state is tracked separately), and when the
    /// server's settings run no check that would be expected (see
    /// [`rust_check_triggers`]).
    pub(crate) fn rust_check_state(
        &self,
        now: Instant,
        publish_settle: Duration,
    ) -> RustCheckState {
        if !matches!(&self.kind, ServerKind::Rust) || !self.rust_analyzer_quiescent {
            return RustCheckState::Current;
        }
        if !self.rust_flycheck_running.is_empty() {
            return RustCheckState::Running;
        }
        if let Some(save) = &self.rust_save {
            let awaiting = match save.last_sent_at {
                None => true,
                Some(sent) => {
                    save.sends < MAX_SAVE_SENDS
                        || now.saturating_duration_since(sent) < SAVE_CHECK_START_GRACE
                }
            };
            return if awaiting {
                RustCheckState::Running
            } else {
                RustCheckState::Unreported
            };
        }
        if let Some(owed_since) = self.rust_workspace_check_owed_since {
            return if now.saturating_duration_since(owed_since) < WORKSPACE_CHECK_START_DEADLINE {
                RustCheckState::Running
            } else {
                RustCheckState::Unreported
            };
        }
        match self.rust_flycheck_finished_at {
            Some(finished) if now.saturating_duration_since(finished) < publish_settle => {
                RustCheckState::Running
            }
            _ => RustCheckState::Current,
        }
    }

    /// The earliest moment after `now` at which [`Self::rust_check_state`]
    /// or [`Self::take_rust_save_to_send`] can give a different answer with
    /// no new message from the server. Every other change follows an event.
    pub(crate) fn rust_check_next_timed_change(
        &self,
        now: Instant,
        publish_settle: Duration,
    ) -> Option<Instant> {
        if !matches!(&self.kind, ServerKind::Rust) {
            return None;
        }
        let mut times = Vec::with_capacity(4);
        if let Some(save) = &self.rust_save {
            times.extend(save.due_at);
            if let Some(sent) = save.last_sent_at {
                times.push(sent + SAVE_RESEND_AFTER);
                times.push(sent + SAVE_CHECK_START_GRACE);
            }
        }
        if let Some(owed_since) = self.rust_workspace_check_owed_since {
            times.push(owed_since + WORKSPACE_CHECK_START_DEADLINE);
        }
        if let Some(finished) = self.rust_flycheck_finished_at {
            times.push(finished + publish_settle);
        }
        times.into_iter().filter(|time| *time > now).min()
    }

    /// Authority for a clean whole-workspace compiler result without reports.
    /// Current alone also describes absence of progress, so require a real
    /// matching begin/end after the latest load. Current additionally proves
    /// no save is owed, no check is running, and final publishes have settled;
    /// save begin ordinals reject a check announced before that save.
    pub(crate) fn rust_check_completed_current(
        &self,
        now: Instant,
        publish_settle: Duration,
    ) -> bool {
        matches!(&self.kind, ServerKind::Rust)
            && self.rust_analyzer_quiescent
            && self.rust_analyzer_failure.is_none()
            && self
                .rust_completed_check_begin
                .is_some_and(|begin| begin > self.rust_check_begins_at_load)
            && self.rust_check_state(now, publish_settle) == RustCheckState::Current
    }

    /// When a check was expected and did not begin by its deadline
    /// ([`Self::rust_check_state`] reports [`RustCheckState::Unreported`]),
    /// ask rust-analyzer for it again, so a new caller waits with a fresh
    /// deadline. Without this, a check that never began would leave every
    /// later request unknown, and the "retry" each of them suggests could
    /// never succeed. A save is queued to be sent again by
    /// [`Self::take_rust_save_to_send`]. For the workspace check this returns
    /// true: the caller sends [`RustAnalyzerRunFlycheck`], which starts one.
    pub(crate) fn rearm_unreported_rust_check(&mut self, now: Instant) -> bool {
        if self.rust_check_state(now, Duration::ZERO) != RustCheckState::Unreported {
            return false;
        }
        if let Some(save) = self.rust_save.as_mut() {
            save.due_at = Some(now);
            save.last_sent_at = None;
            save.sends = 0;
            return false;
        }
        self.rust_workspace_check_owed_since = Some(now);
        true
    }

    /// Whether the server advertised initialize-time
    /// `workspace/didChangeWatchedFiles` support. Dynamic registrations are
    /// reported by `has_watched_file_registration()`.
    pub fn supports_watched_files(&self) -> bool {
        self.supports_watched_files
    }

    /// Whether this server currently has an active dynamic watched-file
    /// registration. This, not the initialize-time capability shape, controls
    /// whether `workspace/didChangeWatchedFiles` may be sent.
    pub fn has_watched_file_registration(&self) -> bool {
        self.watched_file_registrations
            .lock()
            .map(|registrations| !registrations.is_empty())
            .unwrap_or(false)
    }

    /// Every watcher from the server's current dynamic registrations, or
    /// `None` when it holds none. `None` is different from an empty set: a
    /// server that only advertised initialize-time support never said which
    /// files it cares about, so the caller chooses a filter for it.
    pub(crate) fn registered_file_watchers(&self) -> Option<RegisteredFileWatchers> {
        let registrations = self.watched_file_registrations.lock().ok()?;
        if registrations.is_empty() {
            return None;
        }
        Some(RegisteredFileWatchers {
            watchers: registrations.values().flatten().cloned().collect(),
        })
    }

    /// Send a request and wait for the response.
    pub fn send_request<R>(&mut self, params: R::Params) -> Result<R::Result, LspError>
    where
        R: lsp_types::request::Request,
        R::Params: serde::Serialize,
        R::Result: DeserializeOwned,
    {
        self.ensure_can_send()?;

        let value = self.send_request_value(R::METHOD, params)?;
        serde_json::from_value(value).map_err(Into::into)
    }

    /// Send a request and wait up to `timeout` for the response. If the local
    /// deadline expires, remove the pending response handler and notify the
    /// server with `$/cancelRequest` so it can stop work.
    pub fn send_request_with_timeout<R>(
        &mut self,
        params: R::Params,
        timeout: Duration,
    ) -> Result<R::Result, LspError>
    where
        R: lsp_types::request::Request,
        R::Params: serde::Serialize,
        R::Result: DeserializeOwned,
    {
        self.ensure_can_send()?;

        let value = self.send_request_value_with_timeout(R::METHOD, params, timeout)?;
        serde_json::from_value(value).map_err(Into::into)
    }

    fn send_request_value<P>(&mut self, method: &'static str, params: P) -> Result<Value, LspError>
    where
        P: serde::Serialize,
    {
        self.send_request_value_with_timeout(method, params, INTERACTIVE_REQUEST_TIMEOUT)
    }

    fn send_request_value_with_timeout<P>(
        &mut self,
        method: &'static str,
        params: P,
        timeout: Duration,
    ) -> Result<Value, LspError>
    where
        P: serde::Serialize,
    {
        self.start_request_value(method, params)?.wait(timeout)
    }

    /// Write a request to the server and return a handle for its response.
    /// Waiting on the handle needs no access to the client, so a caller that
    /// reached the client through the language-server manager lock can
    /// release that lock while the server works on the request.
    pub(crate) fn start_request<R>(
        &mut self,
        params: R::Params,
    ) -> Result<PendingLspRequest, LspError>
    where
        R: lsp_types::request::Request,
        R::Params: serde::Serialize,
    {
        self.start_request_value(R::METHOD, params)
    }

    fn start_request_value<P>(
        &mut self,
        method: &'static str,
        params: P,
    ) -> Result<PendingLspRequest, LspError>
    where
        P: serde::Serialize,
    {
        self.ensure_can_send()?;

        let id = RequestId::Int(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = bounded(1);
        {
            let mut pending = self.lock_pending()?;
            pending.insert(id.clone(), tx);
        }

        let request = Request::new(id.clone(), method, Some(serde_json::to_value(params)?));
        if let Err(err) = self.writer.send(serde_json::to_string(&request)?) {
            self.remove_pending(&id);
            return Err(err);
        }
        Ok(PendingLspRequest {
            id,
            method,
            kind: self.kind.clone(),
            rx,
            pending: Arc::clone(&self.pending),
            writer: self.writer.clone(),
        })
    }

    /// Send a notification (fire-and-forget).
    pub fn send_notification<N>(&mut self, params: N::Params) -> Result<(), LspError>
    where
        N: lsp_types::notification::Notification,
        N::Params: serde::Serialize,
    {
        self.ensure_can_send()?;
        let notification = Notification::new(N::METHOD, Some(serde_json::to_value(params)?));
        self.writer.send(serde_json::to_string(&notification)?)
    }

    /// Write an already serialized notification.
    fn send_serialized_notification(&mut self, json: &str) -> Result<(), LspError> {
        self.ensure_can_send()?;
        self.writer.send(json.to_string())
    }

    /// Send `textDocument/didChange` carrying the whole document (see
    /// [`full_did_change_message`]).
    pub(crate) fn send_full_did_change(
        &mut self,
        uri: &lsp_types::Uri,
        version: i32,
        text: &str,
    ) -> Result<(), LspError> {
        self.ensure_can_send()?;
        let json = full_did_change_message(uri, version, text)?;
        self.send_serialized_notification(&json)
    }

    /// Send `textDocument/didSave`, with the borrowed text when given (see
    /// [`did_save_message`]).
    pub(crate) fn send_did_save_borrowed(
        &mut self,
        uri: &lsp_types::Uri,
        text: Option<&str>,
    ) -> Result<(), LspError> {
        self.ensure_can_send()?;
        let json = did_save_message(uri, text)?;
        self.send_serialized_notification(&json)
    }

    /// Graceful shutdown: send shutdown request, then exit notification.
    pub fn shutdown(&mut self) -> Result<(), LspError> {
        self.shutdown_with_request_timeout(HANDSHAKE_REQUEST_TIMEOUT)
    }

    /// Idle reclaim must not sit on the initialize-length Shutdown handshake.
    /// A short request timeout falls through to the kill-if-still-running error
    /// path so the detached reap thread finishes within `SHUTDOWN_TIMEOUT`.
    pub(crate) fn shutdown_for_idle_reap(&mut self) -> Result<(), LspError> {
        self.shutdown_with_request_timeout(EXIT_POLL_INTERVAL)
    }

    fn shutdown_with_request_timeout(&mut self, request_timeout: Duration) -> Result<(), LspError> {
        if self.state == ServerState::Exited {
            self.child_registry.untrack(self.child_pid);
            return Ok(());
        }

        if self.child.try_wait()?.is_some() {
            self.state = ServerState::Exited;
            self.child_registry.untrack(self.child_pid);
            return Ok(());
        }

        if let Err(err) =
            self.send_request_with_timeout::<lsp_types::request::Shutdown>((), request_timeout)
        {
            self.state = ServerState::ShuttingDown;
            return self.abort_live_child_after_shutdown_error(err);
        }

        if let Err(err) = self.send_notification::<lsp_types::notification::Exit>(()) {
            return self.abort_live_child_after_shutdown_error(err);
        }
        self.state = ServerState::ShuttingDown;

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            if self.child.try_wait()?.is_some() {
                self.state = ServerState::Exited;
                return Ok(());
            }
            if Instant::now() >= deadline {
                // Kill the entire process group, not just the wrapper PID, so
                // npm-wrapped servers (biome's `node biome lsp-proxy` spawns
                // a separate cli-darwin-arm64 child) don't leak orphans.
                kill_lsp_child_group(&mut self.child);
                self.state = ServerState::Exited;
                self.child_registry.untrack(self.child_pid);
                return Err(LspError::Timeout(format!(
                    "timed out waiting for {:?} to exit",
                    self.kind
                )));
            }
            thread::sleep(EXIT_POLL_INTERVAL);
        }
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|tail| stderr_tail_to_string(&tail))
            .unwrap_or_default()
    }

    pub fn child_exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }

    pub fn child_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    pub(crate) fn child_pid(&self) -> u32 {
        self.child_pid
    }

    /// Which part of its life this server is in, for exit reports.
    pub fn phase(&self) -> ServerPhase {
        match self.state {
            ServerState::Starting => ServerPhase::Spawn,
            ServerState::Initializing => ServerPhase::Initialize,
            ServerState::Ready | ServerState::ShuttingDown | ServerState::Exited => {
                ServerPhase::Running
            }
        }
    }

    /// Poll for the child's exit for up to `timeout`, returning its status if
    /// it exited in that time. Does not kill the child.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Build the exit report for this process. Waits briefly for the stderr
    /// reader to reach end-of-file so the server's last words are included.
    pub fn exit_report(
        &self,
        phase: ServerPhase,
        status: Option<std::process::ExitStatus>,
        killed_by_aft: bool,
    ) -> ServerExitReport {
        let elapsed = self.spawned_at.elapsed();
        let deadline = Instant::now() + STDERR_EOF_WAIT;
        while !self.stderr_closed.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        ServerExitReport {
            kind: self.kind.clone(),
            root: self.root.clone(),
            pid: self.child_pid,
            phase,
            status,
            killed_by_aft,
            elapsed,
            command: self.command_display.clone(),
            stderr_tail: self.stderr_tail(),
        }
    }

    /// Reap a client whose reader stopped and describe how the process ended.
    /// After a clean end-of-file the process is normally already exiting, so
    /// give it a moment to report its own status before killing it.
    pub(crate) fn reap_with_report(&mut self, reason: &ServerExitReason) -> ServerExitReport {
        let phase = self.phase();
        if matches!(reason, ServerExitReason::Eof) {
            let _ = self.wait_for_exit(STDERR_EOF_WAIT);
        }
        match self.reap_after_reader_exit(reason) {
            ReaderExitReap::AlreadyExited(status) => self.exit_report(phase, Some(status), false),
            ReaderExitReap::KilledWhileAlive => {
                let killed_status = self.child.try_wait().ok().flatten();
                self.exit_report(phase, killed_status, true)
            }
        }
    }

    /// If the child is still running, kill its process group and wait bounded.
    /// Always untrack. The caller logs whether this was a real exit or a reader
    /// death that left the child alive.
    pub(crate) fn reap_after_reader_exit(&mut self, _reason: &ServerExitReason) -> ReaderExitReap {
        let outcome = match self.child.try_wait() {
            Ok(Some(status)) => ReaderExitReap::AlreadyExited(status),
            Ok(None) | Err(_) => {
                kill_lsp_child_group(&mut self.child);
                self.wait_for_child_exit_bounded();
                ReaderExitReap::KilledWhileAlive
            }
        };
        self.state = ServerState::Exited;
        self.child_registry.untrack(self.child_pid);
        outcome
    }

    fn abort_live_child_after_shutdown_error(&mut self, err: LspError) -> Result<(), LspError> {
        if self.child.try_wait()?.is_some() {
            self.state = ServerState::Exited;
            self.child_registry.untrack(self.child_pid);
            return Ok(());
        }
        kill_lsp_child_group(&mut self.child);
        self.wait_for_child_exit_bounded();
        self.state = ServerState::Exited;
        self.child_registry.untrack(self.child_pid);
        Err(err)
    }

    fn wait_for_child_exit_bounded(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(EXIT_POLL_INTERVAL);
        }
    }

    // Used only by the Unix-gated child-spawning test modules.
    #[cfg(all(test, unix))]
    pub(crate) fn suppress_kill_on_drop_for_test(&mut self) {
        self.suppress_kill_on_drop = true;
    }

    // Used only by the Unix-gated child-spawning test modules.
    #[cfg(all(test, unix))]
    pub(crate) fn poison_writer_for_test(&self) {
        self.writer.poison_for_test();
    }

    pub fn state(&self) -> ServerState {
        self.state
    }

    pub fn kind(&self) -> ServerKind {
        self.kind.clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure_can_send(&self) -> Result<(), LspError> {
        if self.is_unresponsive() {
            return Err(LspError::ServerNotReady("server not responding".into()));
        }
        if matches!(self.state, ServerState::ShuttingDown | ServerState::Exited) {
            return Err(LspError::ServerNotReady(format!(
                "language server {:?} is not ready (state: {:?})",
                self.kind, self.state
            )));
        }
        Ok(())
    }

    fn lock_pending(&self) -> Result<std::sync::MutexGuard<'_, PendingMap>, LspError> {
        self.pending
            .lock()
            .map_err(|_| io::Error::other("pending response map poisoned").into())
    }

    fn remove_pending(&self, id: &RequestId) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(id);
        }
    }
}

#[derive(serde::Serialize)]
struct BorrowedNotification<'a, P> {
    jsonrpc: &'static str,
    method: &'a str,
    params: &'a P,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FullDidChangeParams<'a> {
    text_document: VersionedDocument<'a>,
    content_changes: [FullText<'a>; 1],
}

#[derive(serde::Serialize)]
struct VersionedDocument<'a> {
    uri: &'a lsp_types::Uri,
    version: i32,
}

#[derive(serde::Serialize)]
struct FullText<'a> {
    text: &'a str,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DidSaveParams<'a> {
    text_document: SavedDocument<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct SavedDocument<'a> {
    uri: &'a lsp_types::Uri,
}

/// The `textDocument/didChange` message for a whole-document change, the
/// same JSON as the typed `DidChangeTextDocumentParams` produce. The text is
/// serialized once from the borrowed string; the typed params copied the
/// document into the params and again into a JSON value first, for every
/// server the document is open in.
pub(crate) fn full_did_change_message(
    uri: &lsp_types::Uri,
    version: i32,
    text: &str,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&BorrowedNotification {
        jsonrpc: "2.0",
        method: <lsp_types::notification::DidChangeTextDocument as lsp_types::notification::Notification>::METHOD,
        params: &FullDidChangeParams {
            text_document: VersionedDocument { uri, version },
            content_changes: [FullText { text }],
        },
    })
}

/// The `textDocument/didSave` message, serialized from borrowed text like
/// [`full_did_change_message`].
pub(crate) fn did_save_message(
    uri: &lsp_types::Uri,
    text: Option<&str>,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&BorrowedNotification {
        jsonrpc: "2.0",
        method: <lsp_types::notification::DidSaveTextDocument as lsp_types::notification::Notification>::METHOD,
        params: &DidSaveParams {
            text_document: SavedDocument { uri },
            text,
        },
    })
}

/// A request written to a language server whose response has not been read
/// yet (see [`LspClient::start_request`]).
pub(crate) struct PendingLspRequest {
    id: RequestId,
    method: &'static str,
    kind: ServerKind,
    rx: crossbeam_channel::Receiver<JsonRpcResponse>,
    pending: Arc<Mutex<PendingMap>>,
    writer: LspWriter,
}

impl PendingLspRequest {
    /// Wait up to `timeout` for the response. If the local deadline expires,
    /// remove the pending response handler and notify the server with
    /// `$/cancelRequest` so it can stop work.
    pub(crate) fn wait(self, timeout: Duration) -> Result<Value, LspError> {
        let response = match self.rx.recv_timeout(timeout) {
            Ok(response) => response,
            Err(RecvTimeoutError::Timeout) => {
                self.remove_pending();
                let notification =
                    Notification::new("$/cancelRequest", Some(json!({ "id": self.id })));
                self.writer
                    .mark_unresponsive_if_silent(self.writer.received_count());
                self.writer
                    .send_best_effort(serde_json::to_string(&notification)?);
                return Err(LspError::Timeout(format!(
                    "timed out waiting for '{}' response from {:?}",
                    self.method, self.kind
                )));
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.remove_pending();
                return Err(LspError::ServerNotReady(format!(
                    "language server {:?} disconnected while waiting for '{}'",
                    self.kind, self.method
                )));
            }
        };

        if let Some(error) = response.error {
            return Err(LspError::ServerError {
                code: error.code,
                message: error.message,
            });
        }

        Ok(response.result.unwrap_or(Value::Null))
    }

    fn remove_pending(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.suppress_kill_on_drop {
            // Test-only crash seam: retain the tracked child so the reaper and
            // lifecycle census observe the same orphan signature as a failed
            // client teardown instead of hiding it by untracking first.
            self.child_registry.mark_client_gone(self.child_pid);
            return;
        }
        // Record the transition before normal teardown untracks it. A control
        // thread that snapshots in this narrow window sees an honest orphan
        // rather than a child that is still reported as client-owned.
        self.child_registry.mark_client_gone(self.child_pid);
        // Untrack before the synchronous kill so signal cleanup cannot race this
        // normal teardown.
        self.child_registry.untrack(self.child_pid);
        kill_lsp_child_group(&mut self.child);
    }
}

fn spawn_stderr_drain_thread(
    stderr: std::process::ChildStderr,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_closed: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(mut tail) = stderr_tail.lock() {
                        append_stderr_tail(&mut tail, &line);
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        stderr_closed.store(true, Ordering::Release);
    });
}

fn append_stderr_tail(tail: &mut VecDeque<String>, line: &str) {
    if tail.len() == STDERR_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(trim_stderr_line(line));
}

fn trim_stderr_line(line: &str) -> String {
    let line = line.trim_end_matches(|ch| ch == '\r' || ch == '\n');
    if line.len() <= STDERR_LINE_BYTES {
        return line.to_string();
    }

    let mut start = line.len() - STDERR_LINE_BYTES;
    while start < line.len() && !line.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &line[start..])
}

fn stderr_tail_to_string(tail: &VecDeque<String>) -> String {
    tail.iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Force-terminate an LSP child and its entire process group on Unix.
/// On Windows, `taskkill /F /T` kills the process tree.
///
/// Necessary because some LSP servers ship as npm-installed Node shims that
/// spawn the real binary as a child. Killing only the wrapper PID leaves the
/// real server orphaned to PID 1 and accumulates over time.
fn kill_lsp_child_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pgid = child.id() as i32;
        crate::bash_background::process::terminate_pgid(pgid, Some(child));
        let _ = child.wait();
    }
    #[cfg(not(unix))]
    {
        crate::bash_background::process::terminate_process(child);
        let _ = child.wait();
    }
}

fn record_watched_file_registration(
    registrations: &WatchedFileRegistrations,
    method: &str,
    params: Option<&Value>,
) {
    match method {
        "client/registerCapability" => {
            let Some(items) = params
                .and_then(|params| params.get("registrations"))
                .and_then(|registrations| registrations.as_array())
            else {
                return;
            };
            if let Ok(mut guard) = registrations.lock() {
                for item in items {
                    if item.get("method").and_then(Value::as_str)
                        == Some("workspace/didChangeWatchedFiles")
                    {
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            let watchers = item
                                .pointer("/registerOptions/watchers")
                                .and_then(Value::as_array)
                                .map(|watchers| {
                                    watchers
                                        .iter()
                                        .filter_map(RegisteredFileWatcher::parse)
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            log::debug!(
                                "server registered watched files id={id} watchers={:?}",
                                item.pointer("/registerOptions/watchers")
                            );
                            guard.insert(id.to_string(), watchers);
                        }
                    }
                }
            }
        }
        "client/unregisterCapability" => {
            let Some(items) = params
                .and_then(|params| params.get("unregisterations"))
                .and_then(|registrations| registrations.as_array())
            else {
                return;
            };
            if let Ok(mut guard) = registrations.lock() {
                for item in items {
                    if item.get("method").and_then(Value::as_str)
                        == Some("workspace/didChangeWatchedFiles")
                    {
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            guard.remove(id);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn workspace_configuration_response(
    kind: &ServerKind,
    root: &Path,
    params: Option<&Value>,
) -> Value {
    let items = params
        .and_then(|params| params.get("items"))
        .and_then(Value::as_array);
    let python_path = (kind == &ServerKind::Python)
        .then(|| project_python_path(root))
        .flatten()
        // Lossy on purpose: PathBuf's Serialize rejects non-UTF-8 bytes and a
        // panic here would wedge the reader thread mid-handshake. A lossy
        // interpreter path degrades one exotic workspace instead.
        .map(|path| path.to_string_lossy().into_owned());

    Value::Array(match items {
        Some(items) => items
            .iter()
            .map(|item| {
                if item.get("section").and_then(Value::as_str) == Some("python") {
                    if let Some(path) = &python_path {
                        return json!({ "pythonPath": path });
                    }
                }
                Value::Null
            })
            .collect(),
        None => vec![Value::Null],
    })
}

fn project_python_path(root: &Path) -> Option<PathBuf> {
    [root.join(".venv"), root.join("venv")]
        .into_iter()
        .find_map(|virtualenv| {
            if cfg!(windows) {
                let candidate = virtualenv.join("Scripts").join("python.exe");
                return candidate.is_file().then_some(candidate);
            }

            ["python", "python3"]
                .into_iter()
                .map(|binary| virtualenv.join("bin").join(binary))
                .find(|candidate| candidate.is_file())
        })
}

/// Parse `ServerDiagnosticCapabilities` from a re-serialized
/// `ServerCapabilities` JSON value.
///
/// LSP 3.17 spec for `diagnosticProvider`:
/// - `capabilities.diagnosticProvider` may be absent (no pull support),
///   `DiagnosticOptions`, or `DiagnosticRegistrationOptions`.
/// - If present:
///   - `interFileDependencies: bool` (we don't currently use this)
///   - `workspaceDiagnostics: bool` → workspace pull support
///   - `identifier?: string` → optional identifier scoping result IDs
///
/// We parse the raw JSON Value defensively: presence of any
/// `diagnosticProvider` value (object or `true`) means the server supports
/// at least `textDocument/diagnostic` pull.
fn parse_diagnostic_capabilities(value: &Value) -> ServerDiagnosticCapabilities {
    let mut caps = ServerDiagnosticCapabilities::default();

    if let Some(provider) = value.get("diagnosticProvider") {
        // diagnosticProvider can be `true` (rare) or an object. Treat both as
        // pull_diagnostics support.
        if provider.is_object() || provider.as_bool() == Some(true) {
            caps.pull_diagnostics = true;
        }

        if let Some(obj) = provider.as_object() {
            if obj
                .get("workspaceDiagnostics")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                caps.workspace_diagnostics = true;
            }
            if let Some(identifier) = obj.get("identifier").and_then(|v| v.as_str()) {
                caps.identifier = Some(identifier.to_string());
            }
        }
    }

    // Workspace diagnostic refresh (rare — most servers don't request this,
    // and we declared refreshSupport=false in our client capabilities anyway).
    if let Some(refresh) = value
        .get("workspace")
        .and_then(|w| w.get("diagnostic"))
        .and_then(|d| d.get("refreshSupport"))
        .and_then(|r| r.as_bool())
    {
        caps.refresh_support = refresh;
    }

    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the Unix-gated reap tests below spawn real children and name paths.
    #[cfg(unix)]
    use std::collections::HashMap;
    use std::io::{BufReader, Cursor};
    #[cfg(unix)]
    use std::path::{Path, PathBuf};

    #[test]
    fn parse_caps_no_diagnostic_provider() {
        let value = json!({});
        let caps = parse_diagnostic_capabilities(&value);
        assert!(!caps.pull_diagnostics);
        assert!(!caps.workspace_diagnostics);
        assert!(caps.identifier.is_none());
    }

    #[test]
    fn parse_caps_basic_pull_only() {
        let value = json!({
            "diagnosticProvider": {
                "interFileDependencies": false,
                "workspaceDiagnostics": false
            }
        });
        let caps = parse_diagnostic_capabilities(&value);
        assert!(caps.pull_diagnostics);
        assert!(!caps.workspace_diagnostics);
    }

    #[test]
    fn parse_caps_full_pull_with_workspace() {
        let value = json!({
            "diagnosticProvider": {
                "interFileDependencies": true,
                "workspaceDiagnostics": true,
                "identifier": "rust-analyzer"
            }
        });
        let caps = parse_diagnostic_capabilities(&value);
        assert!(caps.pull_diagnostics);
        assert!(caps.workspace_diagnostics);
        assert_eq!(caps.identifier.as_deref(), Some("rust-analyzer"));
    }

    #[test]
    fn parse_caps_provider_as_bare_true() {
        // LSP 3.17 allows DiagnosticOptions OR boolean — treat true as pull_diagnostics
        let value = json!({
            "diagnosticProvider": true
        });
        let caps = parse_diagnostic_capabilities(&value);
        assert!(caps.pull_diagnostics);
        assert!(!caps.workspace_diagnostics);
    }

    #[test]
    fn borrowed_document_messages_match_the_typed_notifications() {
        use lsp_types::notification::{
            DidChangeTextDocument, DidSaveTextDocument, Notification as _,
        };
        let uri: lsp_types::Uri = "file:///work/src/main.rs".parse().expect("uri");
        let text = "fn main() {\n    println!(\"\\u{1F600} \\t\");\n}\n";
        let typed = |method: &str, params: serde_json::Value| {
            serde_json::to_value(crate::lsp::jsonrpc::Notification::new(method, Some(params)))
                .expect("typed notification")
        };
        let change = typed(
            DidChangeTextDocument::METHOD,
            serde_json::to_value(lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier::new(uri.clone(), 7),
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.to_string(),
                }],
            })
            .expect("change params"),
        );
        let borrowed: serde_json::Value =
            serde_json::from_str(&full_did_change_message(&uri, 7, text).expect("message"))
                .expect("borrowed change parses");
        assert_eq!(borrowed, change);

        for with_text in [Some(text), None] {
            let save = typed(
                DidSaveTextDocument::METHOD,
                serde_json::to_value(lsp_types::DidSaveTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier::new(uri.clone()),
                    text: with_text.map(str::to_string),
                })
                .expect("save params"),
            );
            let borrowed: serde_json::Value =
                serde_json::from_str(&did_save_message(&uri, with_text).expect("message"))
                    .expect("borrowed save parses");
            assert_eq!(borrowed, save);
        }
    }

    #[test]
    fn interactive_request_timeout_is_eight_seconds() {
        assert_eq!(INTERACTIVE_REQUEST_TIMEOUT, Duration::from_secs(8));
    }

    #[test]
    fn handshake_request_timeout_remains_thirty_seconds() {
        assert_eq!(HANDSHAKE_REQUEST_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn parse_caps_workspace_refresh_support() {
        let value = json!({
            "workspace": {
                "diagnostic": {
                    "refreshSupport": true
                }
            }
        });
        let caps = parse_diagnostic_capabilities(&value);
        assert!(caps.refresh_support);
        // No diagnosticProvider → pull still false
        assert!(!caps.pull_diagnostics);
    }

    #[test]
    fn pyright_configuration_uses_workspace_virtualenv_interpreter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let python = if cfg!(windows) {
            root.join(".venv").join("Scripts").join("python.exe")
        } else {
            root.join(".venv").join("bin").join("python")
        };
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, []).unwrap();
        let params = json!({
            "items": [
                { "section": "python" },
                { "section": "pyright" }
            ]
        });

        let response = workspace_configuration_response(&ServerKind::Python, root, Some(&params));

        assert_eq!(response[0]["pythonPath"], python.display().to_string());
        assert!(response[1].is_null());
    }

    #[test]
    fn ty_configuration_does_not_receive_pyright_interpreter_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let python = if cfg!(windows) {
            root.join(".venv").join("Scripts").join("python.exe")
        } else {
            root.join(".venv").join("bin").join("python")
        };
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(python, []).unwrap();
        let params = json!({ "items": [{ "section": "python" }] });

        let response = workspace_configuration_response(&ServerKind::Ty, root, Some(&params));

        assert!(response[0].is_null());
    }

    #[test]
    fn eof_read_maps_to_eof_reason() {
        let mut reader = BufReader::new(Cursor::new([]));
        let result = transport::read_message(&mut reader);
        assert!(matches!(result, Ok(None)));
        assert_eq!(
            ServerExitReason::from_read_result(result),
            ServerExitReason::Eof
        );
    }

    #[test]
    fn malformed_frame_maps_to_read_error_reason() {
        let mut reader = BufReader::new(Cursor::new(b"Content-Length: 3\r\n\r\n{{{"));
        let result = transport::read_message(&mut reader);
        assert!(result.is_err());
        match ServerExitReason::from_read_result(result) {
            ServerExitReason::ReadError(message) => assert!(
                !message.is_empty(),
                "ReadError must carry the concrete framing error"
            ),
            other => panic!("expected ReadError, got {other:?}"),
        }
    }

    #[cfg(unix)]
    fn spawn_long_lived_client(
        script: &str,
        event_tx: Sender<LspEvent>,
        registry: LspChildRegistry,
        root: PathBuf,
    ) -> LspClient {
        LspClient::spawn(
            ServerKind::TypeScript,
            root,
            Path::new("sh"),
            &["-c".to_string(), script.to_string()],
            &HashMap::new(),
            event_tx,
            registry,
        )
        .expect("spawn long-lived LSP stand-in")
    }

    #[cfg(unix)]
    #[test]
    fn completed_check_authority_requires_a_begin_after_the_latest_load_and_save() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let tmp = tempfile::tempdir().unwrap();
        let mut client = spawn_long_lived_client(
            "exec sleep 60",
            tx,
            LspChildRegistry::new(),
            tmp.path().to_path_buf(),
        );
        client.kind = ServerKind::Rust;
        client.rust_analyzer_quiescent = true;
        client.rust_checks_on_save = true;
        assert_eq!(
            client.rust_check_state(Instant::now(), Duration::ZERO),
            RustCheckState::Current
        );
        assert!(!client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        client.record_rust_progress("rust-analyzer/flycheck/0", "end", None);
        assert!(!client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        client.rust_check_begins_read.store(1, Ordering::SeqCst);
        client.record_rust_progress("rust-analyzer/flycheck/0", "begin", Some("cargo check"));
        assert!(!client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        client.record_rust_progress("rust-analyzer/flycheck/0", "end", None);
        assert!(client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        assert!(!client.rust_check_completed_current(Instant::now(), Duration::from_secs(1)));

        // A begin read before the save must not become its authority merely
        // because the event channel is drained after sending the save.
        let uri = "file:///test/lib.rs".parse().unwrap();
        client.rust_check_begins_read.store(2, Ordering::SeqCst);
        client.record_save_sent(&uri);
        client.record_rust_progress("rust-analyzer/flycheck/1", "begin", Some("cargo check"));
        client.record_rust_progress("rust-analyzer/flycheck/1", "end", None);
        assert!(!client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        client.rust_check_begins_read.store(3, Ordering::SeqCst);
        client.record_rust_progress("rust-analyzer/flycheck/2", "begin", Some("cargo check"));
        client.record_rust_progress("rust-analyzer/flycheck/2", "end", None);
        assert!(client.rust_check_completed_current(Instant::now(), Duration::ZERO));

        // Reloading the workspace requires a check begun after the reload;
        // an earlier completed check no longer certifies the new manifests.
        client.rust_check_begins_read.store(4, Ordering::SeqCst);
        let previous = client.begin_rust_workspace_reload(SystemTime::now());
        client.set_rust_analyzer_quiescent(true);
        client.record_rust_progress("rust-analyzer/flycheck/3", "begin", Some("cargo check"));
        client.record_rust_progress("rust-analyzer/flycheck/3", "end", None);
        assert!(!client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        client.restore_rust_workspace_state(previous);
        assert!(client.rust_check_completed_current(Instant::now(), Duration::ZERO));
        client.begin_rust_workspace_reload(SystemTime::now());
        client.set_rust_analyzer_quiescent(true);
        client.rust_check_begins_read.store(5, Ordering::SeqCst);
        client.record_rust_progress("rust-analyzer/flycheck/4", "begin", Some("cargo check"));
        client.record_rust_progress("rust-analyzer/flycheck/4", "end", None);
        assert!(client.rust_check_completed_current(Instant::now(), Duration::ZERO));
    }

    #[cfg(unix)]
    #[test]
    fn reader_emits_read_error_for_malformed_frame() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let registry = LspChildRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let client = spawn_long_lived_client(
            "printf 'Content-Length: 3\r\n\r\n{{{'; exec sleep 60",
            tx,
            registry,
            tmp.path().to_path_buf(),
        );
        let event = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader should emit ServerExited");
        match event {
            LspEvent::ServerExited {
                reason: ServerExitReason::ReadError(message),
                ..
            } => assert!(!message.is_empty()),
            other => panic!("expected ReadError ServerExited, got {other:?}"),
        }
        drop(client);
    }

    #[cfg(unix)]
    #[test]
    fn reader_emits_eof_when_child_closes_stdout() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let registry = LspChildRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let client = spawn_long_lived_client("exit 0", tx, registry, tmp.path().to_path_buf());
        let event = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader should emit ServerExited on EOF");
        match event {
            LspEvent::ServerExited {
                reason: ServerExitReason::Eof,
                ..
            } => {}
            other => panic!("expected Eof ServerExited, got {other:?}"),
        }
        drop(client);
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_error_kills_and_untracks_live_child() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let registry = LspChildRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let mut client = spawn_long_lived_client(
            "exec sleep 60",
            tx,
            registry.clone(),
            tmp.path().to_path_buf(),
        );
        let pid = client.child_pid();
        assert!(
            registry.pids().contains(&pid),
            "child must be tracked before shutdown"
        );
        assert!(
            crate::bash_background::process::is_process_alive(pid),
            "child must still be running"
        );
        client.poison_writer_for_test();
        let result = client.shutdown();
        assert!(result.is_err(), "shutdown must return Err, got {result:?}");
        assert!(
            !crate::bash_background::process::is_process_alive(pid),
            "shutdown Err must not leave a live child"
        );
        assert!(
            !registry.pids().contains(&pid),
            "shutdown Err must untrack the child"
        );
    }
}
