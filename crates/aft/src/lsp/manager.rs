use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use lsp_types::notification::{
    DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
};
use lsp_types::{
    DidChangeWatchedFilesParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, FileChangeType, FileEvent, TextDocumentIdentifier, TextDocumentItem,
};

use crate::alert_state::AcceptedDiagnosticSnapshot;
use crate::config::Config;
use crate::lsp::child_registry::LspChildRegistry;
use crate::lsp::client::{
    LspClient, LspEvent, RustAnalyzerRunFlycheck, RustCheckState, SaveNotification,
    ServerExitReport, ServerPhase, ServerState, FLYCHECK_PUBLISH_SETTLE,
};
use crate::lsp::diagnostics::{
    from_lsp_diagnostics, DiagnosticEntry, DiagnosticsStore, StoredDiagnostic,
};
use crate::lsp::document::{DiskSnapshot, DocumentStore};
use crate::lsp::position::{uri_for_path, uri_to_path};
use crate::lsp::pull_params::{
    AftDocumentDiagnosticParams, AftDocumentDiagnosticRequest, AftWorkspaceDiagnosticParams,
    AftWorkspaceDiagnosticRequest,
};
use crate::lsp::registry::{
    is_config_file_path_with_custom, resolve_server_binary, servers_for_file,
    servers_with_root_marker, ServerDef, ServerKind,
};
use crate::lsp::roots::ServerKey;
use crate::lsp::typescript_project::{
    find_project_typescript_package, native_server_misidentified_reason,
    native_server_unavailable_reason, resolve_native_binary, unservable_typescript_reason,
    ProjectTypeScript, NATIVE_SERVER_INFO_NAME,
};
use crate::lsp::LspError;
use crate::slog_error;
use crate::slog_info;

const STDERR_REASON_BYTES: usize = 2 * 1024;
/// The total grace period for draining every LSP client during process shutdown.
/// It is a hard ceiling: forced termination of servers that ignore the
/// graceful handshake happens inside it, not after it.
pub const LSP_SHUTDOWN_ALL_BUDGET: Duration = Duration::from_millis(1500);

/// The tail of [`LSP_SHUTDOWN_ALL_BUDGET`] reserved for forced termination.
/// Servers still running when only this much of the budget is left are
/// killed, and the rest of the budget is the only time spent waiting for
/// those kills to be reaped. Killing at the end of the budget instead let
/// the kill and reap run past it on a loaded machine.
pub const LSP_FORCED_TERMINATION_RESERVE: Duration = Duration::from_millis(300);

fn server_key_for_definition(
    def: &ServerDef,
    file_path: &Path,
    config: &Config,
) -> Option<ServerKey> {
    def.workspace_root_for_file_with_project_root(file_path, config.project_root.as_deref())
        .map(|root| ServerKey {
            kind: def.kind.clone(),
            root,
        })
}

fn server_key_sort(left: &ServerKey, right: &ServerKey) -> std::cmp::Ordering {
    // Native TypeScript servers share an id and can share a root, so the last
    // comparison orders them by their TypeScript directory.
    let native_dir = |kind: &ServerKind| match kind {
        ServerKind::TypeScriptNative(dir) => Some(Arc::clone(dir)),
        _ => None,
    };
    left.kind
        .id_str()
        .cmp(right.kind.id_str())
        .then(left.root.cmp(&right.root))
        .then_with(|| native_dir(&left.kind).cmp(&native_dir(&right.kind)))
}

/// Outcome of attempting to ensure a server is running for a single matching
/// `ServerDef`. Returned per matching server so the caller can report exactly
/// what happened to the user instead of collapsing all failures into "no
/// server".
#[derive(Debug, Clone)]
pub enum ServerAttemptResult {
    /// Server is running and ready to serve requests for this file.
    Ok { server_key: ServerKey },
    /// No workspace root was found by walking up from the file looking for
    /// any of the server's configured root markers.
    NoRootMarker { looked_for: Vec<String> },
    /// The server's binary could not be found on PATH (or override was
    /// missing/invalid).
    BinaryNotInstalled { binary: String },
    /// Binary was found but spawning or initializing the server failed.
    SpawnFailed { binary: String, reason: String },
}

/// One server's attempt to handle a file.
#[derive(Debug, Clone)]
pub struct ServerAttempt {
    /// Stable server identifier (kind ID, e.g. "pyright", "rust-analyzer").
    pub server_id: String,
    /// Server display name from the registry.
    pub server_name: String,
    pub result: ServerAttemptResult,
}

/// Aggregate outcome of `ensure_server_for_file_detailed`. Distinguishes:
/// - "No server registered for this file's extension" (`attempts.is_empty()`)
/// - "Servers registered but none could start" (`successful.is_empty()` but
///   `!attempts.is_empty()`)
/// - "At least one server is ready" (`!successful.is_empty()`)
#[derive(Debug, Clone, Default)]
pub struct EnsureServerOutcomes {
    /// Server keys that are now running and ready to serve requests.
    pub successful: Vec<ServerKey>,
    /// Per-server attempt records. Empty if no server is registered for the
    /// file's extension.
    pub attempts: Vec<ServerAttempt>,
}

/// Side-effect-free result of resolving the server kinds applicable to an
/// inspection root. The public record set is deliberately only `ServerKey`s;
/// the definitions retained internally are used later by the explicit start
/// step and never cause a process to be opened during resolution.
#[derive(Clone, Debug)]
pub struct ApplicableServerSnapshot {
    pub server_keys: Vec<ServerKey>,
    /// Servers whose workspace root marker was found in the resolved area but
    /// which have no file there to analyze, so none of them is started. They
    /// are reported so a Rust repository that ships a `package.json` only to
    /// install a tool reads as "TypeScript not applicable", not as silence or
    /// as a broken TypeScript project.
    pub not_applicable: Vec<NotApplicableServer>,
    candidates: Vec<ApplicableServerCandidate>,
    producer_failures: Vec<ApplicableServerFailure>,
}

/// A configured server that has a root marker but nothing to analyze.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotApplicableServer {
    /// Stable server identifier (kind ID, e.g. "typescript").
    pub server_id: String,
    /// The root marker file that was found (for example `package.json`).
    pub marker: String,
    /// File extensions the server handles, none of which were found.
    pub extensions: Vec<String>,
}

impl NotApplicableServer {
    pub fn reason(&self) -> String {
        let extensions = self
            .extensions
            .iter()
            .map(|extension| format!(".{extension}"))
            .collect::<Vec<_>>()
            .join("/");
        format!("{} found, but no {extensions} files", self.marker)
    }
}

#[derive(Clone, Debug)]
struct ApplicableServerCandidate {
    key: ServerKey,
    definition: ServerDef,
    source_file: PathBuf,
}

/// What an applicability walk found on disk, before any manager state (cached
/// spawn failures, binary resolution) is consulted.
///
/// The walk reads the whole inspected area and can take seconds on a large
/// tree or a slow filesystem, so it runs without the `LspManager` lock; only
/// the short classification in [`LspManager::classify_applicable_servers`]
/// needs the manager. Holding the lock across the walk stalled every other
/// user of the manager, including the standalone request loop.
#[derive(Clone, Debug)]
pub struct ApplicabilityWalk {
    /// The first walked file for each server key, in walk order.
    candidates: Vec<ApplicableServerCandidate>,
    /// First root marker seen per server kind, with the directory holding it
    /// and the marker's file name.
    markers: HashMap<ServerKind, (ServerDef, PathBuf, String)>,
}

#[derive(Clone, Debug)]
pub enum ApplicabilityResolutionError {
    RootUnreadable { root: PathBuf, reason: String },
    RequestDeadline { root: PathBuf },
}

#[derive(Clone, Debug)]
pub struct ApplicableServerFailure {
    pub server_key: ServerKey,
    pub result: ServerAttemptResult,
}

impl ApplicableServerFailure {
    pub fn reason(&self) -> String {
        self.result.failure_reason()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ApplicableServerStartOutcomes {
    pub successful: Vec<ServerKey>,
    pub failures: Vec<ApplicableServerFailure>,
    /// The first producer whose startup could not complete before the inspect
    /// request's shared work deadline.
    pub deadline_exceeded: Option<ServerKey>,
}

impl ServerAttemptResult {
    pub fn failure_reason(&self) -> String {
        match self {
            Self::BinaryNotInstalled { binary } => format!("{binary} is unavailable"),
            Self::SpawnFailed { reason, .. } => summarize_failure_reason(reason),
            Self::NoRootMarker { looked_for } => {
                format!(
                    "no workspace root marker found (looked for {})",
                    looked_for.join(", ")
                )
            }
            Self::Ok { .. } => "server started successfully".to_string(),
        }
    }
}

impl EnsureServerOutcomes {
    /// True if no server in the registry matched this file's extension.
    pub fn no_server_registered(&self) -> bool {
        self.attempts.is_empty()
    }

    /// True when servers matched the file's extension but none actually apply
    /// to this project — i.e. nothing started and every attempt failed the root
    /// marker check (e.g. oxlint registered for `.ts` with no `.oxlintrc.json`).
    /// Distinct from `no_server_registered` (extension unsupported) and from a
    /// real outage (binary missing / spawn failed): a missing root marker is a
    /// filesystem fact that never changes mid-scan, so such a file will never
    /// produce diagnostics and must not be reported as "pending".
    pub fn only_inapplicable_root_markers(&self) -> bool {
        self.successful.is_empty()
            && !self.attempts.is_empty()
            && self
                .attempts
                .iter()
                .all(|attempt| matches!(attempt.result, ServerAttemptResult::NoRootMarker { .. }))
    }
}

/// Outcome of a post-edit diagnostics wait. Reports the per-server status
/// alongside the fresh diagnostics, so the response layer can build an
/// honest tri-state payload (`success: true` + `complete: bool` + named
/// gap fields per `crates/aft/src/protocol.rs`).
///
/// `diagnostics` only contains entries from servers that proved freshness.
/// `accepted_snapshots` preserves that same result per producer and document
/// version so alert consumers never infer a partition from flattened output.
/// Pre-edit cached entries and unversioned reports are NEVER included.
#[derive(Debug, Clone, Default)]
pub struct PostEditWaitOutcome {
    /// Complete, per-producer accepted snapshots. This includes a snapshot
    /// with an empty diagnostics list when a live server authoritatively found
    /// the edited document clean; pending, exited, warming, and unversioned
    /// producers do not appear here.
    pub accepted_snapshots: Vec<AcceptedDiagnosticSnapshot>,
    /// Authoritative diagnostics flattened only for the legacy response body.
    /// Consumers that preserve producer partitions must use
    /// `accepted_snapshots` instead.
    pub diagnostics: Vec<StoredDiagnostic>,
    /// Servers we expected to publish but didn't before the deadline.
    /// Reported to the agent via `pending_lsp_servers` so they understand
    /// the result is partial.
    pub pending_servers: Vec<ServerKey>,
    /// Pending producers proven silent by the client, not merely warming,
    /// unversioned, or unreachable because the manager was busy.
    pub unresponsive_servers: Vec<ServerKey>,
    /// Servers whose process exited between notification and deadline.
    /// Reported separately so the agent knows the gap is unrecoverable
    /// without a server restart, not "wait longer."
    pub exited_servers: Vec<ServerKey>,
}

/// Pre-edit freshness snapshot for one server/file pair.
#[derive(Debug, Clone, Copy, Default)]
pub struct PreEditSnapshot {
    pub epoch: u64,
    pub document_version_at_capture: Option<i32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleDiagnosticsMark {
    pub had_entries: bool,
    pub changed: bool,
}

pub fn post_edit_entry_is_fresh(
    entry: &DiagnosticEntry,
    target_version: i32,
    pre: PreEditSnapshot,
) -> bool {
    if entry.stale || entry.epoch <= pre.epoch {
        return false;
    }

    match entry.version {
        Some(version) => version >= target_version,
        // Unversioned publishDiagnostics payloads cannot prove which document
        // state they describe. Epoch advancement only proves arrival order; an
        // old analysis result can still arrive after our pre-snapshot. Treat as
        // pending/partial rather than fresh.
        None => false,
    }
}

impl PostEditWaitOutcome {
    /// True if every expected server reported a fresh result. False means
    /// the agent should treat the diagnostics as a partial picture.
    pub fn complete(&self) -> bool {
        self.pending_servers.is_empty() && self.exited_servers.is_empty()
    }
}

/// Per-server outcome of a `textDocument/diagnostic` (per-file pull) request.
#[derive(Debug, Clone)]
pub enum PullFileOutcome {
    /// Server returned a full report; diagnostics stored.
    Full { diagnostic_count: usize },
    /// Server returned `kind: "unchanged"` — cached diagnostics still valid.
    Unchanged,
    /// Server returned a partial-result token; we don't subscribe to streamed
    /// progress so the response is treated as a soft empty until the next pull.
    PartialNotSupported,
    /// Server doesn't advertise pull capability — caller should fall back to
    /// push diagnostics for this server.
    PullNotSupported,
    /// The pull request failed (timeout, server error, etc.).
    RequestFailed { reason: String },
}

/// A per-file `textDocument/diagnostic` request to one server. It is sent
/// with the language-server manager locked, then waited for with the lock
/// released: a server can take seconds to answer, and while the lock is held
/// every other request that needs the manager (a concurrent `read` among
/// them) waits too.
pub(crate) struct DocumentPull {
    key: ServerKey,
    canonical_path: PathBuf,
    document_version: Option<i32>,
    /// The pull itself opened the document in this server (it was not open
    /// for an edit or an earlier query).
    opened_for_pull: bool,
    state: DocumentPullState,
}

enum DocumentPullState {
    /// Settled without a reply to wait for (for example, pull unsupported).
    Done(PullFileOutcome),
    InFlight {
        request: crate::lsp::client::PendingLspRequest,
        /// When to stop waiting, fixed when the request is sent. An
        /// absolute time, so a caller that sends several pulls and then
        /// waits for them in turn never waits past the budget each one was
        /// sent with.
        wait_until: Instant,
    },
    Replied(Result<serde_json::Value, LspError>),
}

impl DocumentPull {
    /// Wait for the server's reply. Needs no manager access, so call it with
    /// the manager lock released; [`LspManager::finish_document_pull`] then
    /// stores the result.
    pub(crate) fn wait(mut self) -> Self {
        let state = std::mem::replace(
            &mut self.state,
            DocumentPullState::Done(PullFileOutcome::PullNotSupported),
        );
        self.state = match state {
            DocumentPullState::InFlight {
                request,
                wait_until,
            } => DocumentPullState::Replied(
                request.wait(wait_until.saturating_duration_since(Instant::now())),
            ),
            settled => settled,
        };
        self
    }
}

/// Pull a file's diagnostics from every server that supports it, holding the
/// language-server manager lock only to open the document and send the
/// requests, and again to store the replies; never while servers work.
/// `lock` acquires the manager (for example `|| ctx.lsp()`).
pub fn pull_file_diagnostics_unlocked<G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    config: &Config,
    timeout: Option<Duration>,
) -> Result<Vec<PullFileResult>, LspError>
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    pull_file_diagnostics_unlocked_inner(lock, file_path, config, timeout, false)
}

/// [`pull_file_diagnostics_unlocked`], storing each rust-analyzer report
/// together with the file's latest `cargo check` results. rust-analyzer
/// answers a pull with its own analysis only and pushes the compiler's errors
/// separately, so storing the pulled report alone drops them. Use this only
/// after waiting for a running check to finish (see
/// [`LspManager::rust_check_state`]); otherwise the stored compiler errors
/// can describe the files before an edit.
pub fn pull_file_diagnostics_with_cargo_check_unlocked<G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    config: &Config,
    timeout: Option<Duration>,
) -> Result<Vec<PullFileResult>, LspError>
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    pull_file_diagnostics_unlocked_inner(lock, file_path, config, timeout, true)
}

fn pull_file_diagnostics_unlocked_inner<G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    config: &Config,
    timeout: Option<Duration>,
    with_cargo_check: bool,
) -> Result<Vec<PullFileResult>, LspError>
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    // Start any missing server without the lock; the pulls below then only
    // open the document and send the requests under it.
    start_servers_for_file_unlocked(&lock, file_path, config);
    let pulls = lock().begin_file_pulls(file_path, config, deadline)?;
    // All requests are in flight at once; wait for each without the lock.
    let replies = pulls
        .into_iter()
        .map(DocumentPull::wait)
        .collect::<Vec<_>>();
    let mut lsp = lock();
    Ok(replies
        .into_iter()
        .map(|pull| {
            let server_key = pull.key.clone();
            let opened_for_pull = pull.opened_for_pull;
            let outcome = lsp.finish_document_pull(pull);
            // Read the pulled report under the same lock that stored it: a
            // push drained in between would replace it. Until a check has
            // run, nothing pushed holds compiler results worth keeping.
            if with_cargo_check
                && server_key.kind == ServerKind::Rust
                && lsp
                    .clients
                    .get(&server_key)
                    .is_some_and(LspClient::rust_check_seen)
                && matches!(
                    outcome,
                    PullFileOutcome::Full { .. } | PullFileOutcome::Unchanged
                )
            {
                if let Some(pulled) = lsp.server_file_diagnostics(&server_key, file_path) {
                    lsp.store_pull_push_union(&server_key, file_path, pulled);
                }
            }
            PullFileResult {
                server_key,
                outcome,
                opened_for_pull,
            }
        })
        .collect())
}

/// `workspace/diagnostic` for one server with the manager lock released
/// while the server works (see [`pull_file_diagnostics_unlocked`]).
pub fn pull_workspace_diagnostics_unlocked<G>(
    lock: impl Fn() -> G,
    server_key: &ServerKey,
    timeout: Option<Duration>,
) -> Result<PullWorkspaceResult, LspError>
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    let timeout = timeout.unwrap_or(LspManager::PULL_WORKSPACE_TIMEOUT);
    let Some(request) = lock().begin_workspace_pull(server_key)? else {
        return Ok(unsupported_workspace_pull(server_key));
    };
    let reply = request.wait(timeout);
    lock().finish_workspace_pull(server_key, reply)
}

/// Send an interactive request (hover, definition, references, rename) to the
/// running server that owns `file_path` and wait for its reply.
///
/// The manager lock is held only to find the client and write the request.
/// The wait for the reply, up to eight seconds on a busy server, runs without
/// it: holding the lock there made every other manager user (the request
/// loop rendering the status bar for a sibling `read`, an edit's `didChange`)
/// wait for the slow server too. `lock` acquires the manager (for example
/// `|| ctx.lsp()`). `None` when no running client owns the file.
pub fn send_file_request_unlocked<R, G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    config: &Config,
    params: R::Params,
) -> Option<Result<R::Result, LspError>>
where
    R: lsp_types::request::Request,
    R::Params: serde::Serialize,
    R::Result: serde::de::DeserializeOwned,
    G: std::ops::DerefMut<Target = LspManager>,
{
    let started = {
        let mut lsp = lock();
        let client = lsp.client_for_file_mut(file_path, config)?;
        client.start_request::<R>(params)
    };
    Some(
        started
            .and_then(|request| request.wait(crate::lsp::client::INTERACTIVE_REQUEST_TIMEOUT))
            .and_then(|value| serde_json::from_value(value).map_err(Into::into)),
    )
}

/// `workspace/diagnostic` for several servers at once: every request is sent
/// under one lock, all replies are awaited together without it, and stored
/// under one lock. Pulling the servers one after another made a directory
/// query wait for the sum of their replies (up to ten seconds each) instead
/// of the slowest one. Results are in the order of `server_keys`.
pub fn pull_workspace_diagnostics_many_unlocked<G>(
    lock: impl Fn() -> G,
    server_keys: &[ServerKey],
    timeout: Option<Duration>,
) -> Vec<Result<PullWorkspaceResult, LspError>>
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    let timeout = timeout.unwrap_or(LspManager::PULL_WORKSPACE_TIMEOUT);
    let started: Vec<_> = {
        let mut lsp = lock();
        server_keys
            .iter()
            .map(|key| lsp.begin_workspace_pull(key))
            .collect()
    };
    // Every request was sent just now, so each still gets its full budget.
    let deadline = Instant::now() + timeout;
    let replies: Vec<_> = started
        .into_iter()
        .map(|started| {
            started.map(|request| {
                request
                    .map(|request| request.wait(deadline.saturating_duration_since(Instant::now())))
            })
        })
        .collect();
    let mut lsp = lock();
    server_keys
        .iter()
        .zip(replies)
        .map(|(key, reply)| match reply {
            Err(error) => Err(error),
            Ok(None) => Ok(unsupported_workspace_pull(key)),
            Ok(Some(reply)) => lsp.finish_workspace_pull(key, reply),
        })
        .collect()
}

/// The diagnostics of both lists, each identical diagnostic once.
fn union_of_diagnostics(
    mut first: Vec<StoredDiagnostic>,
    second: Vec<StoredDiagnostic>,
) -> Vec<StoredDiagnostic> {
    for diagnostic in second {
        if !first.contains(&diagnostic) {
            first.push(diagnostic);
        }
    }
    first
}

fn unsupported_workspace_pull(server_key: &ServerKey) -> PullWorkspaceResult {
    PullWorkspaceResult {
        server_key: server_key.clone(),
        files_reported: Vec::new(),
        complete: false,
        cancelled: false,
        supports_workspace: false,
    }
}

/// Result of ensuring a document is open in every matching server.
#[derive(Debug, Clone, Default)]
pub struct EnsureFileOpenResult {
    pub server_keys: Vec<ServerKey>,
    /// Servers that received `textDocument/didOpen` during this call.
    pub newly_opened: Vec<ServerKey>,
}

impl EnsureFileOpenResult {
    pub fn is_empty(&self) -> bool {
        self.server_keys.is_empty()
    }
}

/// Result of `pull_file_diagnostics` for one matching server.
#[derive(Debug, Clone)]
pub struct PullFileResult {
    pub server_key: ServerKey,
    pub outcome: PullFileOutcome,
    /// The pull opened the document in this server just to ask about it.
    /// The caller closes it once done (see
    /// [`LspManager::close_documents_opened_for_pulls`]), so documents
    /// nobody edits do not stay open in the server for the rest of the
    /// session.
    pub opened_for_pull: bool,
}

/// Result of `pull_workspace_diagnostics` for a single server.
#[derive(Debug, Clone)]
pub struct PullWorkspaceResult {
    pub server_key: ServerKey,
    /// Files for which a Full report was received and cached. Files that came
    /// back as `Unchanged` are NOT listed here because their cached entry was
    /// already authoritative.
    pub files_reported: Vec<PathBuf>,
    /// True if the server returned a full response within the timeout.
    pub complete: bool,
    /// True if we cancelled (request timed out before the server responded).
    pub cancelled: bool,
    /// True if the server advertised workspace pull support. When false, the
    /// other fields are empty and the caller should fall back to file-mode
    /// pull or to push semantics.
    pub supports_workspace: bool,
}

pub struct DrainedLspEvents {
    pub events: Vec<LspEvent>,
    pub diagnostics_changed: bool,
    /// Complete snapshots accepted from document-version-verified publishes
    /// emitted by already-live, quiescent producers. Runtime consumers must
    /// retain each producer's snapshot until they construct an
    /// `AcceptedObservation` that records those producer-specific results;
    /// flattening the snapshots would erase producer ownership.
    pub accepted_snapshots: Vec<AcceptedDiagnosticSnapshot>,
    pub has_more: bool,
}

/// State carried by an `AppContext` post-edit wait after it releases the
/// manager mutex. The raw receiver clone competes for each event exactly once;
/// the dedicated wake receiver covers the case where another drain path wins
/// that race and updates the manager before this waiter sees the raw event.
pub(crate) struct PostEditDiagnosticsWait {
    lookup_path: PathBuf,
    expected_versions: Vec<(ServerKey, i32)>,
    pre_snapshot: HashMap<ServerKey, PreEditSnapshot>,
    responses_at_start: HashMap<ServerKey, u64>,
    event_rx: Receiver<LspEvent>,
    wake_rx: Receiver<()>,
    waiter_id: u64,
    deadline: std::time::Instant,
    fresh: HashMap<ServerKey, Vec<StoredDiagnostic>>,
    exited: Vec<ServerKey>,
}

impl PostEditDiagnosticsWait {
    pub(crate) fn deadline_reached(&self) -> bool {
        std::time::Instant::now() >= self.deadline
    }

    pub(crate) fn next_event(&self) -> Option<LspEvent> {
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }

        crossbeam_channel::select! {
            recv(self.event_rx) -> event => event.ok(),
            recv(self.wake_rx) -> _ => None,
            default(remaining) => None,
        }
    }
}

/// A subscription for a caller that waits for language-server events with
/// the manager lock released (see [`LspManager::subscribe_events`]). It
/// returns when an event arrives or another drain path handled one, so the
/// caller re-checks at once instead of sleeping a fixed interval.
pub(crate) struct LspEventWait {
    event_rx: Receiver<LspEvent>,
    wake_rx: Receiver<()>,
    waiter_id: u64,
}

impl LspEventWait {
    /// Block until an event arrives, another drain handles one, or `until`.
    /// A returned event must be passed to
    /// [`LspManager::handle_waited_event`].
    pub(crate) fn next_event(&self, until: Instant) -> Option<LspEvent> {
        let remaining = until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        crossbeam_channel::select! {
            recv(self.event_rx) -> event => event.ok(),
            recv(self.wake_rx) -> _ => None,
            default(remaining) => None,
        }
    }

    /// Forget a wake raised by drains up to now. Call it with the manager
    /// locked, right after checking the state: every drain so far is covered
    /// by that check (the waiter's own handling of an event wakes it too),
    /// and a drain after it can only happen once the lock is released, so
    /// its wake is not lost.
    pub(crate) fn clear_pending_wake(&self) {
        let _ = self.wake_rx.try_recv();
    }
}

impl IntoIterator for DrainedLspEvents {
    type Item = LspEvent;
    type IntoIter = std::vec::IntoIter<LspEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.into_iter()
    }
}

/// Result of one bounded all-client shutdown. The same counts are emitted in
/// the `lsp shutdown_all` summary log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LspShutdownAllOutcome {
    pub graceful: usize,
    pub forced: usize,
    /// Forcibly killed servers whose exit was not observed before the budget
    /// ran out. Their process groups were sent a kill that cannot be ignored;
    /// if this process exits first, the system reaps them.
    pub unreaped: usize,
    pub elapsed: Duration,
}

pub struct LspManager {
    /// Active server instances, keyed by (ServerKind, workspace_root).
    clients: HashMap<ServerKey, LspClient>,
    /// Binary names for active server instances. Kept separate from
    /// `LspClient` so crash handling can report the installable binary name
    /// after a post-initialize process exit.
    server_binaries: HashMap<ServerKey, String>,
    /// Tracks opened documents and versions per active server.
    documents: HashMap<ServerKey, DocumentStore>,
    /// Stored publishDiagnostics payloads across all servers.
    diagnostics: DiagnosticsStore,
    /// Unified event channel — all server reader threads send here.
    event_tx: Sender<LspEvent>,
    event_rx: Receiver<LspEvent>,
    /// One-slot wake channels for post-edit waits that released the manager
    /// mutex. Drains notify them after handling an event, so a waiter cannot
    /// sleep through a diagnostics update consumed by another drain path.
    post_edit_waiters: HashMap<u64, Sender<()>>,
    next_post_edit_waiter_id: u64,
    /// Optional binary path overrides used by integration tests.
    binary_overrides: HashMap<ServerKind, PathBuf>,
    /// Last plugin-pushed LSP search paths. `None` keeps direct `LspManager`
    /// users compatible with the paths supplied on their `Config` argument.
    pushed_search_paths: Option<Vec<PathBuf>>,
    /// Extra env vars merged into every spawned LSP child. Used in tests to
    /// drive the fake server's behavioral variants (`AFT_FAKE_LSP_PULL=1`,
    /// `AFT_FAKE_LSP_WORKSPACE=1`, etc.). Production code does not set this.
    extra_env: HashMap<String, String>,
    /// Per-(kind,root) cache of start failures. Once a server fails to start
    /// for a workspace root, later requests report the remembered failure
    /// instead of starting it again. Without this, every file open or
    /// didChange retries `spawn_server` and logs a fresh ERROR — visible as
    /// repeated `failed to spawn TypeScript Language Server: Could not find a
    /// valid TypeScript installation` lines per edit.
    ///
    /// A permanent failure (binary not installed, a configuration error the
    /// server names) is replayed until the search paths or the configuration
    /// change; see [`Self::clear_failed_spawns`]. A transient one (a handshake
    /// past its time budget, an unexplained crash) is replayed only until its
    /// backoff window passes; the next request then starts the server again.
    /// A server that is slow to start once, such as Eclipse JDTLS indexing a
    /// large repository, would otherwise stay unavailable until AFT restarts.
    failed_spawns: HashMap<ServerKey, FailedSpawn>,
    /// The backoff that followed the latest transient start failure of each
    /// server and root. Kept across retries so repeated failures wait longer;
    /// removed when the server starts or fails permanently.
    transient_backoff: HashMap<ServerKey, Duration>,
    /// Server/root pairs for which we already logged that watched-file
    /// notifications are skipped because the capability is absent.
    watched_file_skip_logged: HashSet<ServerKey>,
    /// rust-analyzer instances with a workspace reload running on a helper
    /// thread because the file watcher saw a Cargo manifest change (see
    /// [`spawn_watcher_rust_workspace_reload`]). The value collects the
    /// manifests of further changes seen while that reload runs, so the
    /// thread checks them once more instead of a second thread starting.
    watcher_rust_reloads: HashMap<ServerKey, Option<Vec<PathBuf>>>,
    /// The last watched-file routing decision, retained on Windows so a CI
    /// timeout can distinguish a skipped send from a delayed fake-server reply.
    #[cfg(windows)]
    last_watched_file_notification_trace: String,
    /// Tracks PIDs of spawned LSP child processes so the signal handler can
    /// kill them on SIGTERM/SIGINT before aft exits, preventing orphans.
    /// Defaults to empty; production wires this from `AppContext`.
    child_registry: LspChildRegistry,
    /// The last few server-exit log lines, newest last, so tests and status
    /// probes can read why a server stopped without installing a logger.
    recent_exit_log_lines: std::collections::VecDeque<String>,
    /// Exit reports for servers that died before a client owned them (during
    /// spawn or `initialize`) or whose failure was already reported, keyed by
    /// PID. Each waits for its reader's `ServerExited` event so the exit is
    /// logged exactly once, with full detail.
    pending_exit_reports: HashMap<u32, ServerExitReport>,
    /// Servers being started by [`start_applicable_server_unlocked`] without
    /// the manager lock, keyed by server.
    starting: HashMap<ServerKey, StartReservation>,
    /// Advanced whenever every client is taken away, so a start that was in
    /// flight across it does not publish its client into the emptied manager.
    clients_generation: u64,
    /// Documents a scoped inspect opened only to collect their diagnostics
    /// and then closed. Servers answer `didClose` with an empty publish that
    /// clears the closed document (TypeScript means "no longer tracked").
    /// Storing it would replace the diagnostics the inspect just collected
    /// with a clean-looking report, so empty publishes for these documents
    /// are ignored until a non-empty publish arrives or AFT opens or edits
    /// the file again (see `is_inspect_close_clearing`).
    ///
    /// Each entry carries the order it was added in. Past
    /// [`INSPECT_CLOSED_DOCUMENTS_CAP`] the oldest are forgotten: a server
    /// answers a close within moments, so an old entry no longer guards
    /// anything, and without the cap a long session inspecting many files
    /// kept one entry per file for good.
    inspect_closed_documents: HashMap<(ServerKey, PathBuf), u64>,
    inspect_close_sequence: u64,
    /// The latest pushed diagnostics per file from rust-analyzer instances
    /// that also answer pull requests. In that mode rust-analyzer reports its
    /// own analysis through pull and its `cargo check` results through push,
    /// and both land in the same store entry, so whichever arrives last would
    /// hide the other. A scoped inspect stores the union of the two. Other
    /// servers that support both report one analysis either way, so for them
    /// the pulled report stands alone as before.
    latest_push_for_pull_servers: HashMap<(ServerKey, PathBuf), Vec<StoredDiagnostic>>,
    /// Pulled diagnostics a scoped inspect recorded per rust-analyzer file,
    /// kept so a later `cargo check` push for the file is stored beside them
    /// instead of replacing them. Forgotten once AFT opens or edits the file
    /// or the watcher sees it change, because they then describe an older
    /// state of the file.
    latest_pull_for_rust: HashMap<(ServerKey, PathBuf), Vec<StoredDiagnostic>>,
    /// Times the retry window of transient start failures in `failed_spawns`.
    retry_clock: RetryClock,
}
#[cfg(test)]
thread_local! {
    /// Typed parses of `publishDiagnostics` payloads on this thread.
    static PUBLISH_DIAGNOSTICS_PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many documents closed by a scoped inspect `LspManager` remembers (see
/// `LspManager::inspect_closed_documents`).
const INSPECT_CLOSED_DOCUMENTS_CAP: usize = 4_096;

/// How many server-exit log lines `LspManager` keeps for inspection.
const RECENT_EXIT_LOG_LINES: usize = 16;
/// Upper bound on exit reports waiting for their reader event. Every spawned
/// reader sends one event, so this only guards against an unforeseen leak.
const PENDING_EXIT_REPORTS_CAP: usize = 64;
/// How long a failed `initialize` waits for the server process to finish
/// exiting, so the error can carry its real exit status.
const INITIALIZE_EXIT_WAIT: Duration = Duration::from_millis(250);
/// Longest failure reason, in bytes, that inspect shows when a language
/// server (a diagnostics producer) could not start.
const PRODUCER_FAILURE_REASON_BYTES: usize = 500;

/// Monotonic time source that decides when a server whose start failed for
/// a transient reason may be started again. Production reads
/// `Instant::now()`; tests substitute a clock they can move forward so a
/// retry window can pass without the test sleeping through it.
#[derive(Clone)]
pub struct RetryClock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl RetryClock {
    /// The real monotonic clock.
    pub fn system() -> Self {
        Self(Arc::new(Instant::now))
    }

    /// A clock that reports whatever `now` returns.
    pub fn from_fn(now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub fn now(&self) -> Instant {
        (self.0)()
    }
}

impl Default for RetryClock {
    fn default() -> Self {
        Self::system()
    }
}

/// How long a server whose start failed for a transient reason waits before
/// the next request may start it again. Each further transient failure of the
/// same server and root doubles the wait, up to
/// [`TRANSIENT_RETRY_MAX_BACKOFF`]; a successful start resets it.
const TRANSIENT_RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(30);
/// The longest wait between attempts to start a server whose start keeps
/// failing for transient reasons.
const TRANSIENT_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Whether a failed server start can succeed later without anything in the
/// environment changing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureDurability {
    /// Only a change to the environment fixes it: a binary that is missing or
    /// cannot be executed, a server that exited naming a configuration error
    /// AFT recognises, or a server AFT's own checks refused. Replayed until
    /// the search paths or the configuration change.
    Permanent,
    /// May pass on a later attempt: a handshake that ran past its time
    /// budget (the server is then killed), a server that died without naming
    /// a recognised configuration error, or an I/O error on its pipes.
    /// Retried once a backoff window has passed.
    Transient,
}

/// A server start that produced no client.
#[derive(Debug)]
struct StartFailure {
    error: LspError,
    durability: FailureDurability,
}

/// A failed start remembered for one server and workspace root.
#[derive(Clone, Debug)]
struct FailedSpawn {
    result: ServerAttemptResult,
    /// When a transient failure may be retried; `None` for a permanent one.
    retry_at: Option<Instant>,
}

impl FailedSpawn {
    /// The result to report for this failure at `now`. A transient failure
    /// says so, and says when the next start attempt will be made, so it does
    /// not read as "this server cannot run here". The note leads the reason
    /// because inspect shows only the reason's first line, cut to a length.
    fn reported_result(&self, now: Instant) -> ServerAttemptResult {
        let (Some(retry_at), ServerAttemptResult::SpawnFailed { binary, reason }) =
            (self.retry_at, &self.result)
        else {
            return self.result.clone();
        };
        let remaining = retry_at.saturating_duration_since(now);
        let when = if remaining.is_zero() {
            "retrying now".to_string()
        } else {
            format!("will retry after {}", format_retry_delay(remaining))
        };
        ServerAttemptResult::SpawnFailed {
            binary: binary.clone(),
            reason: format!("transient failure, {when}: {reason}"),
        }
    }
}

/// The backoff that follows a transient failure, given the one that followed
/// the previous transient failure of the same server and root, if any.
fn next_transient_backoff(previous: Option<Duration>) -> Duration {
    previous.map_or(TRANSIENT_RETRY_INITIAL_BACKOFF, |previous| {
        previous.saturating_mul(2).min(TRANSIENT_RETRY_MAX_BACKOFF)
    })
}

/// A retry delay in whole seconds, rounded up so a pending retry never reads
/// as "after 0s": `45s`, `2m`, `4m 30s`.
fn format_retry_delay(delay: Duration) -> String {
    let seconds = delay.as_secs() + u64::from(delay.subsec_nanos() > 0);
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, seconds) => format!("{minutes}m {seconds}s"),
    }
}

impl LspManager {
    pub fn new() -> Self {
        let (event_tx, event_rx) = unbounded();
        Self {
            clients: HashMap::new(),
            server_binaries: HashMap::new(),
            documents: HashMap::new(),
            diagnostics: DiagnosticsStore::new(),
            event_tx,
            event_rx,
            post_edit_waiters: HashMap::new(),
            next_post_edit_waiter_id: 0,
            binary_overrides: HashMap::new(),
            pushed_search_paths: None,
            extra_env: HashMap::new(),
            failed_spawns: HashMap::new(),
            transient_backoff: HashMap::new(),
            watched_file_skip_logged: HashSet::new(),
            watcher_rust_reloads: HashMap::new(),
            #[cfg(windows)]
            last_watched_file_notification_trace: "no watched-file notification attempted"
                .to_string(),
            child_registry: LspChildRegistry::new(),
            recent_exit_log_lines: std::collections::VecDeque::new(),
            pending_exit_reports: HashMap::new(),
            starting: HashMap::new(),
            clients_generation: 0,
            inspect_closed_documents: HashMap::new(),
            inspect_close_sequence: 0,
            latest_push_for_pull_servers: HashMap::new(),
            latest_pull_for_rust: HashMap::new(),
            retry_clock: RetryClock::system(),
        }
    }

    /// For testing: replace the clock that times retries of servers whose
    /// start failed for a transient reason.
    #[doc(hidden)]
    pub fn set_retry_clock(&mut self, clock: RetryClock) {
        self.retry_clock = clock;
    }

    /// Remember a failed start of `key` and return the result to report for
    /// it now. A transient failure opens a backoff window during which later
    /// requests replay it instead of starting the server.
    fn record_failed_spawn(
        &mut self,
        key: &ServerKey,
        result: ServerAttemptResult,
        durability: FailureDurability,
    ) -> ServerAttemptResult {
        let now = self.retry_clock.now();
        let retry_at = match durability {
            FailureDurability::Permanent => {
                self.transient_backoff.remove(key);
                None
            }
            FailureDurability::Transient => {
                let backoff = next_transient_backoff(self.transient_backoff.get(key).copied());
                self.transient_backoff.insert(key.clone(), backoff);
                slog_info!(
                    "lsp start failed transiently server={} root={} retry_after={}",
                    key.kind.id_str(),
                    key.root.display(),
                    format_retry_delay(backoff)
                );
                Some(now + backoff)
            }
        };
        let failed = FailedSpawn { result, retry_at };
        let reported = failed.reported_result(now);
        self.failed_spawns.insert(key.clone(), failed);
        reported
    }

    /// The remembered failure to report instead of starting `key`, or `None`
    /// when the server should be started: nothing failed, or a transient
    /// failure's backoff window has passed.
    fn failure_to_replay(&self, key: &ServerKey) -> Option<ServerAttemptResult> {
        let failed = self.failed_spawns.get(key)?;
        let now = self.retry_clock.now();
        if failed.retry_at.is_some_and(|retry_at| now >= retry_at) {
            return None;
        }
        Some(failed.reported_result(now))
    }

    /// Log that a start about to run retries a transient failure.
    fn log_transient_retry(&self, key: &ServerKey) {
        if self.failed_spawns.contains_key(key) {
            slog_info!(
                "lsp start retry server={} root={} reason=transient failure backoff elapsed; retrying now",
                key.kind.id_str(),
                key.root.display()
            );
        }
    }

    /// Forget the failure history of a server that has now started.
    fn note_start_succeeded(&mut self, key: &ServerKey) {
        self.failed_spawns.remove(key);
        self.transient_backoff.remove(key);
    }

    /// The most recent server-exit log lines, oldest first.
    #[doc(hidden)]
    pub fn recent_server_exit_log_lines(&self) -> Vec<String> {
        self.recent_exit_log_lines.iter().cloned().collect()
    }

    fn record_exit_log_line(&mut self, line: String) {
        slog_info!("{line}");
        if self.recent_exit_log_lines.len() == RECENT_EXIT_LOG_LINES {
            self.recent_exit_log_lines.pop_front();
        }
        self.recent_exit_log_lines.push_back(line);
    }

    /// Set the child-PID registry. Must be called before any servers spawn.
    pub fn set_child_registry(&mut self, registry: LspChildRegistry) {
        self.child_registry = registry;
    }

    /// For testing: set an extra environment variable that gets passed to
    /// every spawned LSP child process. Useful for driving fake-server
    /// behavioral variants in integration tests.
    pub fn set_extra_env(&mut self, key: &str, value: &str) {
        self.extra_env.insert(key.to_string(), value.to_string());
    }

    /// Count active LSP server instances.
    pub fn server_count(&self) -> usize {
        self.clients.len()
    }

    /// Estimate the per-server document metadata and diagnostics retained by
    /// the manager. LSP child-process memory is outside this process RSS and is
    /// not included.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let mut bytes = 0u64;
        let mut document_count = 0u64;
        for documents in self.documents.values() {
            let estimate = documents.estimated_memory();
            bytes = bytes.saturating_add(estimate.estimated_bytes.unwrap_or(0));
            document_count = document_count
                .saturating_add(estimate.counts.get("documents").copied().unwrap_or(0));
        }
        let diagnostics = self.diagnostics.estimated_memory();
        bytes = bytes.saturating_add(diagnostics.estimated_bytes.unwrap_or(0));
        crate::memory::MemoryEstimate::estimated(bytes)
            .count("servers", self.clients.len())
            .count("document_stores", self.documents.len())
            .count_u64("documents", document_count)
            .count_u64(
                "diagnostic_entries",
                diagnostics
                    .counts
                    .get("diagnostic_entries")
                    .copied()
                    .unwrap_or(0),
            )
            .count_u64(
                "diagnostics",
                diagnostics.counts.get("diagnostics").copied().unwrap_or(0),
            )
    }

    /// Apply the configured diagnostic LRU cap (the `lsp.diagnostic_cache_size`
    /// knob). 0 disables the cap. Called at construction so the documented
    /// config field actually takes effect instead of always using the default.
    pub fn set_diagnostic_capacity(&mut self, capacity: usize) {
        self.diagnostics.set_capacity(capacity);
    }

    /// For testing: override the binary for a server kind.
    pub fn override_binary(&mut self, kind: ServerKind, binary_path: PathBuf) {
        self.binary_overrides.insert(kind, binary_path);
    }

    /// Replace the plugin-managed binary search paths used by future lazy starts.
    /// Existing servers stay live; failed starts are forgotten when the paths
    /// change so a newly installed binary is retried without restarting AFT.
    pub fn set_search_paths(&mut self, paths: Vec<PathBuf>) -> bool {
        if self.pushed_search_paths.as_ref() == Some(&paths) {
            return false;
        }
        self.pushed_search_paths = Some(paths);
        self.clear_failed_spawns();
        true
    }

    /// Resolve every configured server applicable to a root without starting it.
    pub fn resolve_applicable_servers_for_root(
        &self,
        project_root: &Path,
        config: &Config,
    ) -> Result<ApplicableServerSnapshot, ApplicabilityResolutionError> {
        let walk = walk_applicable_area(project_root, None, config, None)?;
        Ok(self.classify_applicable_servers(walk, config))
    }

    /// Turn a walk into the applicability snapshot: each walked server key is
    /// either a candidate to start or a producer failure (a cached spawn
    /// failure or a binary that does not resolve). Only this step reads
    /// manager state, and it visits each server key once, so it is short
    /// enough to run under the manager lock.
    pub fn classify_applicable_servers(
        &self,
        walk: ApplicabilityWalk,
        config: &Config,
    ) -> ApplicableServerSnapshot {
        let mut candidates = Vec::new();
        let mut producer_failures = Vec::new();
        for candidate in walk.candidates {
            if let Some(result) = self.failure_to_replay(&candidate.key) {
                producer_failures.push(ApplicableServerFailure {
                    server_key: candidate.key,
                    result,
                });
                continue;
            }
            if self
                .resolve_binary(&candidate.definition, &candidate.key.root, config)
                .is_err()
            {
                producer_failures.push(ApplicableServerFailure {
                    server_key: candidate.key,
                    result: ServerAttemptResult::BinaryNotInstalled {
                        binary: candidate.definition.binary.clone(),
                    },
                });
                continue;
            }
            candidates.push(candidate);
        }

        let mut selected_kinds = candidates
            .iter()
            .map(|candidate| candidate.key.kind.clone())
            .chain(
                producer_failures
                    .iter()
                    .map(|failure| failure.server_key.kind.clone()),
            )
            .collect::<HashSet<_>>();
        // The `typescript` server's marker (package.json, tsconfig.json) is
        // served when its files went to the native TypeScript server instead;
        // naming `typescript` as not applicable there would be wrong.
        if selected_kinds
            .iter()
            .any(|kind| matches!(kind, ServerKind::TypeScriptNative(_)))
        {
            selected_kinds.insert(ServerKind::TypeScript);
        }
        // Only a server that could actually have run is worth naming: one whose
        // binary resolves. Markers of servers the user does not have installed
        // (Astro and Prisma also use `package.json`) would otherwise add a
        // not-applicable line for tools nobody expects.
        let mut not_applicable = walk
            .markers
            .into_values()
            .filter(|(definition, _, _)| !selected_kinds.contains(&definition.kind))
            .filter(|(definition, marker_dir, _)| {
                self.resolve_binary(definition, marker_dir, config).is_ok()
            })
            .map(|(definition, _, marker)| NotApplicableServer {
                server_id: definition.kind.id_str().to_string(),
                marker,
                extensions: definition.extensions.clone(),
            })
            .collect::<Vec<_>>();
        not_applicable.sort_by(|left, right| left.server_id.cmp(&right.server_id));

        candidates.sort_by(|left, right| server_key_sort(&left.key, &right.key));
        producer_failures
            .sort_by(|left, right| server_key_sort(&left.server_key, &right.server_key));
        let mut server_keys = candidates
            .iter()
            .map(|candidate| candidate.key.clone())
            .chain(
                producer_failures
                    .iter()
                    .map(|failure| failure.server_key.clone()),
            )
            .collect::<Vec<_>>();
        server_keys.sort_by(server_key_sort);
        ApplicableServerSnapshot {
            server_keys,
            not_applicable,
            candidates,
            producer_failures,
        }
    }

    /// Start exactly the servers selected by a prior applicability snapshot.
    ///
    /// No document is opened here. That boundary makes a late configuration or
    /// filesystem change apply to the next inspection rather than starting an
    /// unrecorded server in the current call.
    pub fn start_applicable_servers(
        &mut self,
        snapshot: &ApplicableServerSnapshot,
        config: &Config,
    ) -> ApplicableServerStartOutcomes {
        self.start_applicable_servers_inner(snapshot, config, None)
    }

    fn start_applicable_servers_inner(
        &mut self,
        snapshot: &ApplicableServerSnapshot,
        config: &Config,
        deadline: Option<std::time::Instant>,
    ) -> ApplicableServerStartOutcomes {
        let mut outcomes = ApplicableServerStartOutcomes {
            failures: snapshot.producer_failures.clone(),
            ..ApplicableServerStartOutcomes::default()
        };
        for candidate in &snapshot.candidates {
            let initialize_timeout = deadline
                .map(|deadline| deadline.saturating_duration_since(std::time::Instant::now()));
            if initialize_timeout.is_some_and(|remaining| remaining.is_zero()) {
                outcomes.deadline_exceeded = Some(candidate.key.clone());
                break;
            }
            if self.clients.contains_key(&candidate.key) {
                outcomes.successful.push(candidate.key.clone());
                continue;
            }
            self.log_transient_retry(&candidate.key);
            match self.spawn_server_with_timeout(
                &candidate.definition,
                &candidate.key.root,
                &candidate.source_file,
                config,
                initialize_timeout,
            ) {
                Ok(client) => {
                    self.note_start_succeeded(&candidate.key);
                    self.clients.insert(candidate.key.clone(), client);
                    self.server_binaries
                        .insert(candidate.key.clone(), candidate.definition.binary.clone());
                    self.documents.entry(candidate.key.clone()).or_default();
                    outcomes.successful.push(candidate.key.clone());
                }
                Err(failure) => {
                    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                        // A request-owned timeout says nothing durable about the
                        // producer; a later call with a fresh budget may retry it.
                        outcomes.deadline_exceeded = Some(candidate.key.clone());
                        break;
                    }
                    let result = classify_spawn_error(&candidate.definition.binary, &failure.error);
                    let result =
                        self.record_failed_spawn(&candidate.key, result, failure.durability);
                    outcomes.failures.push(ApplicableServerFailure {
                        server_key: candidate.key.clone(),
                        result,
                    });
                }
            }
        }
        outcomes
    }

    /// First step of [`start_applicable_server_unlocked`], under the lock.
    /// Returns `None` once `outcomes` holds this server's result; otherwise
    /// says whether to wait for another thread's start or spawn unlocked. A
    /// start that follows a wait reports the other thread's cached failure
    /// instead of retrying it.
    fn begin_unlocked_start(
        &mut self,
        candidate: &ApplicableServerCandidate,
        config: &Config,
        after_wait: bool,
        deadline: Instant,
        outcomes: &mut ApplicableServerStartOutcomes,
    ) -> Option<StartNext> {
        let key = &candidate.key;
        if self.clients.contains_key(key) {
            outcomes.successful.push(key.clone());
            return None;
        }
        if let Some(reservation) = self.starting.get(key) {
            return Some(StartNext::Wait(Arc::clone(&reservation.signal)));
        }
        if after_wait {
            if let Some(result) = self.failure_to_replay(key) {
                outcomes.failures.push(ApplicableServerFailure {
                    server_key: key.clone(),
                    result,
                });
                return None;
            }
        }
        match self.prepare_spawn(
            &candidate.definition,
            &key.root,
            &candidate.source_file,
            config,
        ) {
            Ok(prepared) => {
                self.log_transient_retry(key);
                self.starting.insert(
                    key.clone(),
                    StartReservation {
                        deferred_events: Vec::new(),
                        clients_generation: self.clients_generation,
                        signal: Arc::new(StartSignal::default()),
                    },
                );
                Some(StartNext::Spawn(prepared))
            }
            Err(error) => {
                self.record_start_error(candidate, &prepare_failure(error), deadline, outcomes);
                None
            }
        }
    }

    /// Last step of [`start_applicable_server_unlocked`], under the lock:
    /// publish the client or record the failure, then release the reservation.
    fn finish_unlocked_start(
        &mut self,
        candidate: &ApplicableServerCandidate,
        result: Result<LspClient, SpawnFailure>,
        deadline: Instant,
        outcomes: &mut ApplicableServerStartOutcomes,
    ) {
        let key = &candidate.key;
        let started_generation = self
            .starting
            .get(key)
            .map(|reservation| reservation.clients_generation);
        match result {
            // A start through another path (an edit's diagnostics, say) won
            // the race; keep that client and let this one's drop stop its
            // process.
            Ok(_) if self.clients.contains_key(key) => {
                slog_info!(
                    "lsp start discarded server={} root={} reason=duplicate start; stopping redundant client",
                    key.kind.id_str(),
                    key.root.display()
                );
                outcomes.successful.push(key.clone());
            }
            // Every client was taken away while this one was starting
            // (shutdown, idle reap, unbind); do not revive the manager.
            Ok(_) if started_generation != Some(self.clients_generation) => {
                slog_info!(
                    "lsp start discarded server={} root={} reason=shutdown during initialize; stopping client",
                    key.kind.id_str(),
                    key.root.display()
                );
                outcomes.failures.push(ApplicableServerFailure {
                    server_key: key.clone(),
                    result: ServerAttemptResult::SpawnFailed {
                        binary: candidate.definition.binary.clone(),
                        reason: "language servers were shut down while this one was starting"
                            .to_string(),
                    },
                });
            }
            Ok(client) => {
                self.note_start_succeeded(key);
                self.clients.insert(key.clone(), client);
                self.server_binaries
                    .insert(key.clone(), candidate.definition.binary.clone());
                self.documents.entry(key.clone()).or_default();
                outcomes.successful.push(key.clone());
            }
            Err(failure) => {
                let failure = self.absorb_spawn_failure(failure);
                if self.clients.contains_key(key) {
                    outcomes.successful.push(key.clone());
                } else {
                    self.record_start_error(candidate, &failure, deadline, outcomes);
                }
            }
        }
        self.release_start_reservation(key);
    }

    /// Drop a start reservation, handle the events held back for it, and wake
    /// any start waiting on it.
    fn release_start_reservation(&mut self, key: &ServerKey) {
        let Some(reservation) = self.starting.remove(key) else {
            return;
        };
        for event in &reservation.deferred_events {
            self.handle_event(event);
        }
        reservation.signal.finish();
    }

    /// Report a start that produced no client, as the locked start path does:
    /// a failure caused by the request deadline is not cached, any other one
    /// is, so later file events skip the server.
    fn record_start_error(
        &mut self,
        candidate: &ApplicableServerCandidate,
        failure: &StartFailure,
        deadline: Instant,
        outcomes: &mut ApplicableServerStartOutcomes,
    ) {
        if Instant::now() >= deadline {
            outcomes.deadline_exceeded = Some(candidate.key.clone());
            return;
        }
        let result = classify_spawn_error(&candidate.definition.binary, &failure.error);
        let result = self.record_failed_spawn(&candidate.key, result, failure.durability);
        outcomes.failures.push(ApplicableServerFailure {
            server_key: candidate.key.clone(),
            result,
        });
    }

    /// First step of [`ensure_server_for_file_detailed_unlocked`] for one
    /// server, under the lock. Mirrors the checks of
    /// [`Self::ensure_server_for_file_detailed`] (running client, remembered
    /// failure, orphan reap) and reserves the server when it has to start.
    fn begin_file_server_start(
        &mut self,
        def: &ServerDef,
        key: &ServerKey,
        file_path: &Path,
        config: &Config,
    ) -> FileServerStart {
        if self.clients.contains_key(key) {
            return FileServerStart::Running;
        }
        if let Some(reservation) = self.starting.get(key) {
            return FileServerStart::Wait(Arc::clone(&reservation.signal));
        }
        if let Some(cached) = self.failure_to_replay(key) {
            return FileServerStart::Failed(cached);
        }
        self.reap_unreferenced_children_for(key);
        self.log_transient_retry(key);
        match self.prepare_spawn(def, &key.root, file_path, config) {
            Ok(prepared) => {
                self.starting.insert(
                    key.clone(),
                    StartReservation {
                        deferred_events: Vec::new(),
                        clients_generation: self.clients_generation,
                        signal: Arc::new(StartSignal::default()),
                    },
                );
                FileServerStart::Spawn(Box::new(prepared))
            }
            Err(error) => FileServerStart::Failed(self.record_file_start_failure(
                def,
                key,
                prepare_failure(error),
            )),
        }
    }

    /// Remember a failed start the way [`Self::ensure_server_for_file_detailed`]
    /// does, and return the result to report.
    fn record_file_start_failure(
        &mut self,
        def: &ServerDef,
        key: &ServerKey,
        failure: StartFailure,
    ) -> ServerAttemptResult {
        slog_error!("failed to spawn {}: {}", def.name, failure.error);
        let result = classify_spawn_error(&def.binary, &failure.error);
        // Remember the failure so subsequent file events skip this
        // (kind, root) pair instead of producing a fresh spawn attempt +
        // ERROR log per request.
        self.record_failed_spawn(key, result, failure.durability)
    }

    /// Last step of [`ensure_server_for_file_detailed_unlocked`] for one
    /// server, under the lock: publish the client or record the failure,
    /// then release the reservation. `None` means the server is running.
    fn finish_file_server_start(
        &mut self,
        def: &ServerDef,
        key: &ServerKey,
        result: Result<LspClient, SpawnFailure>,
    ) -> Option<ServerAttemptResult> {
        let started_generation = self
            .starting
            .get(key)
            .map(|reservation| reservation.clients_generation);
        let outcome = match result {
            // Another path started the same server meanwhile; keep that
            // client and let this one's drop stop its process.
            Ok(_) if self.clients.contains_key(key) => {
                slog_info!(
                    "lsp start discarded server={} root={} reason=duplicate start; stopping redundant client",
                    key.kind.id_str(),
                    key.root.display()
                );
                None
            }
            // Every client was taken away while this one was starting
            // (shutdown, idle reap, unbind); do not revive the manager.
            Ok(_) if started_generation != Some(self.clients_generation) => {
                slog_info!(
                    "lsp start discarded server={} root={} reason=shutdown during initialize; stopping client",
                    key.kind.id_str(),
                    key.root.display()
                );
                Some(ServerAttemptResult::SpawnFailed {
                    binary: def.binary.clone(),
                    reason: "language servers were shut down while this one was starting"
                        .to_string(),
                })
            }
            Ok(client) => {
                self.note_start_succeeded(key);
                self.clients.insert(key.clone(), client);
                self.server_binaries.insert(key.clone(), def.binary.clone());
                self.documents.entry(key.clone()).or_default();
                None
            }
            Err(failure) => {
                let failure = self.absorb_spawn_failure(failure);
                if self.clients.contains_key(key) {
                    None
                } else {
                    Some(self.record_file_start_failure(def, key, failure))
                }
            }
        };
        self.release_start_reservation(key);
        outcome
    }

    /// Ensure a server is running for the given file. Spawns if needed.
    /// Returns the active server keys for the file, or an empty vec if none match.
    ///
    /// This is the lightweight wrapper around [`ensure_server_for_file_detailed`]
    /// that drops failure context. Prefer the detailed variant in command
    /// handlers that need to surface honest error messages to the agent.
    pub fn ensure_server_for_file(&mut self, file_path: &Path, config: &Config) -> Vec<ServerKey> {
        self.ensure_server_for_file_detailed(file_path, config)
            .successful
    }

    fn running_server_keys_for_file(&self, file_path: &Path, config: &Config) -> Vec<ServerKey> {
        servers_for_file(file_path, config)
            .into_iter()
            .filter_map(|def| server_key_for_definition(&def, file_path, config))
            .filter(|key| self.clients.contains_key(key))
            .collect()
    }

    /// Return whether a navigation request would need to initialize an applicable
    /// server. Missing binaries, inapplicable root markers, and cached start
    /// failures not yet due for a retry stay on the synchronous path because
    /// they cannot incur a handshake wait.
    pub fn navigation_requires_deferred_execution(
        &self,
        file_path: &Path,
        config: &Config,
    ) -> bool {
        let Ok(canonical_path) = canonicalize_for_lsp(file_path) else {
            return false;
        };
        servers_for_file(&canonical_path, config)
            .into_iter()
            .filter_map(|definition| {
                let key = server_key_for_definition(&definition, &canonical_path, config)?;
                Some((definition, key))
            })
            .any(|(definition, key)| {
                if let Some(client) = self.clients.get(&key) {
                    return client.state() != ServerState::Ready;
                }
                self.failure_to_replay(&key).is_none()
                    && self.resolve_binary(&definition, &key.root, config).is_ok()
            })
    }

    /// Detailed version of [`ensure_server_for_file`] that records every
    /// matching server's outcome (`Ok` / `NoRootMarker` / `BinaryNotInstalled`
    /// / `SpawnFailed`).
    ///
    /// Use this when the caller wants to honestly report _why_ a file has no
    /// active server (e.g., to surface "bash-language-server not on PATH" to
    /// the agent instead of silently returning `total: 0`).
    pub fn ensure_server_for_file_detailed(
        &mut self,
        file_path: &Path,
        config: &Config,
    ) -> EnsureServerOutcomes {
        let defs = servers_for_file(file_path, config);
        let mut outcomes = EnsureServerOutcomes::default();

        for def in defs {
            let server_id = def.kind.id_str().to_string();
            let server_name = def.name.to_string();

            let Some(key) = server_key_for_definition(&def, file_path, config) else {
                outcomes.attempts.push(ServerAttempt {
                    server_id,
                    server_name,
                    result: ServerAttemptResult::NoRootMarker {
                        looked_for: def.root_markers.iter().map(|s| s.to_string()).collect(),
                    },
                });
                continue;
            };

            if !self.clients.contains_key(&key) {
                // If this server already failed to start for this root,
                // return the remembered failure without retrying or
                // re-logging. This prevents per-edit ERROR spam when the
                // user's environment is missing a dependency the LSP needs
                // (the typescript-language-server "Could not find a valid
                // TypeScript installation" case is the canonical example).
                // A transient failure is retried once its backoff window has
                // passed.
                if let Some(cached) = self.failure_to_replay(&key) {
                    outcomes.attempts.push(ServerAttempt {
                        server_id,
                        server_name,
                        result: cached,
                    });
                    continue;
                }

                // A spurious ServerExited can drop the client without killing
                // the child. Reap any still-tracked processes for this pair
                // before spawning a replacement, otherwise leaked servers
                // accumulate on a live worktree.
                self.reap_unreferenced_children_for(&key);
                self.log_transient_retry(&key);

                match self.spawn_server(&def, &key.root, file_path, config) {
                    Ok(client) => {
                        self.note_start_succeeded(&key);
                        self.clients.insert(key.clone(), client);
                        self.server_binaries.insert(key.clone(), def.binary.clone());
                        self.documents.entry(key.clone()).or_default();
                    }
                    Err(failure) => {
                        slog_error!("failed to spawn {}: {}", def.name, failure.error);
                        let result = classify_spawn_error(&def.binary, &failure.error);
                        // Remember the failure so subsequent file events skip
                        // this (kind, root) pair instead of producing a fresh
                        // spawn attempt + ERROR log per request.
                        let result = self.record_failed_spawn(&key, result, failure.durability);
                        outcomes.attempts.push(ServerAttempt {
                            server_id,
                            server_name,
                            result,
                        });
                        continue;
                    }
                }
            }

            outcomes.attempts.push(ServerAttempt {
                server_id,
                server_name,
                result: ServerAttemptResult::Ok {
                    server_key: key.clone(),
                },
            });
            outcomes.successful.push(key);
        }

        outcomes
    }

    /// Ensure a server is running using the default LSP registry.
    /// Kept for integration tests that exercise built-in server helpers directly.
    pub fn ensure_server_for_file_default(&mut self, file_path: &Path) -> Vec<ServerKey> {
        self.ensure_server_for_file(file_path, &Config::default())
    }
    /// Ensure that servers are running for the file and that the document is open
    /// in each server's DocumentStore. Reads file content from disk if not already open.
    /// The result identifies which servers were already tracking the document and which
    /// received `textDocument/didOpen` during this call.
    pub fn ensure_file_open(
        &mut self,
        file_path: &Path,
        config: &Config,
    ) -> Result<EnsureFileOpenResult, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        self.end_inspect_close_exception(&canonical_path);
        let server_keys = self.ensure_server_for_file(&canonical_path, config);
        if server_keys.is_empty() {
            return Ok(EnsureFileOpenResult::default());
        }

        let uri = uri_for_path(&canonical_path)?;
        let language_id = language_id_for_extension(
            canonical_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default(),
        )
        .to_string();
        let needs_content = server_keys.iter().any(|key| {
            !self
                .documents
                .get(key)
                .is_some_and(|store| store.is_open(&canonical_path))
        });
        // One read of the file's disk state serves every server's store:
        // reading and hashing it per server cost one full read each.
        let disk = DiskSnapshot::read(&canonical_path);
        let initial_content = needs_content
            .then(|| std::fs::read_to_string(&canonical_path).map_err(LspError::Io))
            .transpose()?;
        let mut drift_content: Option<String> = None;
        let mut newly_opened = Vec::new();

        for key in &server_keys {
            let already_open = self
                .documents
                .get(key)
                .is_some_and(|store| store.is_open(&canonical_path));

            if !already_open {
                let content = initial_content
                    .as_ref()
                    .expect("content is loaded when any server needs didOpen");
                let (send_result, sent) = if let Some(client) = self.clients.get_mut(key) {
                    (
                        client.send_notification::<DidOpenTextDocument>(
                            DidOpenTextDocumentParams {
                                text_document: TextDocumentItem::new(
                                    uri.clone(),
                                    language_id.clone(),
                                    0,
                                    content.clone(),
                                ),
                            },
                        ),
                        true,
                    )
                } else {
                    (Ok(()), false)
                };
                if let Err(err) = send_result {
                    if matches!(err, LspError::Timeout(_)) {
                        // The writer owns the queued frame and can finish it
                        // after the process resumes. Do not send didOpen twice.
                        self.documents
                            .entry(key.clone())
                            .or_default()
                            .open_with(canonical_path.clone(), &DiskSnapshot::default());
                    }
                    let _ = self.close_file_for_servers(&canonical_path, &newly_opened);
                    return Err(err);
                }
                if sent {
                    log_did_open_sent(key, &canonical_path, &language_id);
                }
                self.documents
                    .entry(key.clone())
                    .or_default()
                    .open_with(canonical_path.clone(), &disk);
                newly_opened.push(key.clone());
                continue;
            }

            // Document is already open. Check disk drift — if the file has
            // been modified outside the AFT pipeline (other tool, manual
            // edit, sibling session) we MUST send a didChange before any
            // pull-diagnostic / hover query, otherwise the LSP server
            // returns results computed from stale in-memory content.
            //
            // Without this, ensure_file_open would skip an already-open file
            // without checking whether its disk content changed, leaving the
            // server's in-memory copy stale.
            let drifted = self
                .documents
                .get(key)
                .is_some_and(|store| store.is_stale_against(&canonical_path, &disk));
            if drifted {
                if drift_content.is_none() {
                    match std::fs::read_to_string(&canonical_path) {
                        Ok(content) => drift_content = Some(content),
                        Err(err) => {
                            let _ = self.close_file_for_servers(&canonical_path, &newly_opened);
                            return Err(LspError::Io(err));
                        }
                    }
                }
                let content = drift_content.as_deref().unwrap_or_default();
                let next_version = self
                    .documents
                    .get(key)
                    .and_then(|store| store.version(&canonical_path))
                    .map(|v| v + 1)
                    .unwrap_or(1);
                let send_result = if let Some(client) = self.clients.get_mut(key) {
                    // The file changed on disk, which for the server is a
                    // save: rust-analyzer re-runs `cargo check` only on
                    // `didSave`, so without it the compiler errors it
                    // reports keep describing the old contents.
                    client
                        .send_full_did_change(&uri, next_version, content)
                        .and_then(|()| send_did_save(client, &uri, content))
                } else {
                    Ok(())
                };
                if let Err(err) = send_result {
                    if let Some(store) = self.documents.get_mut(key) {
                        // A timed-out write may arrive later. Reserve its
                        // version, but retain disk drift until a confirmed sync.
                        store.bump_version_with(&canonical_path, &DiskSnapshot::default());
                    }
                    let _ = self.close_file_for_servers(&canonical_path, &newly_opened);
                    return Err(err);
                }
                if let Some(store) = self.documents.get_mut(key) {
                    store.bump_version_with(&canonical_path, &disk);
                }
            }
        }

        Ok(EnsureFileOpenResult {
            server_keys,
            newly_opened,
        })
    }

    pub fn ensure_file_open_default(
        &mut self,
        file_path: &Path,
    ) -> Result<EnsureFileOpenResult, LspError> {
        self.ensure_file_open(file_path, &Config::default())
    }

    /// Notify relevant LSP servers that a file has been written/changed.
    /// This is the main hook called after every file write in AFT.
    ///
    /// If the file's server isn't running yet, starts it (lazy spawn).
    /// If the file isn't open in LSP yet, sends didOpen. Otherwise sends didChange.
    pub fn notify_file_changed(
        &mut self,
        file_path: &Path,
        content: &str,
        config: &Config,
    ) -> Result<(), LspError> {
        self.notify_file_changed_versioned(file_path, content, config)
            .map(|_| ())
    }

    /// Like `notify_file_changed`, but returns the target document version
    /// per server so the post-edit waiter can match `publishDiagnostics`
    /// against the exact version that this notification carried.
    ///
    /// Returns: `Vec<(ServerKey, target_version)>`. `target_version` is the
    /// `version` field on the `VersionedTextDocumentIdentifier` we just sent
    /// (post-bump). For freshly-opened documents (`didOpen`) the version is
    /// `0`. Servers that do not echo a document version cannot prove a report
    /// describes this edit, so post-edit observation leaves them pending rather
    /// than accepting a report merely because it arrived after the edit.
    pub fn notify_file_changed_versioned(
        &mut self,
        file_path: &Path,
        content: &str,
        config: &Config,
    ) -> Result<Vec<(ServerKey, i32)>, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let server_keys = self.ensure_server_for_file(&canonical_path, config);
        self.notify_file_changed_for_server_keys(canonical_path, content, server_keys)
    }

    /// Notify only LSP servers that are already running for this file.
    ///
    /// Post-write notifications are best-effort and must not make a mutation
    /// wait for cold server startup. Explicit LSP requests use
    /// [`Self::notify_file_changed_versioned`] and retain lazy startup.
    pub fn notify_file_changed_if_running(
        &mut self,
        file_path: &Path,
        content: &str,
        config: &Config,
    ) -> Result<(), LspError> {
        self.notify_file_changed_if_running_versioned(file_path, content, config)
            .map(|_| ())
    }

    /// Notify only already-live servers and retain the document versions that
    /// prove a subsequent diagnostics report belongs to this post-edit wait.
    /// Unlike `notify_file_changed_versioned`, this path never starts a server.
    pub fn notify_file_changed_if_running_versioned(
        &mut self,
        file_path: &Path,
        content: &str,
        config: &Config,
    ) -> Result<Vec<(ServerKey, i32)>, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let server_keys = self.running_server_keys_for_file(&canonical_path, config);
        self.notify_file_changed_for_server_keys(canonical_path, content, server_keys)
    }

    fn notify_file_changed_for_server_keys(
        &mut self,
        canonical_path: PathBuf,
        content: &str,
        server_keys: Vec<ServerKey>,
    ) -> Result<Vec<(ServerKey, i32)>, LspError> {
        if server_keys.is_empty() {
            return Ok(Vec::new());
        }
        self.end_inspect_close_exception(&canonical_path);

        let uri = uri_for_path(&canonical_path)?;
        let language_id = language_id_for_extension(
            canonical_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default(),
        )
        .to_string();

        let mut versions: Vec<(ServerKey, i32)> = Vec::with_capacity(server_keys.len());
        // Read once for every server's store (see `ensure_file_open`).
        let disk = DiskSnapshot::read(&canonical_path);

        for key in server_keys {
            let current_version = self
                .documents
                .get(&key)
                .and_then(|store| store.version(&canonical_path));

            if let Some(version) = current_version {
                let next_version = version + 1;
                if let Some(client) = self.clients.get_mut(&key) {
                    let send = client.send_full_did_change(&uri, next_version, content)
                    // AFT has written this content to disk: tell the server
                    // it was saved. rust-analyzer re-runs `cargo check` only
                    // on `didSave`, so without it the compiler errors it
                    // reports keep describing the file before the edit.
                        .and_then(|()| send_did_save(client, &uri, content));
                    if let Err(err) = send {
                        if let Some(store) = self.documents.get_mut(&key) {
                            store.bump_version_with(&canonical_path, &DiskSnapshot::default());
                        }
                        return Err(err);
                    }
                }
                if let Some(store) = self.documents.get_mut(&key) {
                    store.bump_version_with(&canonical_path, &disk);
                }
                versions.push((key, next_version));
                continue;
            }

            if let Some(client) = self.clients.get_mut(&key) {
                let send =
                    client.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                        text_document: TextDocumentItem::new(
                            uri.clone(),
                            language_id.clone(),
                            0,
                            content.to_string(),
                        ),
                    });
                if let Err(err) = send {
                    if matches!(err, LspError::Timeout(_)) {
                        self.documents
                            .entry(key.clone())
                            .or_default()
                            .open_with(canonical_path.clone(), &DiskSnapshot::default());
                    }
                    return Err(err);
                }
                log_did_open_sent(&key, &canonical_path, &language_id);
                // The content was just written to disk: tell the server it
                // was saved, so rust-analyzer re-runs `cargo check`.
                if let Err(err) = send_did_save(client, &uri, content) {
                    self.documents
                        .entry(key.clone())
                        .or_default()
                        .open_with(canonical_path.clone(), &DiskSnapshot::default());
                    return Err(err);
                }
            }
            self.documents
                .entry(key.clone())
                .or_default()
                .open_with(canonical_path.clone(), &disk);
            // didOpen carries version 0 — that's the version the server
            // will echo on its first publishDiagnostics for this document.
            versions.push((key, 0));
        }

        Ok(versions)
    }

    /// Send `didChange` for every open document whose file changed on disk
    /// since it was last synced, to the servers already running for it. Used
    /// when more queued changes arrived than the backlog lists (see
    /// [`crate::lsp::pending_changes::PENDING_LSP_DOCUMENTS_CAP`]): the lost
    /// paths are unknown, but every open document they could matter for is
    /// checked.
    pub(crate) fn resync_drifted_open_documents(&mut self, config: &Config) -> usize {
        let drifted: std::collections::BTreeSet<PathBuf> = self
            .documents
            .values()
            .flat_map(|store| {
                store
                    .open_documents()
                    .into_iter()
                    .filter(|path| store.is_stale_on_disk(path))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut resynced = 0;
        for path in drifted {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            match self.notify_file_changed_if_running(&path, &content, config) {
                Ok(()) => resynced += 1,
                Err(error) => {
                    crate::slog_warn!("drift resync failed for {}: {error}", path.display())
                }
            }
        }
        resynced
    }

    pub fn notify_file_changed_default(
        &mut self,
        file_path: &Path,
        content: &str,
    ) -> Result<(), LspError> {
        self.notify_file_changed(file_path, content, &Config::default())
    }

    /// Notify every active server whose workspace contains at least one changed
    /// path that watched files changed. This is intentionally workspace-scoped
    /// rather than extension-scoped: configuration edits such as `package.json`
    /// or `tsconfig.json` affect a server's project graph even though those
    /// files may not be documents handled by the server itself.
    pub fn notify_files_watched_changed(
        &mut self,
        paths: &[(PathBuf, FileChangeType)],
        _config: &Config,
    ) -> Result<(), LspError> {
        #[cfg(windows)]
        let mut trace = vec![format!(
            "input_paths={paths:?}; active_keys={:?}",
            self.clients.keys().collect::<Vec<_>>()
        )];

        if paths.is_empty() {
            #[cfg(windows)]
            {
                trace.push("outcome=no-input-paths".to_string());
                self.last_watched_file_notification_trace = trace.join("\n");
            }
            return Ok(());
        }

        let mut canonical_events = Vec::with_capacity(paths.len());
        for (path, typ) in paths {
            let canonical_path = resolve_for_lsp_uri(path);
            canonical_events.push((canonical_path, *typ));
        }
        #[cfg(windows)]
        trace.push(format!("resolved_events={canonical_events:?}"));

        let keys: Vec<ServerKey> = self.clients.keys().cloned().collect();
        #[cfg(windows)]
        if keys.is_empty() {
            trace.push("outcome=no-active-client".to_string());
        }
        for key in keys {
            let mut changes = Vec::new();
            for (path, typ) in &canonical_events {
                if !path.starts_with(&key.root) {
                    continue;
                }
                changes.push(FileEvent::new(uri_for_path(path)?, *typ));
            }

            if changes.is_empty() {
                #[cfg(windows)]
                trace.push(format!("key={key:?}; outcome=outside-root"));
                continue;
            }

            if let Some(client) = self.clients.get_mut(&key) {
                // Send when the server either advertised initialize-time
                // watched-file support or dynamically registered a watcher.
                // The dynamic client capability we send during initialize only
                // permits runtime registration; it is tracked separately via
                // `has_watched_file_registration()`.
                let supports_static_watched_files = client.supports_watched_files();
                let has_dynamic_registration = client.has_watched_file_registration();
                if !(supports_static_watched_files || has_dynamic_registration) {
                    #[cfg(windows)]
                    trace.push(format!(
                        "key={key:?}; changes={changes:?}; outcome=unsupported; static={supports_static_watched_files}; dynamic={has_dynamic_registration}"
                    ));
                    if self.watched_file_skip_logged.insert(key.clone()) {
                        log::debug!(
                            "skipping didChangeWatchedFiles for {:?} (not supported or registered)",
                            key
                        );
                    }
                    continue;
                }
                #[cfg(windows)]
                trace.push(format!("key={key:?}; changes={changes:?}; action=send"));
                let send_result = client.send_notification::<DidChangeWatchedFiles>(
                    DidChangeWatchedFilesParams { changes },
                );
                #[cfg(windows)]
                trace.push(format!(
                    "key={key:?}; outcome={}",
                    if send_result.is_ok() {
                        "sent"
                    } else {
                        "send-error"
                    }
                ));
                if let Err(error) = send_result {
                    #[cfg(windows)]
                    {
                        self.last_watched_file_notification_trace = trace.join("\n");
                    }
                    return Err(error);
                }
            }
        }

        #[cfg(windows)]
        {
            self.last_watched_file_notification_trace = trace.join("\n");
        }
        Ok(())
    }

    /// Forward changes the project file watcher saw (edits made by other
    /// programs, `cargo update`, a branch switch) to the running servers that
    /// asked to be told about them, one `workspace/didChangeWatchedFiles` per
    /// server per batch.
    ///
    /// Servers are gated exactly like [`Self::notify_files_watched_changed`]:
    /// only one that advertised watched-file support or registered a watcher
    /// hears anything. A server with dynamic registrations gets the events
    /// its registered globs and change kinds select. A server with only
    /// initialize-time support never said which files it wants, so it gets
    /// the project configuration files (see `is_config_file_path_with_custom`),
    /// the same set AFT's own edits forward.
    ///
    /// A batch selecting more than [`WATCHED_FILE_FORWARD_CAP`] events for one
    /// server (a large checkout or generated tree) is not sent per file: the
    /// server gets only the configuration files in it, and nothing if even
    /// those overflow.
    ///
    /// Returns, per rust-analyzer instance, the Cargo manifests, lockfiles,
    /// toolchain files or Cargo configs in the batch that it did NOT receive
    /// through a watcher it registered (the list is empty after an overflow,
    /// when the batch was not inspected per file). rust-analyzer 1.98 re-runs
    /// `cargo metadata` on its own when told about a `Cargo.toml` or
    /// `Cargo.lock` matching its registered globs, so those need nothing
    /// more. For the rest (a file its globs do not cover, a server that
    /// registered no globs, a batch too large to forward) the caller asks
    /// for a manifest-gated reload instead (see
    /// [`spawn_watcher_rust_workspace_reload`]).
    pub fn forward_watcher_file_events(
        &mut self,
        events: &[(PathBuf, FileChangeType)],
        extra_config_markers: &[String],
    ) -> Vec<(ServerKey, Vec<PathBuf>)> {
        let mut rust_manifest_changes = Vec::new();
        if events.is_empty() {
            return rust_manifest_changes;
        }
        // A server's root and globs may spell a path either the way the
        // watcher reported it or resolved (on macOS a temp dir is both
        // `/var/...` and `/private/var/...`), so both spellings are matched.
        let events: Vec<(&Path, PathBuf, FileChangeType)> = events
            .iter()
            .map(|(path, typ)| (path.as_path(), resolve_for_lsp_uri(path), *typ))
            .collect();
        let keys: Vec<ServerKey> = self.clients.keys().cloned().collect();
        for key in keys {
            let in_root: Vec<&(&Path, PathBuf, FileChangeType)> = events
                .iter()
                .filter(|(raw, resolved, _)| {
                    resolved.starts_with(&key.root) || raw.starts_with(&key.root)
                })
                .collect();
            if in_root.is_empty() {
                continue;
            }
            let mut manifests: Vec<PathBuf> = if key.kind == ServerKind::Rust {
                in_root
                    .iter()
                    .filter(|(_, resolved, _)| is_rust_workspace_manifest(&key.root, resolved))
                    .map(|(_, resolved, _)| resolved.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let mut overflowed = false;
            let mut sent_to_registered_watcher: Vec<PathBuf> = Vec::new();

            if let Some(client) = self.clients.get(&key) {
                if client.supports_watched_files() || client.has_watched_file_registration() {
                    let watchers = client.registered_file_watchers();
                    let is_config =
                        |raw: &Path| is_config_file_path_with_custom(raw, extra_config_markers);
                    let wanted =
                        |(raw, resolved, typ): &&&(&Path, PathBuf, FileChangeType)| match &watchers
                        {
                            Some(watchers) => {
                                watchers.matches(raw, *typ) || watchers.matches(resolved, *typ)
                            }
                            None => is_config(raw),
                        };
                    // `take` bounds the work as well as the message: a burst of
                    // a hundred thousand paths stops being inspected one past
                    // the cap.
                    let mut selected: Vec<_> = in_root
                        .iter()
                        .filter(wanted)
                        .take(WATCHED_FILE_FORWARD_CAP + 1)
                        .collect();
                    if selected.len() > WATCHED_FILE_FORWARD_CAP {
                        overflowed = true;
                        selected = in_root
                            .iter()
                            .filter(wanted)
                            .filter(|(raw, _, _)| is_config(raw))
                            .take(WATCHED_FILE_FORWARD_CAP + 1)
                            .collect();
                        if selected.len() > WATCHED_FILE_FORWARD_CAP {
                            selected.clear();
                        }
                        slog_info!(
                            "watched-file forward for {:?} exceeded {} files; sending {} configuration file(s) only",
                            key,
                            WATCHED_FILE_FORWARD_CAP,
                            selected.len()
                        );
                    }
                    let changes: Vec<FileEvent> = selected
                        .iter()
                        .filter_map(|(_, resolved, typ)| {
                            uri_for_path(resolved)
                                .ok()
                                .map(|uri| FileEvent::new(uri, *typ))
                        })
                        .collect();
                    if !changes.is_empty() {
                        let sent = self.clients.get_mut(&key).map(|client| {
                            client.send_notification::<DidChangeWatchedFiles>(
                                DidChangeWatchedFilesParams { changes },
                            )
                        });
                        match sent {
                            Some(Ok(())) if watchers.is_some() => {
                                sent_to_registered_watcher = selected
                                    .iter()
                                    .map(|(_, resolved, _)| resolved.clone())
                                    .collect();
                            }
                            Some(Err(error)) => {
                                crate::slog_warn!(
                                    "watched-file forward to {:?} failed: {error}",
                                    key
                                );
                            }
                            _ => {}
                        }
                    }
                } else if self.watched_file_skip_logged.insert(key.clone()) {
                    log::debug!(
                        "skipping didChangeWatchedFiles for {:?} (not supported or registered)",
                        key
                    );
                }
            }

            if key.kind != ServerKind::Rust {
                continue;
            }
            let changed: Vec<PathBuf> = in_root
                .iter()
                .map(|(_, resolved, _)| resolved.clone())
                .collect();
            self.announce_external_rust_save(&key, &changed);
            manifests.retain(|manifest| {
                !sent_to_registered_watcher.contains(manifest)
                    || !matches!(
                        manifest.file_name().and_then(|name| name.to_str()),
                        Some("Cargo.toml" | "Cargo.lock")
                    )
            });
            // After an overflow the batch was not inspected per file, so ask
            // anyway; the reload is gated on manifest modification times and
            // costs nothing when none changed.
            if !manifests.is_empty() || overflowed {
                rust_manifest_changes.push((key, manifests));
            }
        }
        rust_manifest_changes
    }

    /// Tell rust-analyzer that a Rust source file changed outside AFT (an
    /// editor, a script, a branch switch). rust-analyzer re-runs `cargo
    /// check` only when told that a file was saved, and a watched-file event
    /// is not such a notice, so without this the compiler errors it reports
    /// keep describing the files before the change: new errors missing and
    /// fixed ones still listed. The save is sent a little later (see
    /// [`LspClient::owe_rust_save`]) because rust-analyzer drops a check
    /// asked for right beside the watched-file event. One save per batch is
    /// enough because the check covers the whole workspace. A file AFT wrote
    /// itself was announced as saved when it was written, and its document
    /// still matches the disk, so AFT's own edits do not restart the check.
    fn announce_external_rust_save(&mut self, key: &ServerKey, changed: &[PathBuf]) {
        // Deciding whether AFT already announced a file reads and hashes it.
        // To bound that work, look at this many changed files at most; if all
        // of them were announced and more remain, announce the first one.
        const SCAN_CAP: usize = 64;
        let documents = self.documents.get(key);
        let mut rust_files = changed
            .iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .peekable();
        let first = rust_files.peek().map(|path| (*path).clone());
        let saved = rust_files
            .by_ref()
            .take(SCAN_CAP)
            .find(|path| {
                !documents.is_some_and(|store| store.is_open(path) && !store.is_stale_on_disk(path))
            })
            .cloned()
            .or_else(|| rust_files.next().and(first));
        let Some(path) = saved else {
            return;
        };
        let Ok(uri) = uri_for_path(&path) else {
            return;
        };
        if let Some(client) = self.clients.get_mut(key) {
            if client.save_notification().is_some() {
                client.owe_rust_save(&uri);
            }
        }
    }

    /// Register a watcher-started rust-analyzer reload. Returns false when
    /// one is already running for `key`; `manifests` are then queued for
    /// that reload's thread to check once it finishes.
    pub(crate) fn claim_watcher_rust_reload(
        &mut self,
        key: &ServerKey,
        manifests: &[PathBuf],
    ) -> bool {
        match self.watcher_rust_reloads.get_mut(key) {
            Some(pending) => {
                pending
                    .get_or_insert_with(Vec::new)
                    .extend(manifests.iter().cloned());
                false
            }
            None => {
                self.watcher_rust_reloads.insert(key.clone(), None);
                true
            }
        }
    }

    /// Manifests queued while a watcher-started reload ran, or `None` when
    /// nothing was queued, which also ends that reload's registration.
    pub(crate) fn take_queued_watcher_rust_reload(
        &mut self,
        key: &ServerKey,
    ) -> Option<Vec<PathBuf>> {
        let queued = self.watcher_rust_reloads.get_mut(key)?.take();
        if queued.is_none() {
            self.watcher_rust_reloads.remove(key);
        }
        queued
    }

    /// Every running rust-analyzer instance.
    pub(crate) fn rust_server_keys(&self) -> Vec<ServerKey> {
        self.clients
            .keys()
            .filter(|key| key.kind == ServerKind::Rust)
            .cloned()
            .collect()
    }

    /// Close a document in all servers that have it open.
    pub fn notify_file_closed(&mut self, file_path: &Path) -> Result<(), LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let keys = self
            .documents
            .iter()
            .filter(|(_, store)| store.is_open(&canonical_path))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        self.close_file_for_servers(&canonical_path, &keys)
    }

    /// Close a document only in the specified servers.
    ///
    /// Scoped inspection uses this to release documents it opened without
    /// disturbing pre-existing editor documents in other server stores.
    pub(crate) fn close_file_for_servers(
        &mut self,
        file_path: &Path,
        server_keys: &[ServerKey],
    ) -> Result<(), LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let uri = uri_for_path(&canonical_path)?;
        let mut first_error = None;

        for key in server_keys {
            let was_open = self
                .documents
                .get(key)
                .is_some_and(|store| store.is_open(&canonical_path));
            if !was_open {
                continue;
            }

            if let Some(client) = self.clients.get_mut(key) {
                if let Err(err) =
                    client.send_notification::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                        text_document: TextDocumentIdentifier::new(uri.clone()),
                    })
                {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }

            if let Some(store) = self.documents.get_mut(key) {
                store.close(&canonical_path);
            }
            self.diagnostics.clear_for_server_file(key, &canonical_path);
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Open a document in the given running servers so they analyze it and
    /// publish its diagnostics. Unlike [`Self::ensure_file_open`] this never
    /// starts a server: a scoped inspect has already started exactly the
    /// servers its scope needs, and must not spawn others from inside a file
    /// loop. Servers that are not running, or already have the document open,
    /// are skipped. Returns the servers that received `didOpen` in this call.
    pub(crate) fn open_document_for_servers(
        &mut self,
        file_path: &Path,
        server_keys: &[ServerKey],
    ) -> Result<Vec<ServerKey>, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let targets = server_keys
            .iter()
            .filter(|key| self.clients.contains_key(*key))
            .filter(|key| {
                !self
                    .documents
                    .get(*key)
                    .is_some_and(|store| store.is_open(&canonical_path))
            })
            .cloned()
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let uri = uri_for_path(&canonical_path)?;
        let language_id = language_id_for_extension(
            canonical_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default(),
        )
        .to_string();
        let content = std::fs::read_to_string(&canonical_path).map_err(LspError::Io)?;
        let mut opened = Vec::with_capacity(targets.len());
        for key in targets {
            let Some(client) = self.clients.get_mut(&key) else {
                continue;
            };
            let sent = client.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: TextDocumentItem::new(
                    uri.clone(),
                    language_id.clone(),
                    0,
                    content.clone(),
                ),
            });
            if let Err(err) = sent {
                // Leave no half-opened set behind: the caller only closes what
                // this call reports as opened.
                let _ = self.close_inspect_documents(&canonical_path, &opened);
                return Err(err);
            }
            log_did_open_sent(&key, &canonical_path, &language_id);
            self.documents
                .entry(key.clone())
                .or_default()
                .open(canonical_path.clone());
            self.inspect_closed_documents
                .remove(&(key.clone(), canonical_path.clone()));
            opened.push(key);
        }
        Ok(opened)
    }

    /// Close documents a scoped inspect opened with
    /// [`Self::open_document_for_servers`]. The diagnostics collected while
    /// they were open stay in the store: they are the latest analysis of the
    /// file, and a later external edit marks them stale through the watcher.
    /// The server's reply to the close is ignored (see
    /// `inspect_closed_documents`).
    pub(crate) fn close_inspect_documents(
        &mut self,
        file_path: &Path,
        server_keys: &[ServerKey],
    ) -> Result<(), LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let uri = uri_for_path(&canonical_path)?;
        let mut first_error = None;
        for key in server_keys {
            let was_open = self
                .documents
                .get(key)
                .is_some_and(|store| store.is_open(&canonical_path));
            if !was_open {
                continue;
            }
            if let Some(client) = self.clients.get_mut(key) {
                match client.send_notification::<DidCloseTextDocument>(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier::new(uri.clone()),
                }) {
                    Ok(()) => {
                        self.note_inspect_closed_document(key, &canonical_path);
                    }
                    Err(err) => {
                        if first_error.is_none() {
                            first_error = Some(err);
                        }
                    }
                }
            }
            if let Some(store) = self.documents.get_mut(key) {
                store.close(&canonical_path);
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Close the documents a file pull opened only to ask about them (see
    /// [`PullFileResult::opened_for_pull`]), keeping the diagnostics they
    /// produced, as a scoped inspect does with
    /// [`Self::close_inspect_documents`]. A document AFT edits stays open:
    /// an edit opens it through `didOpen`/`didChange`, not through a pull.
    /// A document reopened or edited since the pull is left open.
    pub(crate) fn close_documents_opened_for_pulls(
        &mut self,
        file_path: &Path,
        results: &[PullFileResult],
    ) {
        let keys: Vec<ServerKey> = results
            .iter()
            .filter(|result| result.opened_for_pull)
            .map(|result| result.server_key.clone())
            .filter(|key| {
                // Still at the version the pull opened: no edit since.
                canonicalize_for_lsp(file_path).is_ok_and(|canonical| {
                    self.documents
                        .get(key)
                        .and_then(|store| store.version(&canonical))
                        == Some(0)
                })
            })
            .collect();
        if keys.is_empty() {
            return;
        }
        if let Err(error) = self.close_inspect_documents(file_path, &keys) {
            crate::slog_warn!(
                "closing {} after a diagnostics pull failed: {error}",
                file_path.display()
            );
        }
    }

    /// Whether the running server answers `textDocument/diagnostic` requests.
    pub(crate) fn server_supports_pull(&self, server_key: &ServerKey) -> bool {
        self.clients
            .get(server_key)
            .and_then(|client| client.diagnostic_capabilities())
            .is_some_and(|caps| caps.pull_diagnostics)
    }

    /// This server's current stored diagnostics for a file, if it has a report.
    pub(crate) fn server_file_diagnostics(
        &self,
        server_key: &ServerKey,
        file_path: &Path,
    ) -> Option<Vec<StoredDiagnostic>> {
        let lookup_path = normalize_lookup_path(file_path);
        self.diagnostics
            .entries_for_file(&lookup_path)
            .into_iter()
            .find_map(|(key, entry)| (key == server_key).then(|| entry.diagnostics.clone()))
    }

    /// Store the union of a pulled report and the server's latest push for
    /// the same file, keeping the stored report's resultId and document
    /// version. See `latest_push_for_pull_servers` for why one would
    /// otherwise hide the other. The pulled part is remembered so a later
    /// `cargo check` push for the file is stored beside it too.
    pub(crate) fn store_pull_push_union(
        &mut self,
        server_key: &ServerKey,
        file_path: &Path,
        pulled: Vec<StoredDiagnostic>,
    ) {
        let lookup_path = normalize_lookup_path(file_path);
        let pushed = self
            .latest_push_for_pull_servers
            .get(&(server_key.clone(), lookup_path.clone()))
            .cloned()
            .unwrap_or_default();
        let (result_id, version) = self
            .diagnostics
            .entries_for_file(&lookup_path)
            .into_iter()
            .find_map(|(key, entry)| {
                (key == server_key).then(|| (entry.result_id.clone(), entry.version))
            })
            .unwrap_or((None, None));
        let merged = union_of_diagnostics(pulled.clone(), pushed);
        self.latest_pull_for_rust
            .insert((server_key.clone(), lookup_path.clone()), pulled);
        let provisional = self
            .clients
            .get(server_key)
            .is_some_and(|client| client.diagnostics_are_provisional());
        self.diagnostics.publish_full_with_provisional(
            server_key.clone(),
            lookup_path,
            merged,
            result_id,
            version,
            provisional,
        );
    }

    /// Whether a document is open in this server's document store.
    pub(crate) fn document_is_open_in(&self, server_key: &ServerKey, file_path: &Path) -> bool {
        let lookup_path = normalize_lookup_path(file_path);
        self.documents
            .get(server_key)
            .is_some_and(|store| store.is_open(&lookup_path))
    }

    /// The publish epoch of this server's current report for a file, if it
    /// has one. A higher epoch than one read earlier proves a newer report.
    pub(crate) fn diagnostic_epoch(&self, server_key: &ServerKey, file_path: &Path) -> Option<u64> {
        let lookup_path = normalize_lookup_path(file_path);
        self.diagnostics
            .entries_for_file(&lookup_path)
            .into_iter()
            .find_map(|(key, entry)| (key == server_key).then_some(entry.epoch))
    }

    /// Whether this server is a running client.
    pub(crate) fn has_client(&self, server_key: &ServerKey) -> bool {
        self.clients.contains_key(server_key)
    }

    /// Whether this rust-analyzer's published diagnostics carry the
    /// compiler's results for the files as they are now. See
    /// [`LspClient::rust_check_state`].
    pub(crate) fn rust_check_state(&self, server_key: &ServerKey) -> RustCheckState {
        self.clients
            .get(server_key)
            .map_or(RustCheckState::Current, |client| {
                client.rust_check_state(Instant::now(), FLYCHECK_PUBLISH_SETTLE)
            })
    }

    /// A completed compiler check can certify an empty Rust result without
    /// publishDiagnostics. No progress is not enough: the client requires
    /// matching begin/end events after the latest workspace load and save.
    pub(crate) fn rust_check_completed_current(&self, server_key: &ServerKey) -> bool {
        self.clients.get(server_key).is_some_and(|client| {
            client.rust_check_completed_current(Instant::now(), FLYCHECK_PUBLISH_SETTLE)
        })
    }

    pub(crate) fn saved_rust_checks(
        &self,
        deadline: Instant,
    ) -> HashMap<ServerKey, super::completed_rust_check::SavedCheck> {
        self.clients
            .iter()
            .filter_map(|(key, client)| {
                if key.kind != ServerKind::Rust
                    || self.producer_failure(key).is_some()
                    || self.rust_check_completed_current(key)
                {
                    return None;
                }
                let saved = client.completed_rust_check.as_ref()?.validated(deadline)?;
                Some((key.clone(), saved))
            })
            .collect()
    }

    pub(crate) fn rust_check_running_reason(&self, key: &ServerKey) -> String {
        self.clients
            .get(key)
            .and_then(|c| c.completed_rust_check.as_ref())
            .and_then(|cache| cache.running_reason())
            .unwrap_or_else(|| {
                crate::inspect::diagnostics_category::RUST_CHECK_RUNNING_REASON.to_string()
            })
    }

    /// Ask rust-analyzer again for a check that was expected and did not
    /// begin by its deadline (see
    /// [`LspClient::rearm_unreported_rust_check`]). Callers do this when they
    /// start waiting, so each new request waits for the check with a fresh
    /// deadline instead of answering "unknown" at once because an earlier
    /// request's check never began.
    pub(crate) fn rearm_unreported_rust_check(&mut self, server_key: &ServerKey) {
        let Some(client) = self.clients.get_mut(server_key) else {
            return;
        };
        if client.rearm_unreported_rust_check(Instant::now()) {
            let sent = client.send_notification::<RustAnalyzerRunFlycheck>(
                serde_json::json!({ "textDocument": null }),
            );
            match sent {
                Ok(()) => slog_info!(
                    "lsp_protocol server=rust root={} method=rust-analyzer/runFlycheck event=sent",
                    server_key.root.display()
                ),
                Err(error) => {
                    crate::slog_warn!("rust-analyzer/runFlycheck failed: {error}")
                }
            }
        }
        self.send_due_rust_saves();
    }

    /// Runtime notes (the SDK a server started with) for the given servers
    /// only, so an inspect scoped to Rust files does not repeat a TypeScript
    /// note from a server started for some other request.
    pub fn runtime_notes_for(&self, server_keys: &HashSet<ServerKey>) -> Vec<String> {
        let mut notes: Vec<_> = self
            .clients
            .iter()
            .filter(|(key, _)| server_keys.contains(*key))
            .filter_map(|(_, client)| client.runtime_note.clone())
            .collect();
        notes.sort();
        notes.dedup();
        notes
    }

    /// Get an active client for a file path, if one exists.
    pub fn client_for_file(&self, file_path: &Path, config: &Config) -> Option<&LspClient> {
        let key = self.server_key_for_file(file_path, config)?;
        self.clients.get(&key)
    }

    pub fn client_for_file_default(&self, file_path: &Path) -> Option<&LspClient> {
        self.client_for_file(file_path, &Config::default())
    }

    /// Get a mutable active client for a file path, if one exists.
    pub fn client_for_file_mut(
        &mut self,
        file_path: &Path,
        config: &Config,
    ) -> Option<&mut LspClient> {
        let key = self.server_key_for_file(file_path, config)?;
        self.clients.get_mut(&key)
    }

    pub fn client_for_file_mut_default(&mut self, file_path: &Path) -> Option<&mut LspClient> {
        self.client_for_file_mut(file_path, &Config::default())
    }

    /// Number of tracked server clients.
    pub fn active_client_count(&self) -> usize {
        self.clients.len()
    }

    /// Drain all pending LSP events. Call from the main loop.
    pub fn drain_events(&mut self) -> DrainedLspEvents {
        self.drain_events_bounded(usize::MAX)
    }

    /// Whether LSP events are waiting to be drained. Cheap channel peek for
    /// the maintenance scheduler's skip probe.
    pub fn has_pending_events(&self) -> bool {
        !self.event_rx.is_empty()
    }

    pub fn drain_events_bounded(&mut self, max_events: usize) -> DrainedLspEvents {
        let mut events = Vec::new();
        let mut diagnostics_changed = false;
        let mut accepted_snapshots = Vec::new();
        while events.len() < max_events {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            // `handle_event` returns the file of a stored publish; the
            // snapshot reuses it instead of parsing the payload again.
            if let Some(file) = self.handle_event(&event) {
                diagnostics_changed = true;
                if let Some(snapshot) = self.accepted_live_publish_snapshot(&event, &file) {
                    accepted_snapshots.push(snapshot);
                }
            }
            events.push(event);
        }
        let has_more = events.len() >= max_events && !self.event_rx.is_empty();
        self.send_due_rust_saves();
        for client in self.clients.values_mut() {
            if client.rust_check_completed_current(Instant::now(), FLYCHECK_PUBLISH_SETTLE)
                && !client.diagnostics_are_provisional()
            {
                if let Some(cache) = client.completed_rust_check.as_mut() {
                    cache.complete();
                }
            }
        }
        DrainedLspEvents {
            events,
            diagnostics_changed,
            accepted_snapshots,
            has_more,
        }
    }

    /// Send the rust-analyzer saves that are due: deferred saves for changes
    /// the file watcher reported, and saves that started no check run and
    /// may be sent again (see [`LspClient::take_rust_save_to_send`]). Runs
    /// with every event drain, so callers waiting for a check (inspect,
    /// `lsp_diagnostics`) keep these moving while they wait.
    fn send_due_rust_saves(&mut self) {
        let now = Instant::now();
        for client in self.clients.values_mut() {
            let Some(uri) = client.take_rust_save_to_send(now) else {
                continue;
            };
            let Some(save) = client.save_notification() else {
                continue;
            };
            let text = (save == SaveNotification::IncludeText)
                .then(|| uri_to_path(&uri).and_then(|path| std::fs::read_to_string(path).ok()))
                .flatten();
            let sent = client.send_notification::<DidSaveTextDocument>(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier::new(uri.clone()),
                text,
            });
            match sent {
                Ok(()) => {
                    client.mark_rust_save_sent(now);
                    slog_info!(
                        "lsp_protocol server=rust root={} method=textDocument/didSave event=sent-deferred uri={}",
                        client.root().display(),
                        uri.as_str()
                    );
                }
                Err(error) => {
                    crate::slog_warn!("deferred didSave for {} failed: {error}", uri.as_str());
                    // Count the attempt so a broken pipe cannot be retried
                    // on every drain.
                    client.mark_rust_save_sent(now);
                }
            }
        }
    }

    /// Wait for diagnostics to arrive for a specific file until a timeout expires.
    pub fn wait_for_diagnostics(
        &mut self,
        file_path: &Path,
        config: &Config,
        timeout: std::time::Duration,
    ) -> Vec<StoredDiagnostic> {
        let deadline = std::time::Instant::now() + timeout;
        self.wait_for_file_diagnostics(file_path, config, deadline)
    }

    pub fn wait_for_diagnostics_default(
        &mut self,
        file_path: &Path,
        timeout: std::time::Duration,
    ) -> Vec<StoredDiagnostic> {
        self.wait_for_diagnostics(file_path, &Config::default(), timeout)
    }

    /// Test-only accessor for the diagnostics store. Used by integration
    /// tests that need to inspect per-server entries (e.g., to verify that
    /// `ServerKey::root` is populated correctly, not the empty path that
    /// the legacy `publish_with_kind` path produced).
    #[doc(hidden)]
    pub fn diagnostics_store_for_test(&self) -> &DiagnosticsStore {
        &self.diagnostics
    }

    #[doc(hidden)]
    pub fn diagnostics_store_mut_for_test(&mut self) -> &mut DiagnosticsStore {
        &mut self.diagnostics
    }

    #[doc(hidden)]
    pub fn post_edit_outcome_for_entry_for_test(
        key: ServerKey,
        entry: &DiagnosticEntry,
        target_version: i32,
        pre: PreEditSnapshot,
    ) -> PostEditWaitOutcome {
        Self::post_edit_outcome_for_entry(key, entry, target_version, pre)
    }

    fn post_edit_outcome_for_entry(
        key: ServerKey,
        entry: &DiagnosticEntry,
        target_version: i32,
        pre: PreEditSnapshot,
    ) -> PostEditWaitOutcome {
        let mut fresh = HashMap::new();
        if let Some(diagnostics) =
            Self::authoritative_post_edit_diagnostics(entry, target_version, pre)
        {
            fresh.insert(key.clone(), diagnostics);
        }
        Self::post_edit_outcome(vec![(key, target_version)], fresh, Vec::new())
    }

    fn authoritative_post_edit_diagnostics(
        entry: &DiagnosticEntry,
        target_version: i32,
        pre: PreEditSnapshot,
    ) -> Option<Vec<StoredDiagnostic>> {
        (!entry.provisional && post_edit_entry_is_fresh(entry, target_version, pre))
            .then(|| entry.diagnostics.clone())
    }

    #[doc(hidden)]
    pub fn enqueue_event_for_test(&self, event: LspEvent) {
        self.event_tx
            .send(event)
            .expect("LSP event receiver should remain connected");
    }

    // Used only by the Unix-gated child-spawning test modules.
    #[cfg(all(test, unix))]
    pub(crate) fn event_sender_for_test(&self) -> Sender<LspEvent> {
        self.event_tx.clone()
    }

    // Used only by the Unix-gated child-spawning test modules.
    #[cfg(all(test, unix))]
    pub(crate) fn insert_client_for_test(&mut self, client: LspClient) {
        let key = ServerKey {
            kind: client.kind(),
            root: client.root().to_path_buf(),
        };
        self.clients.insert(key, client);
    }

    #[doc(hidden)]
    pub fn pending_event_count_for_test(&self) -> usize {
        self.event_rx.len()
    }

    #[doc(hidden)]
    pub fn document_is_open_for_test(&self, file_path: &Path) -> bool {
        canonicalize_for_lsp(file_path).is_ok_and(|canonical_path| {
            self.documents
                .values()
                .any(|store| store.is_open(&canonical_path))
        })
    }

    /// Error/warning counts across the entire warm diagnostics set (all files
    /// any server has published for this session). Powers the agent status bar;
    /// reads the continuously-drained store with no extra LSP round-trip.
    pub fn warm_error_warning_counts(&self) -> (usize, usize) {
        self.diagnostics.error_warning_counts()
    }

    pub fn warm_error_warning_counts_with_provisional(&self) -> ((usize, usize), bool) {
        self.diagnostics.error_warning_counts_with_provisional()
    }

    pub fn diagnostics_generation(&self) -> u64 {
        self.diagnostics.generation()
    }

    /// Status-bar error/warning counts with a per-file `keep` predicate and
    /// cross-server dedup applied (see
    /// [`DiagnosticsStore::filtered_error_warning_counts`]). The caller supplies
    /// the project-root + tsconfig-membership policy via `keep`.
    pub fn filtered_error_warning_counts(
        &self,
        keep: impl FnMut(&std::path::Path) -> bool,
    ) -> (usize, usize) {
        self.diagnostics.filtered_error_warning_counts(keep)
    }

    /// Status-bar counts plus whether any kept diagnostics came from a server
    /// that is still warming. The readiness flag is needed to retain the last
    /// authoritative E/W values while provisional reports replace old entries.
    pub fn filtered_error_warning_counts_with_provisional(
        &self,
        keep: impl FnMut(&std::path::Path) -> bool,
    ) -> ((usize, usize), bool) {
        self.diagnostics
            .filtered_error_warning_counts_with_provisional(keep)
    }

    /// Active rust-analyzer instances that have not yet reported quiescence.
    /// Other server kinds are intentionally absent because they do not use the
    /// rust-analyzer readiness extension.
    pub fn provisional_server_keys(&self) -> Vec<ServerKey> {
        self.clients
            .iter()
            .filter(|(_, client)| client.diagnostics_are_provisional())
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Snapshot the current per-server epoch for every entry that exists
    /// for `file_path`. Servers without an entry yet (never published)
    /// are absent from the map; for those, `pre = 0` (any first publish
    /// will be considered fresh under the epoch-fallback rule).
    pub fn snapshot_diagnostic_epochs(&self, file_path: &Path) -> HashMap<ServerKey, u64> {
        let lookup_path = normalize_lookup_path(file_path);
        self.diagnostics
            .entries_for_file(&lookup_path)
            .into_iter()
            .map(|(key, entry)| (key.clone(), entry.epoch))
            .collect()
    }

    /// Snapshot the current diagnostic epoch and document version for every
    /// active server relevant to `file_path` before a post-edit notification.
    pub fn snapshot_pre_edit_state(&self, file_path: &Path) -> HashMap<ServerKey, PreEditSnapshot> {
        let lookup_path = normalize_lookup_path(file_path);
        let mut snapshots: HashMap<ServerKey, PreEditSnapshot> = self
            .diagnostics
            .entries_for_file(&lookup_path)
            .into_iter()
            .map(|(key, entry)| {
                (
                    key.clone(),
                    PreEditSnapshot {
                        epoch: entry.epoch,
                        document_version_at_capture: None,
                    },
                )
            })
            .collect();

        for (key, store) in &self.documents {
            if let Some(version) = store.version(&lookup_path) {
                snapshots
                    .entry(key.clone())
                    .or_default()
                    .document_version_at_capture = Some(version);
            }
        }

        snapshots
    }

    /// True when the current diagnostic entry for `server_key` can be tied to
    /// that server's current in-memory document version for `file_path`.
    ///
    /// File-mode `lsp_diagnostics` uses this for push-only fallback after it
    /// has synced/opened the document. Versioned publishes are accepted when
    /// they match the current document version; unversioned publishes are not
    /// accepted as fresh because epoch/wall-clock ordering alone is racy.
    pub fn diagnostic_entry_is_fresh_for_document(
        &self,
        file_path: &Path,
        server_key: &ServerKey,
        pre: PreEditSnapshot,
    ) -> bool {
        let lookup_path = normalize_lookup_path(file_path);
        let Some(entry) = self
            .diagnostics
            .entries_for_file(&lookup_path)
            .into_iter()
            .find_map(|(key, entry)| if key == server_key { Some(entry) } else { None })
        else {
            return false;
        };

        if entry.stale {
            return false;
        }

        let target_version = self
            .documents
            .get(server_key)
            .and_then(|store| store.version(&lookup_path))
            .or(pre.document_version_at_capture)
            .unwrap_or(0);

        matches!(entry.version, Some(version) if version >= target_version)
    }

    /// Prepare a post-edit wait and subscribe it before the manager mutex is
    /// released. The subscription closes the race between the state snapshot
    /// and a concurrent event drain.
    pub(crate) fn start_post_edit_diagnostics_wait(
        &mut self,
        file_path: &Path,
        expected_versions: &[(ServerKey, i32)],
        pre_snapshot: &HashMap<ServerKey, PreEditSnapshot>,
        timeout: std::time::Duration,
    ) -> PostEditDiagnosticsWait {
        let lookup_path = normalize_lookup_path(file_path);

        // Events sent after didChange may already be queued. Handle them before
        // parking, while freshness checks still reject pre-edit publications.
        let _ = self.drain_events_for_file(&lookup_path);

        let waiter_id = self.next_post_edit_waiter_id;
        self.next_post_edit_waiter_id = self.next_post_edit_waiter_id.wrapping_add(1);
        let (wake_tx, wake_rx) = bounded(1);
        self.post_edit_waiters.insert(waiter_id, wake_tx);

        PostEditDiagnosticsWait {
            lookup_path,
            expected_versions: expected_versions.to_vec(),
            pre_snapshot: pre_snapshot.clone(),
            responses_at_start: expected_versions
                .iter()
                .filter_map(|(key, _)| {
                    self.clients
                        .get(key)
                        .map(|client| (key.clone(), client.received_message_count()))
                })
                .collect(),
            event_rx: self.event_rx.clone(),
            wake_rx,
            waiter_id,
            deadline: std::time::Instant::now() + timeout,
            fresh: HashMap::new(),
            exited: Vec::new(),
        }
    }

    pub(crate) fn poll_post_edit_diagnostics_wait(
        &mut self,
        wait: &mut PostEditDiagnosticsWait,
        event: Option<LspEvent>,
    ) -> bool {
        if let Some(event) = event {
            self.handle_event(&event);
        }

        for (key, target_version) in &wait.expected_versions {
            if wait.fresh.contains_key(key) || wait.exited.contains(key) {
                continue;
            }
            if !self.clients.contains_key(key) {
                wait.exited.push(key.clone());
                continue;
            }
            if self
                .clients
                .get(key)
                .is_some_and(LspClient::is_unresponsive)
            {
                continue;
            }
            if let Some(entry) = self
                .diagnostics
                .entries_for_file(&wait.lookup_path)
                .into_iter()
                .find_map(|(stored_key, entry)| (stored_key == key).then_some(entry))
            {
                let pre = wait.pre_snapshot.get(key).copied().unwrap_or_default();
                if let Some(diagnostics) =
                    Self::authoritative_post_edit_diagnostics(entry, *target_version, pre)
                {
                    wait.fresh.insert(key.clone(), diagnostics);
                }
            }
        }

        wait.fresh.len()
            + wait.exited.len()
            + wait
                .expected_versions
                .iter()
                .filter(|(key, _)| {
                    !wait.fresh.contains_key(key)
                        && !wait.exited.contains(key)
                        && self
                            .clients
                            .get(key)
                            .is_some_and(LspClient::is_unresponsive)
                })
                .count()
            == wait.expected_versions.len()
    }

    pub(crate) fn finish_post_edit_diagnostics_wait(
        &mut self,
        wait: PostEditDiagnosticsWait,
    ) -> PostEditWaitOutcome {
        self.post_edit_waiters.remove(&wait.waiter_id);
        for (key, _) in &wait.expected_versions {
            if wait.fresh.contains_key(key) || wait.exited.contains(key) {
                continue;
            }
            // A provisional or unversioned report still proves the server
            // answered. Do not quarantine it merely for lacking authority.
            let answered = self
                .diagnostics
                .entries_for_file(&wait.lookup_path)
                .into_iter()
                .any(|(stored_key, entry)| {
                    stored_key == key
                        && entry.epoch
                            > wait
                                .pre_snapshot
                                .get(key)
                                .copied()
                                .unwrap_or_default()
                                .epoch
                });
            if !answered {
                if let (Some(client), Some(observed)) =
                    (self.clients.get(key), wait.responses_at_start.get(key))
                {
                    client.mark_unresponsive_if_silent(*observed);
                }
            }
        }
        let unresponsive_servers = wait
            .expected_versions
            .iter()
            .filter_map(|(key, _)| {
                if wait.fresh.contains_key(key) || wait.exited.contains(key) {
                    return None;
                }
                self.clients
                    .get(key)
                    .filter(|client| client.is_unresponsive())
                    .map(|_| key.clone())
            })
            .collect();
        let mut outcome = Self::post_edit_outcome(wait.expected_versions, wait.fresh, wait.exited);
        outcome.unresponsive_servers = unresponsive_servers;
        outcome
    }

    /// Register a waiter that blocks on language-server events without the
    /// manager lock (see [`LspEventWait`]). Call [`Self::unsubscribe_events`]
    /// when done.
    pub(crate) fn subscribe_events(&mut self) -> LspEventWait {
        let waiter_id = self.next_post_edit_waiter_id;
        self.next_post_edit_waiter_id = self.next_post_edit_waiter_id.wrapping_add(1);
        let (wake_tx, wake_rx) = bounded(1);
        self.post_edit_waiters.insert(waiter_id, wake_tx);
        LspEventWait {
            event_rx: self.event_rx.clone(),
            wake_rx,
            waiter_id,
        }
    }

    /// Handle an event an [`LspEventWait`] took off the channel, then drain
    /// whatever else is queued.
    pub(crate) fn handle_waited_event(&mut self, event: Option<LspEvent>) {
        if let Some(event) = event {
            self.handle_event(&event);
        }
        self.drain_events();
    }

    pub(crate) fn unsubscribe_events(&mut self, wait: LspEventWait) {
        self.post_edit_waiters.remove(&wait.waiter_id);
    }

    /// The next moment a rust-analyzer server's check state (or a due save)
    /// can change without the server sending anything: a deferred save
    /// falling due, a check start grace ending, or the publish settle window
    /// closing. A waiter sleeps until the earlier of this and its next event.
    pub(crate) fn rust_check_next_timed_change(&self, server_key: &ServerKey) -> Option<Instant> {
        self.clients.get(server_key).and_then(|client| {
            client.rust_check_next_timed_change(Instant::now(), FLYCHECK_PUBLISH_SETTLE)
        })
    }

    /// Wait for fresh per-server diagnostics matching the just-sent document
    /// version. Cached pre-edit entries and provisional warming reports remain
    /// pending; a server exit is reported separately.
    ///
    /// `AppContext` uses the prepare/poll/finish methods directly so channel
    /// waiting happens without its manager mutex. This convenience method keeps
    /// the same behavior for standalone manager callers.
    pub fn wait_for_post_edit_diagnostics(
        &mut self,
        file_path: &Path,
        // `config` is intentionally accepted (matches sibling wait APIs and
        // future-proofs us if freshness rules need it). Currently unused
        // because expected_versions/pre_snapshot fully determine behavior.
        _config: &Config,
        expected_versions: &[(ServerKey, i32)],
        pre_snapshot: &HashMap<ServerKey, PreEditSnapshot>,
        timeout: std::time::Duration,
    ) -> PostEditWaitOutcome {
        let mut wait = self.start_post_edit_diagnostics_wait(
            file_path,
            expected_versions,
            pre_snapshot,
            timeout,
        );
        let mut complete = self.poll_post_edit_diagnostics_wait(&mut wait, None);

        while !complete && !wait.deadline_reached() {
            let event = wait.next_event();
            complete = self.poll_post_edit_diagnostics_wait(&mut wait, event);
        }

        self.finish_post_edit_diagnostics_wait(wait)
    }

    fn post_edit_outcome(
        mut expected: Vec<(ServerKey, i32)>,
        mut fresh: HashMap<ServerKey, Vec<StoredDiagnostic>>,
        exited: Vec<ServerKey>,
    ) -> PostEditWaitOutcome {
        expected.sort_by(|(left, _), (right, _)| server_key_sort(left, right));

        let mut accepted_snapshots = Vec::new();
        let mut pending_servers = Vec::new();
        for (server_key, document_version) in expected {
            if let Some(diagnostics) = fresh.remove(&server_key) {
                accepted_snapshots.push(AcceptedDiagnosticSnapshot::new(
                    server_key,
                    document_version,
                    diagnostics,
                ));
            } else if !exited.contains(&server_key) {
                pending_servers.push(server_key);
            }
        }

        let mut diagnostics = accepted_snapshots
            .iter()
            .flat_map(|snapshot| snapshot.diagnostics.iter().cloned())
            .collect::<Vec<_>>();
        diagnostics.sort_by(|left, right| {
            left.file
                .cmp(&right.file)
                .then(left.line.cmp(&right.line))
                .then(left.column.cmp(&right.column))
                .then(left.message.cmp(&right.message))
        });

        PostEditWaitOutcome {
            accepted_snapshots,
            diagnostics,
            pending_servers,
            unresponsive_servers: Vec::new(),
            exited_servers: exited,
        }
    }

    /// Wait for diagnostics to arrive for a specific file until a deadline.
    ///
    /// Drains already-queued events first, then blocks on the shared event
    /// channel only until either `publishDiagnostics` arrives for this file or
    /// the deadline is reached.
    pub fn wait_for_file_diagnostics(
        &mut self,
        file_path: &Path,
        config: &Config,
        deadline: std::time::Instant,
    ) -> Vec<StoredDiagnostic> {
        let lookup_path = normalize_lookup_path(file_path);

        if self.server_key_for_file(&lookup_path, config).is_none() {
            return Vec::new();
        }

        loop {
            if self.drain_events_for_file(&lookup_path) {
                break;
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }

            let timeout = deadline.saturating_duration_since(now);
            match self.event_rx.recv_timeout(timeout) {
                Ok(event) => {
                    if matches!(
                        self.handle_event(&event),
                        Some(ref published_file) if published_file.as_path() == lookup_path.as_path()
                    ) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        self.get_diagnostics_for_file(&lookup_path)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Default timeout for `textDocument/diagnostic` (per-file pull). Servers
    /// usually respond in under 1s for files they've already analyzed; we
    /// allow up to 10s before falling back to push semantics. Currently
    /// surfaced via [`Self::pull_file_timeout`] for callers that want to
    /// override the wait via the `wait_ms` knob.
    pub const PULL_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Public accessor so command handlers can reuse the documented default.
    pub fn pull_file_timeout() -> std::time::Duration {
        Self::PULL_FILE_TIMEOUT
    }

    /// Default timeout for `workspace/diagnostic`. The LSP spec allows the
    /// server to hold this open indefinitely; we cap at 10s and report
    /// `complete: false` to the agent rather than hanging the bridge.
    const PULL_WORKSPACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Issue a `textDocument/diagnostic` (LSP 3.17 per-file pull) request to
    /// every server that supports pull diagnostics for the given file.
    ///
    /// Returns the per-server outcome. If a server reports `kind: "unchanged"`,
    /// the cached entry's diagnostics are surfaced (deterministic re-use of
    /// the previous response). If a server doesn't advertise pull capability,
    /// it's skipped here — the caller should fall back to push for those.
    ///
    /// Side effects: results are stored in `DiagnosticsStore` so directory-mode
    /// queries can aggregate them later.
    pub fn pull_file_diagnostics(
        &mut self,
        file_path: &Path,
        config: &Config,
    ) -> Result<Vec<PullFileResult>, LspError> {
        self.pull_file_diagnostics_inner(file_path, config, None)
    }

    /// Pull diagnostics within a caller-owned wait budget. This is used after
    /// edits because a pull-capable server may disable push diagnostics after
    /// seeing the client's LSP 3.17 diagnostic capability.
    pub fn pull_file_diagnostics_with_timeout(
        &mut self,
        file_path: &Path,
        config: &Config,
        timeout: Duration,
    ) -> Result<Vec<PullFileResult>, LspError> {
        self.pull_file_diagnostics_inner(file_path, config, Some(Instant::now() + timeout))
    }

    fn pull_file_diagnostics_inner(
        &mut self,
        file_path: &Path,
        config: &Config,
        deadline: Option<Instant>,
    ) -> Result<Vec<PullFileResult>, LspError> {
        let pulls = self.begin_file_pulls(file_path, config, deadline)?;
        Ok(pulls
            .into_iter()
            .map(|pull| {
                let server_key = pull.key.clone();
                let opened_for_pull = pull.opened_for_pull;
                let outcome = self.finish_document_pull(pull.wait());
                PullFileResult {
                    server_key,
                    outcome,
                    opened_for_pull,
                }
            })
            .collect())
    }

    /// Make sure the file's servers are running and have the document open
    /// with current content, then send a pull request to each server that
    /// supports it. The replies are waited for separately (see
    /// [`DocumentPull::wait`]), so the caller can release the manager lock.
    pub(crate) fn begin_file_pulls(
        &mut self,
        file_path: &Path,
        config: &Config,
        deadline: Option<Instant>,
    ) -> Result<Vec<DocumentPull>, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        // Handles disk drift via DocumentStore::is_stale_on_disk.
        let opened = self.ensure_file_open(&canonical_path, config)?;
        let uri = uri_for_path(&canonical_path)?;
        Ok(opened
            .server_keys
            .into_iter()
            .map(|key| {
                let opened_for_pull = opened.newly_opened.contains(&key);
                let mut pull = self.begin_open_document_pull(key, &canonical_path, &uri, deadline);
                pull.opened_for_pull = opened_for_pull;
                pull
            })
            .collect())
    }

    /// Send one server a pull request for a document that server already has
    /// open. A scoped inspect uses this after opening the document in exactly
    /// the servers its scope started.
    pub(crate) fn begin_document_pull(
        &mut self,
        key: &ServerKey,
        file_path: &Path,
        deadline: Instant,
    ) -> Result<DocumentPull, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let uri = uri_for_path(&canonical_path)?;
        Ok(self.begin_open_document_pull(key.clone(), &canonical_path, &uri, Some(deadline)))
    }

    fn begin_open_document_pull(
        &mut self,
        key: ServerKey,
        canonical_path: &Path,
        uri: &lsp_types::Uri,
        deadline: Option<Instant>,
    ) -> DocumentPull {
        let canonical_path = canonical_path.to_path_buf();
        let document_version = self
            .documents
            .get(&key)
            .and_then(|store| store.version(&canonical_path));
        let mut pull = DocumentPull {
            key,
            canonical_path,
            document_version,
            opened_for_pull: false,
            state: DocumentPullState::Done(PullFileOutcome::PullNotSupported),
        };
        let supports_pull = self
            .clients
            .get(&pull.key)
            .and_then(|c| c.diagnostic_capabilities())
            .is_some_and(|caps| caps.pull_diagnostics);
        if !supports_pull {
            return pull;
        }

        // Look up previous resultId for incremental requests.
        let previous_result_id = self
            .diagnostics
            .entries_for_file(&pull.canonical_path)
            .into_iter()
            .find(|(k, _)| **k == pull.key)
            .and_then(|(_, entry)| entry.result_id.clone());
        let identifier = self
            .clients
            .get(&pull.key)
            .and_then(|c| c.diagnostic_capabilities())
            .and_then(|caps| caps.identifier.clone());
        let params = AftDocumentDiagnosticParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            identifier,
            previous_result_id,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        // Capped below the global request timeout so a stalled pull server
        // cannot consume the entire inspect or post-edit budget.
        let timeout = deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Self::PULL_FILE_TIMEOUT)
            })
            .unwrap_or(Self::PULL_FILE_TIMEOUT);
        slog_info!(
            "lsp_protocol server={} root={} method=textDocument/diagnostic event=sent file={} document_version={}",
            pull.key.kind.id_str(),
            pull.key.root.display(),
            pull.canonical_path.display(),
            pull.document_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "none".to_string())
        );
        let started = match self.clients.get_mut(&pull.key) {
            Some(client) => client.start_request::<AftDocumentDiagnosticRequest>(params),
            None => Err(LspError::ServerNotReady("server not found".into())),
        };
        pull.state = match started {
            Ok(request) => DocumentPullState::InFlight {
                request,
                wait_until: Instant::now() + timeout,
            },
            Err(err) => DocumentPullState::Replied(Err(err)),
        };
        pull
    }

    /// Store the reply of a pull started by [`Self::begin_file_pulls`] or
    /// [`Self::begin_document_pull`] and return its outcome. A pull that was
    /// not waited for yet is waited for here, with the manager locked.
    pub(crate) fn finish_document_pull(&mut self, pull: DocumentPull) -> PullFileOutcome {
        let pull = pull.wait();
        let DocumentPull {
            key,
            canonical_path,
            document_version,
            state,
            ..
        } = pull;
        let reply = match state {
            DocumentPullState::Done(outcome) => return outcome,
            DocumentPullState::Replied(reply) => reply,
            DocumentPullState::InFlight { .. } => unreachable!("DocumentPull::wait replies"),
        };
        let report = reply.and_then(|value| {
            serde_json::from_value::<lsp_types::DocumentDiagnosticReportResult>(value)
                .map_err(Into::into)
        });
        match report {
            Ok(report) => {
                let report_kind = match &report {
                    lsp_types::DocumentDiagnosticReportResult::Report(
                        lsp_types::DocumentDiagnosticReport::Full(_),
                    ) => "full",
                    lsp_types::DocumentDiagnosticReportResult::Report(
                        lsp_types::DocumentDiagnosticReport::Unchanged(_),
                    ) => "unchanged",
                    lsp_types::DocumentDiagnosticReportResult::Partial(_) => "partial",
                };
                slog_info!(
                    "lsp_protocol server={} root={} method=textDocument/diagnostic event=received file={} report_kind={}",
                    key.kind.id_str(),
                    key.root.display(),
                    canonical_path.display(),
                    report_kind
                );
                if matches!(
                    &report,
                    lsp_types::DocumentDiagnosticReportResult::Report(
                        lsp_types::DocumentDiagnosticReport::Full(_)
                    )
                ) {
                    // The server may publish diagnostics for didOpen before
                    // returning a full pull response. Apply those older events
                    // first so the full report remains authoritative. An
                    // unchanged response must inspect only a previous pull cache.
                    self.drain_events();
                }
                self.ingest_document_report(&key, &canonical_path, document_version, report)
            }
            Err(err) => {
                slog_info!(
                    "lsp_protocol server={} root={} method=textDocument/diagnostic event=failed file={} error={}",
                    key.kind.id_str(),
                    key.root.display(),
                    canonical_path.display(),
                    err
                );
                if let Some(result) = self.cache_post_initialize_exit(&key, &err) {
                    PullFileOutcome::RequestFailed {
                        reason: server_attempt_result_reason(&result),
                    }
                } else if recoverable_pull_rejection(&err)
                    && self.clients.get(&key).is_some_and(|client| {
                        matches!(
                            client.state(),
                            ServerState::Ready | ServerState::Initializing
                        )
                    })
                {
                    PullFileOutcome::RequestFailed {
                        reason: format!("pull_rejected_push_fallback: {err}"),
                    }
                } else {
                    PullFileOutcome::RequestFailed {
                        reason: err.to_string(),
                    }
                }
            }
        }
    }

    /// Issue a `workspace/diagnostic` request to a specific server. Cancels
    /// internally if `timeout` elapses before the server responds. Cached
    /// entries from the response are stored so directory-mode queries pick
    /// them up. Waits with `self` borrowed; callers holding the manager lock
    /// should use [`pull_workspace_diagnostics_unlocked`].
    pub fn pull_workspace_diagnostics(
        &mut self,
        server_key: &ServerKey,
        timeout: Option<std::time::Duration>,
    ) -> Result<PullWorkspaceResult, LspError> {
        let timeout = timeout.unwrap_or(Self::PULL_WORKSPACE_TIMEOUT);
        match self.begin_workspace_pull(server_key)? {
            None => Ok(unsupported_workspace_pull(server_key)),
            Some(request) => {
                let reply = request.wait(timeout);
                self.finish_workspace_pull(server_key, reply)
            }
        }
    }

    /// Send a `workspace/diagnostic` request; `None` when the server does not
    /// support it.
    pub(crate) fn begin_workspace_pull(
        &mut self,
        server_key: &ServerKey,
    ) -> Result<Option<crate::lsp::client::PendingLspRequest>, LspError> {
        let supports_workspace = self
            .clients
            .get(server_key)
            .and_then(|c| c.diagnostic_capabilities())
            .is_some_and(|caps| caps.workspace_diagnostics);
        if !supports_workspace {
            return Ok(None);
        }

        let identifier = self
            .clients
            .get(server_key)
            .and_then(|c| c.diagnostic_capabilities())
            .and_then(|caps| caps.identifier.clone());

        // Report the result ids of current reports so the server can answer
        // "unchanged" for those files instead of resending every report;
        // an unchanged item leaves the stored report as it is.
        let previous_result_ids = self
            .diagnostics
            .result_ids_for_server(server_key)
            .into_iter()
            .filter_map(|(file, value)| {
                Some(lsp_types::PreviousResultId {
                    uri: uri_for_path(&file).ok()?,
                    value,
                })
            })
            .collect();
        let params = AftWorkspaceDiagnosticParams {
            identifier,
            previous_result_ids,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        self.clients
            .get_mut(server_key)
            .ok_or_else(|| LspError::ServerNotReady("server not found".into()))?
            .start_request::<AftWorkspaceDiagnosticRequest>(params)
            .map(Some)
    }

    /// Store the reply to a request sent by [`Self::begin_workspace_pull`].
    pub(crate) fn finish_workspace_pull(
        &mut self,
        server_key: &ServerKey,
        reply: Result<serde_json::Value, LspError>,
    ) -> Result<PullWorkspaceResult, LspError> {
        let result = match reply.and_then(|value| {
            serde_json::from_value::<lsp_types::WorkspaceDiagnosticReportResult>(value)
                .map_err(Into::into)
        }) {
            Ok(result) => result,
            Err(LspError::Timeout(_)) => {
                return Ok(PullWorkspaceResult {
                    server_key: server_key.clone(),
                    files_reported: Vec::new(),
                    complete: false,
                    cancelled: true,
                    supports_workspace: true,
                });
            }
            Err(err) => {
                if let Some(result) = self.cache_post_initialize_exit(server_key, &err) {
                    return Err(LspError::ServerNotReady(server_attempt_result_reason(
                        &result,
                    )));
                }
                return Err(err);
            }
        };

        // Extract the items list. Partial responses are not a complete
        // workspace view, but the partial payload can still contain useful
        // document reports; ingest those while surfacing complete=false.
        let (items, complete) = match result {
            lsp_types::WorkspaceDiagnosticReportResult::Report(report) => (report.items, true),
            lsp_types::WorkspaceDiagnosticReportResult::Partial(partial) => (partial.items, false),
        };

        // Ingest each file report into the diagnostics store.
        let mut files_reported = Vec::with_capacity(items.len());
        for item in items {
            match item {
                lsp_types::WorkspaceDocumentDiagnosticReport::Full(full) => {
                    if let Some(file) = uri_to_path(&full.uri) {
                        let stored = from_lsp_diagnostics(
                            file.clone(),
                            full.full_document_diagnostic_report.items.clone(),
                            &server_key.kind,
                        );
                        self.diagnostics.publish_with_result_id(
                            server_key.clone(),
                            file.clone(),
                            stored,
                            full.full_document_diagnostic_report.result_id.clone(),
                        );
                        files_reported.push(file);
                    }
                }
                lsp_types::WorkspaceDocumentDiagnosticReport::Unchanged(_unchanged) => {
                    // "Unchanged" means the previously cached report is still
                    // valid. We left it in place; nothing to do.
                }
            }
        }

        Ok(PullWorkspaceResult {
            server_key: server_key.clone(),
            files_reported,
            complete,
            cancelled: false,
            supports_workspace: true,
        })
    }

    fn cache_post_initialize_exit(
        &mut self,
        key: &ServerKey,
        err: &LspError,
    ) -> Option<ServerAttemptResult> {
        let binary = self
            .server_binaries
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.kind.id_str().to_string());
        let report = {
            let client = self.clients.get_mut(key)?;
            let status = client.wait_for_exit(Duration::from_millis(100))?;
            client.exit_report(client.phase(), Some(status), false)
        };
        let reason = format_post_initialize_exit_reason(&binary, &report, err);
        let durability = exit_durability(&reason, &report.stderr_tail);
        let result = ServerAttemptResult::SpawnFailed { binary, reason };
        self.clients.remove(key);
        self.remember_exit_report(report);
        self.server_binaries.remove(key);
        self.documents.remove(key);
        self.diagnostics.clear_for_server(key);
        Some(self.record_failed_spawn(key, result, durability))
    }

    /// Store the result of a per-file pull request and return a structured
    /// outcome the caller can inspect.
    fn ingest_document_report(
        &mut self,
        key: &ServerKey,
        canonical_path: &Path,
        document_version: Option<i32>,
        result: lsp_types::DocumentDiagnosticReportResult,
    ) -> PullFileOutcome {
        let report = match result {
            lsp_types::DocumentDiagnosticReportResult::Report(report) => report,
            lsp_types::DocumentDiagnosticReportResult::Partial(_) => {
                // Partial results stream in via $/progress notifications which
                // we don't currently subscribe to. Treat as a soft-empty
                // success — the next pull will get the full version.
                return PullFileOutcome::PartialNotSupported;
            }
        };

        match report {
            lsp_types::DocumentDiagnosticReport::Full(full) => {
                let result_id = full.full_document_diagnostic_report.result_id.clone();
                let stored = from_lsp_diagnostics(
                    canonical_path.to_path_buf(),
                    full.full_document_diagnostic_report.items.clone(),
                    &key.kind,
                );
                let count = stored.len();
                let provisional = self
                    .clients
                    .get(key)
                    .is_some_and(|client| client.diagnostics_are_provisional());
                self.diagnostics.publish_full_with_provisional(
                    key.clone(),
                    canonical_path.to_path_buf(),
                    stored,
                    result_id,
                    document_version,
                    provisional,
                );
                PullFileOutcome::Full {
                    diagnostic_count: count,
                }
            }
            lsp_types::DocumentDiagnosticReport::Unchanged(_unchanged) => {
                // The server says the previous resultId is still valid for the
                // current document. That is only usable if we already have a
                // report for this exact server/file; an initial `unchanged`
                // response cannot prove freshness. A stale watcher entry is
                // acceptable here because the pull response itself proves the
                // cached diagnostics still describe the now-synced file.
                if self
                    .diagnostics
                    .has_report_for_server_file(key, canonical_path)
                {
                    if let Some(version) = document_version {
                        self.diagnostics.confirm_for_server_file_version(
                            key,
                            canonical_path,
                            version,
                        );
                    } else {
                        self.diagnostics
                            .mark_fresh_for_server_file(key, canonical_path);
                    }
                    let authoritative = self
                        .clients
                        .get(key)
                        .map_or(true, |client| !client.diagnostics_are_provisional());
                    if authoritative {
                        self.diagnostics
                            .clear_provisional_for_server_file(key, canonical_path);
                    }
                    PullFileOutcome::Unchanged
                } else {
                    PullFileOutcome::RequestFailed {
                        reason: "no_cache_for_unchanged".to_string(),
                    }
                }
            }
        }
    }

    /// Drain every client and clear spawn/document/diagnostic state without
    /// waiting on child processes. The caller owns graceful shutdown so the
    /// manager lock is not held across a Shutdown handshake.
    pub fn take_all_clients(&mut self) -> Vec<(ServerKey, LspClient)> {
        let clients: Vec<_> = self.clients.drain().collect();
        self.clients_generation = self.clients_generation.wrapping_add(1);
        self.server_binaries.clear();
        self.documents.clear();
        self.diagnostics = DiagnosticsStore::new();
        clients
    }

    /// Shut down every server concurrently within one shared grace period.
    pub fn shutdown_all(&mut self) -> LspShutdownAllOutcome {
        let clients = self.take_all_clients();
        Self::shutdown_taken_clients(clients, self.child_registry.clone())
    }

    /// Shut down clients collected from every root under one process-wide deadline.
    /// The registry also terminates server process groups whose replies never arrive.
    pub fn shutdown_taken_clients(
        clients: Vec<(ServerKey, LspClient)>,
        child_registry: LspChildRegistry,
    ) -> LspShutdownAllOutcome {
        Self::shutdown_all_clients(clients, child_registry, LSP_SHUTDOWN_ALL_BUDGET)
    }

    fn shutdown_all_clients(
        clients: Vec<(ServerKey, LspClient)>,
        child_registry: LspChildRegistry,
        budget: Duration,
    ) -> LspShutdownAllOutcome {
        let started = Instant::now();
        let servers = clients.len();
        let mut pending_pids = clients
            .iter()
            .map(|(_, client)| client.child_pid())
            .collect::<HashSet<_>>();
        let (result_tx, result_rx) = unbounded();

        for (key, mut client) in clients {
            let pid = client.child_pid();
            let result_tx = result_tx.clone();
            std::thread::spawn(move || {
                let result = client.shutdown();
                // Do the drop before reporting completion so the registry cannot
                // briefly report a reaped client as still live after this method
                // has returned its shutdown summary. The drop also waits for the
                // child, so a report means the process has been reaped.
                drop(client);
                let _ = result_tx.send((key, pid, result));
            });
        }
        drop(result_tx);

        // Graceful handshakes get the budget minus the forced-termination
        // reserve; whatever is still running then is killed, and the reserve
        // is the only wait for those kills to take effect. Either way the
        // phase ends by `deadline`.
        let deadline = started + budget;
        let force_at = deadline - LSP_FORCED_TERMINATION_RESERVE.min(budget / 2);
        let mut outcome = LspShutdownAllOutcome::default();
        while !pending_pids.is_empty() {
            let remaining = force_at.saturating_duration_since(Instant::now());
            match result_rx.recv_timeout(remaining) {
                Ok((key, pid, result)) => {
                    if !pending_pids.remove(&pid) {
                        continue;
                    }
                    match result {
                        Ok(()) => outcome.graceful += 1,
                        Err(err) => {
                            outcome.forced += 1;
                            slog_error!("error shutting down {:?}: {}", key, err);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }

        if !pending_pids.is_empty() {
            let pids = pending_pids.into_iter().collect::<Vec<_>>();
            outcome.forced += pids.len();
            // The graceful workers are still blocked in their per-client
            // Shutdown request, so the shared registry kills the process
            // groups directly. The kill cannot be ignored and does not wait;
            // a killed server closes its pipes, which unblocks its worker,
            // and the worker then reaps it and reports in.
            child_registry.force_kill_pids(&pids);
            let mut unreaped = pids.into_iter().collect::<HashSet<_>>();
            while !unreaped.is_empty() {
                // A zero timeout still takes a report that is already queued,
                // so a late wake-up counts every server reaped in the meantime.
                let remaining = deadline.saturating_duration_since(Instant::now());
                match result_rx.recv_timeout(remaining) {
                    Ok((_, pid, _)) => {
                        unreaped.remove(&pid);
                    }
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
            // Waiting longer would break the ceiling. Each of these was sent a
            // kill for its whole process group; its worker thread reaps it if
            // this process keeps running, and the system does if it exits.
            outcome.unreaped = unreaped.len();
        }

        outcome.elapsed = started.elapsed();
        slog_info!(
            "lsp shutdown_all: servers={} graceful={} killed={} unreaped={} elapsed_ms={}",
            servers,
            outcome.graceful,
            outcome.forced,
            outcome.unreaped,
            outcome.elapsed.as_millis()
        );
        outcome
    }

    /// Shut down taken clients on a detached thread named `aft-lsp-idle-reap`.
    /// Idle reapers use this so a hung Shutdown handshake cannot stall the
    /// daemon loop or hold the manager mutex.
    pub(crate) fn spawn_idle_lsp_reap(clients: Vec<(ServerKey, LspClient)>) {
        if clients.is_empty() {
            return;
        }
        let spawn_result = std::thread::Builder::new()
            .name("aft-lsp-idle-reap".into())
            .spawn(move || {
                for (key, mut client) in clients {
                    if let Err(err) = client.shutdown_for_idle_reap() {
                        slog_error!("error shutting down {:?}: {}", key, err);
                    }
                }
            });
        if let Err(err) = spawn_result {
            slog_error!("failed to spawn idle LSP reap thread: {err}");
        }
    }

    /// Check if any server is active.
    pub fn has_active_servers(&self) -> bool {
        self.clients
            .values()
            .any(|client| client.state() == ServerState::Ready)
    }

    /// Report the SDK selected when the server started; resolving it again
    /// could describe a different installation than the running client uses.
    pub fn runtime_notes(&self) -> Vec<String> {
        let mut notes: Vec<_> = self
            .clients
            .values()
            .filter_map(|client| client.runtime_note.clone())
            .collect();
        notes.sort();
        notes.dedup();
        notes
    }

    /// Active server keys (running clients). Used by `lsp_diagnostics`
    /// directory mode to know which servers to ask for workspace pull.
    pub fn active_server_keys(&self) -> Vec<ServerKey> {
        self.clients.keys().cloned().collect()
    }

    /// Return the last watched-file routing decision for Windows CI timeout
    /// diagnostics. The trace records whether a matching client was absent,
    /// outside the changed path's root, unsupported, or successfully written.
    #[cfg(windows)]
    #[doc(hidden)]
    pub fn watched_file_notification_trace_for_test(&self) -> &str {
        &self.last_watched_file_notification_trace
    }

    pub fn get_diagnostics_for_file(&self, file: &Path) -> Vec<&StoredDiagnostic> {
        let normalized = normalize_lookup_path(file);
        self.diagnostics.for_file(&normalized)
    }

    pub fn get_diagnostics_for_file_with_provisional(
        &self,
        file: &Path,
    ) -> Vec<(&StoredDiagnostic, bool)> {
        let normalized = normalize_lookup_path(file);
        self.diagnostics.for_file_with_provisional(&normalized)
    }

    /// Drop all cached diagnostics for a file across every server. Called when a
    /// file is deleted/renamed away so its diagnostics don't linger in the warm
    /// set (no server republishes for a vanished path), inflating the
    /// error/warning counts in the status bar and `aft_inspect`.
    ///
    /// The store key is the canonical path from publish time, but a deleted file
    /// can no longer be canonicalized directly (`canonicalize` needs the file to
    /// exist). We therefore try several equivalent forms: the raw path, the
    /// canonicalize-or-fallback form, and — crucially — a reconstruction that
    /// canonicalizes the still-present parent directory and rejoins the file
    /// name, which reproduces the publish-time key even across `/var`↔
    /// `/private/var`-style symlink aliasing. Returns true if anything was
    /// removed.
    /// Forget all cached spawn FAILURES so the next file event retries them.
    /// Called on `configure`: a configure means something changed (the user may
    /// have just installed the missing language server, or fixed PATH / a
    /// version pin), so a previously-failed (kind, root) pair deserves a fresh
    /// attempt instead of being skipped until a full restart. Bounded: configure
    /// is not a per-request hot path, so this cannot cause a spawn storm.
    /// Returns the number of cleared entries.
    pub fn clear_failed_spawns(&mut self) -> usize {
        let n = self.failed_spawns.len();
        self.failed_spawns.clear();
        self.transient_backoff.clear();
        n
    }

    #[cfg(test)]
    pub(crate) fn insert_failed_spawn_for_test(&mut self) {
        let key = ServerKey {
            kind: crate::lsp::registry::ServerKind::Rust,
            root: std::path::PathBuf::from("/tmp/test-root"),
        };
        self.failed_spawns.insert(
            key,
            FailedSpawn {
                result: ServerAttemptResult::SpawnFailed {
                    binary: "rust-analyzer".to_string(),
                    reason: "test".to_string(),
                },
                retry_at: None,
            },
        );
    }

    pub fn clear_diagnostics_for_file(&mut self, file: &Path) -> bool {
        diagnostic_path_candidates(file)
            .into_iter()
            .fold(false, |removed, candidate| {
                removed | self.diagnostics.clear_for_file(&candidate)
            })
    }

    /// Mark cached diagnostics for this file stale after a watcher-observed
    /// external edit. The same path aliases as deletion are checked so canonical
    /// publish keys are found even when the watcher reports a symlinked path.
    pub fn mark_diagnostics_stale_for_file(&mut self, file: &Path) -> StaleDiagnosticsMark {
        let mut result = StaleDiagnosticsMark::default();
        for candidate in diagnostic_path_candidates(file) {
            // The file changed outside AFT: what inspect collected no longer
            // describes it, so the server's publishes are current again.
            self.end_inspect_close_exception(&candidate);
            let (had_entries, changed) = self.diagnostics.mark_stale_for_file(&candidate);
            result.had_entries |= had_entries;
            result.changed |= changed;
        }
        result
    }

    pub fn get_diagnostics_for_directory(&self, dir: &Path) -> Vec<&StoredDiagnostic> {
        let normalized = normalize_lookup_path(dir);
        self.diagnostics.for_directory(&normalized)
    }

    pub fn get_diagnostics_for_directory_with_provisional(
        &self,
        dir: &Path,
    ) -> Vec<(&StoredDiagnostic, bool)> {
        let normalized = normalize_lookup_path(dir);
        self.diagnostics.for_directory_with_provisional(&normalized)
    }

    pub(crate) fn authoritative_diagnostic_reports(
        &self,
    ) -> impl Iterator<Item = (&ServerKey, &Path, &[StoredDiagnostic])> {
        self.diagnostics.authoritative_reports()
    }

    /// Failed starts leave the project-wide diagnostic total unknown even when
    /// another server has published a clean report.
    pub(crate) fn has_failed_diagnostic_producers(&self, root: Option<&Path>) -> bool {
        self.failed_spawns.keys().any(|key| {
            root.is_none_or(|root| key.root.starts_with(root) || root.starts_with(&key.root))
        })
    }

    pub fn get_all_diagnostics(&self) -> Vec<&StoredDiagnostic> {
        self.diagnostics.all()
    }

    pub fn get_all_diagnostics_with_provisional(&self) -> Vec<(&StoredDiagnostic, bool)> {
        self.diagnostics.all_with_provisional()
    }

    /// True if any LSP server has a current diagnostic report, including an
    /// empty report that proves a checked-clean file. This lets callers avoid
    /// treating an empty flattened diagnostic list as trustworthy when no server
    /// has actually run or every report was marked stale after an external edit.
    pub fn has_any_diagnostic_reports(&self) -> bool {
        self.diagnostics.has_any_fresh_report()
    }

    /// True if any server has a current report for this file, including an
    /// empty checked-clean report. Watcher-stale reports are excluded because
    /// they predate an external edit.
    pub fn has_diagnostic_report_for_file(&self, file: &Path) -> bool {
        let normalized = normalize_lookup_path(file);
        self.diagnostics.has_any_fresh_report_for_file(&normalized)
    }

    /// True if this exact server/file pair has a current diagnostic report,
    /// including an empty checked-clean report. Watcher-stale reports are
    /// excluded because they predate an external edit.
    pub fn has_diagnostic_report_for_server_file(&self, server: &ServerKey, file: &Path) -> bool {
        let normalized = normalize_lookup_path(file);
        self.diagnostics
            .has_fresh_report_for_server_file(server, &normalized)
    }

    /// True if any server has an authoritative report for this file: neither
    /// watcher-stale nor warming-provisional, including an empty checked-clean
    /// report. Needed for per-file diagnostic authority: a global "some server
    /// reported" bit cannot prove that a specific file was analyzed.
    pub fn has_authoritative_report_for_file(&self, file: &Path) -> bool {
        let normalized = normalize_lookup_path(file);
        self.diagnostics
            .has_authoritative_report_for_file(&normalized)
    }

    /// True if this server instance holds any authoritative report: an entry
    /// that is neither watcher-stale nor warming-provisional, including an
    /// empty checked-clean report. Needed by the blocking quiescence wait, which
    /// settles per producer rather than per file.
    pub fn has_authoritative_report_for_server(&self, server: &ServerKey) -> bool {
        self.diagnostics.has_authoritative_report_for_server(server)
    }

    /// True if this server instance is still warming: its reports stay
    /// provisional until it declares quiescence. A server the manager does not
    /// know (never started, or exited since) is not warming and cannot block a
    /// quiescence wait.
    pub fn server_is_warming(&self, server: &ServerKey) -> bool {
        self.clients
            .get(server)
            .is_some_and(|client| client.diagnostics_are_provisional())
    }

    /// A producer has settled when it holds a current authoritative report or
    /// has stopped warming. The blocking inspect wait and the unscoped
    /// diagnostics freshness gate both use this predicate so a complete
    /// producer set cannot be judged incomplete by a second, stricter check.
    pub fn producer_has_settled(&self, server: &ServerKey) -> bool {
        self.producer_failure(server).is_some()
            || self.has_authoritative_report_for_server(server)
            || !self.server_is_warming(server)
    }

    /// A terminal analysis failure is settled but cannot certify clean diagnostics.
    pub fn producer_failure(&self, server: &ServerKey) -> Option<&str> {
        self.clients
            .get(server)
            .and_then(LspClient::diagnostic_failure)
    }

    /// Non-fatal analyzer status to show alongside the producer's diagnostics.
    pub(crate) fn producer_warning(&self, server: &ServerKey) -> Option<&str> {
        self.clients
            .get(server)
            .and_then(|client| client.rust_analyzer_warning.as_deref())
    }

    /// True when every expected producer has settled. Empty input is vacuously
    /// true; callers that mean "no producer was started" must not treat that
    /// as a fresh diagnostics collection.
    pub fn producers_settled(&self, expected: &[ServerKey]) -> bool {
        expected
            .iter()
            .all(|server| self.producer_has_settled(server))
    }

    fn drain_events_for_file(&mut self, file_path: &Path) -> bool {
        let mut saw_file_diagnostics = false;
        while let Ok(event) = self.event_rx.try_recv() {
            if matches!(
                self.handle_event(&event),
                Some(ref published_file) if published_file.as_path() == file_path
            ) {
                saw_file_diagnostics = true;
            }
        }
        saw_file_diagnostics
    }

    /// The snapshot of a publish `handle_event` just stored for `file`, when
    /// it is an accepted live report.
    fn accepted_live_publish_snapshot(
        &self,
        event: &LspEvent,
        file: &Path,
    ) -> Option<AcceptedDiagnosticSnapshot> {
        let LspEvent::Notification {
            server_kind,
            root,
            method,
            params: Some(_),
        } = event
        else {
            return None;
        };
        if method != "textDocument/publishDiagnostics" {
            return None;
        }
        let server_key = ServerKey {
            kind: server_kind.clone(),
            root: root.clone(),
        };
        if self.live_publish_drop_reason(&server_key, file).is_some() {
            return None;
        }
        let document_version = self.documents.get(&server_key)?.version(file)?;
        let entry = self
            .diagnostics
            .entries_for_file(file)
            .into_iter()
            .find_map(|(stored_key, entry)| (stored_key == &server_key).then_some(entry))?;

        Some(AcceptedDiagnosticSnapshot::new(
            server_key,
            document_version,
            entry.diagnostics.clone(),
        ))
    }

    fn live_publish_drop_reason(&self, key: &ServerKey, file: &Path) -> Option<&'static str> {
        let Some(client) = self.clients.get(key) else {
            return Some("server-not-live");
        };
        if client.state() != ServerState::Ready {
            return Some("server-not-ready");
        }
        if client.diagnostics_are_provisional() {
            return Some("producer-warming");
        }
        let Some(document_version) = self
            .documents
            .get(key)
            .and_then(|documents| documents.version(file))
        else {
            return Some("document-not-open");
        };
        let Some(entry) = self
            .diagnostics
            .entries_for_file(file)
            .into_iter()
            .find_map(|(stored_key, entry)| (stored_key == key).then_some(entry))
        else {
            return Some("report-not-stored");
        };
        if entry.stale {
            return Some("report-stale");
        }
        if entry.provisional {
            return Some("report-provisional");
        }
        match entry.version {
            None => Some("missing-version"),
            Some(version) if version != document_version => Some("version-mismatch"),
            Some(_) => None,
        }
    }

    fn handle_event(&mut self, event: &LspEvent) -> Option<PathBuf> {
        let (LspEvent::Notification {
            server_kind, root, ..
        }
        | LspEvent::ServerRequest {
            server_kind, root, ..
        }
        | LspEvent::ServerExited {
            server_kind, root, ..
        }) = event;
        let event_key = ServerKey {
            kind: server_kind.clone(),
            root: root.clone(),
        };
        if !self.clients.contains_key(&event_key) {
            if let Some(reservation) = self.starting.get_mut(&event_key) {
                reservation.deferred_events.push(event.clone());
                return None;
            }
        }
        let published_file = match event {
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params: Some(params),
            } if method == "textDocument/publishDiagnostics" => {
                self.handle_publish_diagnostics(server_kind.clone(), root.clone(), params)
            }
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params: Some(params),
            } if method == "experimental/serverStatus" => {
                self.handle_server_status(server_kind.clone(), root.clone(), params);
                None
            }
            LspEvent::Notification {
                server_kind: ServerKind::Rust,
                root,
                method,
                params: Some(params),
            } if method == "$/progress" => {
                self.handle_rust_progress(root.clone(), params);
                None
            }
            LspEvent::ServerExited {
                server_kind,
                root,
                pid,
                reason,
            } => {
                let key = ServerKey {
                    kind: server_kind.clone(),
                    root: root.clone(),
                };
                let reason_text = reason.to_string();
                let owned_by_live_client = self
                    .clients
                    .get(&key)
                    .is_some_and(|client| client.child_pid() == *pid);
                if owned_by_live_client {
                    if let Some(mut client) = self.clients.remove(&key) {
                        let report = client.reap_with_report(reason);
                        self.record_exit_log_line(report.log_line(&reason_text));
                    }
                } else if let Some(report) = self.pending_exit_reports.remove(pid) {
                    // This process died while starting (or its failure was
                    // already reported to a caller); its full report was
                    // kept for this moment so the exit is logged once.
                    self.record_exit_log_line(report.log_line(&reason_text));
                } else {
                    self.record_exit_log_line(format!(
                        "exited {:?} {} ({reason_text}): pid={pid} status unavailable (aft had already released this server)",
                        server_kind,
                        root.display()
                    ));
                }
                if self.clients.contains_key(&key) {
                    // A newer server for the same root is running; the event
                    // belonged to an older process and must not tear it down.
                    None
                } else {
                    self.server_binaries.remove(&key);
                    self.documents.remove(&key);
                    self.diagnostics.clear_for_server(&key);
                    self.latest_push_for_pull_servers
                        .retain(|(server, _), _| *server != key);
                    self.latest_pull_for_rust
                        .retain(|(server, _), _| *server != key);
                    self.inspect_closed_documents
                        .retain(|(server, _), _| *server != key);
                    None
                }
            }
            _ => None,
        };
        self.wake_post_edit_waiters();
        published_file
    }

    fn wake_post_edit_waiters(&mut self) {
        Self::wake_waiters(&mut self.post_edit_waiters);
    }

    fn wake_waiters(waiters: &mut HashMap<u64, Sender<()>>) {
        waiters.retain(|_, sender| match sender.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => true,
            Err(TrySendError::Disconnected(())) => false,
        });
    }

    fn handle_publish_diagnostics(
        &mut self,
        server: ServerKind,
        root: PathBuf,
        params: &serde_json::Value,
    ) -> Option<PathBuf> {
        #[cfg(test)]
        PUBLISH_DIAGNOSTICS_PARSES.with(|count| count.set(count.get() + 1));
        // Deserialized from the borrowed payload: cloning it first copied
        // every diagnostic once more.
        let publish_params =
            match <lsp_types::PublishDiagnosticsParams as serde::Deserialize>::deserialize(params) {
                Ok(params) => params,
                Err(err) => {
                    slog_info!(
                    "lsp_protocol server={} root={} method=textDocument/publishDiagnostics event=dropped-because-invalid-params error={}",
                    server.id_str(),
                    root.display(),
                    err
                );
                    return None;
                }
            };
        let Some(file) = uri_to_path(&publish_params.uri) else {
            slog_info!(
                "lsp_protocol server={} root={} method=textDocument/publishDiagnostics event=dropped-because-invalid-uri uri={:?}",
                server.id_str(),
                root.display(),
                publish_params.uri
            );
            return None;
        };
        let diagnostic_count = publish_params.diagnostics.len();
        let version = publish_params.version;
        slog_info!(
            "lsp_protocol server={} root={} method=textDocument/publishDiagnostics event=received file={} version={} diagnostics={}",
            server.id_str(),
            root.display(),
            file.display(),
            version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "none".to_string()),
            diagnostic_count
        );
        let stored = from_lsp_diagnostics(file.clone(), publish_params.diagnostics, &server);
        let key = ServerKey { kind: server, root };
        // Preserve the full compiler result independently of the working-set
        // LRU and document-close exceptions, which must not turn errors clean.
        if let Some(cache) = self
            .clients
            .get_mut(&key)
            .and_then(|c| c.completed_rust_check.as_mut())
        {
            cache.reports.insert(file.clone(), stored.clone());
        }
        let mut stored = stored;
        if key.kind == ServerKind::Rust && self.server_supports_pull(&key) {
            // Recorded before the close check below: rust-analyzer's reply to
            // a close still carries the file's `cargo check` results.
            // An empty push is kept as no entry: a missing entry already
            // reads as "no pushed diagnostics", and storing empty lists kept
            // one entry for every file rust-analyzer ever published.
            if stored.is_empty() {
                self.latest_push_for_pull_servers
                    .remove(&(key.clone(), file.clone()));
            } else {
                self.latest_push_for_pull_servers
                    .insert((key.clone(), file.clone()), stored.clone());
            }
        }
        if self.is_inspect_close_clearing(&key, &file, stored.is_empty()) {
            slog_info!(
                "lsp_protocol server={} root={} method=textDocument/publishDiagnostics event=dropped-because-inspect-closed-document file={} diagnostics={}",
                key.kind.id_str(),
                key.root.display(),
                file.display(),
                diagnostic_count
            );
            return None;
        }
        if let Some(pulled) = self.latest_pull_for_rust.get(&(key.clone(), file.clone())) {
            // A rust-analyzer push in pull mode carries only `cargo check`
            // results; store it together with the pulled diagnostics recorded
            // for the file so storing it does not discard them.
            stored = union_of_diagnostics(pulled.clone(), stored);
        }
        // Store with the real ServerKey and the published document version
        // so observation sources can accept only reports that prove which
        // in-memory document state they describe. The earlier
        // `publish_with_kind` path silently dropped both facts.
        let provisional = self
            .clients
            .get(&key)
            .is_some_and(|client| client.diagnostics_are_provisional());
        self.diagnostics.publish_full_with_provisional(
            key.clone(),
            file.clone(),
            stored,
            None,
            version,
            provisional,
        );
        if let Some(reason) = self.live_publish_drop_reason(&key, &file) {
            slog_info!(
                "lsp_protocol server={} root={} method=textDocument/publishDiagnostics event=dropped-because-{} file={} version={}",
                key.kind.id_str(),
                key.root.display(),
                reason,
                file.display(),
                version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| "none".to_string())
            );
        }
        Some(file)
    }

    fn handle_rust_progress(&mut self, root: PathBuf, params: &serde_json::Value) {
        let token = match params.get("token") {
            Some(serde_json::Value::String(token)) => token.clone(),
            Some(serde_json::Value::Number(token)) => token.to_string(),
            _ => return,
        };
        let Some(value) = params.get("value") else {
            return;
        };
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let title = value.get("title").and_then(serde_json::Value::as_str);
        let key = ServerKey {
            kind: ServerKind::Rust,
            root,
        };
        if let Some(client) = self.clients.get_mut(&key) {
            if matches!(kind, "begin" | "end") {
                slog_info!(
                    "lsp_protocol server=rust root={} method=$/progress event={} token={} title={}",
                    key.root.display(),
                    kind,
                    token,
                    title.unwrap_or("-")
                );
            }
            client.record_rust_progress(&token, kind, title);
            if kind == "end"
                && value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|message| {
                        message.to_lowercase().contains("cancel")
                            || message.to_lowercase().contains("fail")
                    })
            {
                if let Some(cache) = client.completed_rust_check.as_mut() {
                    cache.abort();
                }
            }
        }
    }

    /// True when this publish is the empty list a server sends to clear a
    /// document that a scoped inspect opened only to read its diagnostics and
    /// then closed. Such a publish describes the closed document, not the
    /// file, and is ignored for as long as the exception lasts. A non-empty
    /// publish is real news about the file: it is stored and ends the
    /// exception, as does reopening or editing the file through AFT.
    /// Remember a document a scoped inspect closed (see
    /// `inspect_closed_documents`), forgetting the oldest past the cap.
    fn note_inspect_closed_document(&mut self, key: &ServerKey, canonical_path: &Path) {
        self.inspect_close_sequence = self.inspect_close_sequence.wrapping_add(1);
        self.inspect_closed_documents.insert(
            (key.clone(), canonical_path.to_path_buf()),
            self.inspect_close_sequence,
        );
        if self.inspect_closed_documents.len() > INSPECT_CLOSED_DOCUMENTS_CAP {
            // Drop the oldest quarter at once so the scan runs rarely.
            let mut sequences: Vec<u64> = self.inspect_closed_documents.values().copied().collect();
            sequences.sort_unstable();
            let keep_from = sequences[INSPECT_CLOSED_DOCUMENTS_CAP / 4];
            self.inspect_closed_documents
                .retain(|_, sequence| *sequence >= keep_from);
        }
    }

    fn is_inspect_close_clearing(
        &mut self,
        key: &ServerKey,
        file: &Path,
        diagnostics_empty: bool,
    ) -> bool {
        if self.inspect_closed_documents.is_empty() {
            return false;
        }
        let lookup = (key.clone(), file.to_path_buf());
        if !self.inspect_closed_documents.contains_key(&lookup) {
            return false;
        }
        let reopened = self
            .documents
            .get(key)
            .is_some_and(|store| store.is_open(file));
        if reopened || !diagnostics_empty {
            self.inspect_closed_documents.remove(&lookup);
            return false;
        }
        true
    }

    /// Edits and opens through AFT make the file's publishes current again,
    /// and a pulled analysis recorded before them no longer describes it.
    fn end_inspect_close_exception(&mut self, canonical_path: &Path) {
        if !self.inspect_closed_documents.is_empty() {
            self.inspect_closed_documents
                .retain(|(_, file), _| file != canonical_path);
        }
        if !self.latest_pull_for_rust.is_empty() {
            self.latest_pull_for_rust
                .retain(|(_, file), _| file != canonical_path);
        }
    }

    fn handle_server_status(
        &mut self,
        server: ServerKind,
        root: PathBuf,
        params: &serde_json::Value,
    ) {
        if !matches!(&server, ServerKind::Rust) {
            return;
        }

        let key = ServerKey { kind: server, root };
        let health = params.get("health").and_then(serde_json::Value::as_str);
        let message = params
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("rust-analyzer reported unhealthy workspace analysis");
        // Ordinary warnings (for example unavailable proc macros) do not invalidate
        // diagnostics. Locked metadata resolution is the explicit exception: Cargo
        // could not load the dependency graph without modifying the lockfile.
        let locked_metadata_failure = health == Some("warning")
            && message.contains("cargo metadata")
            && message.contains("lock file")
            && message.contains("--locked was passed");
        let reports_failure = health == Some("error") || locked_metadata_failure;
        let quiescent = params.get("quiescent").and_then(serde_json::Value::as_bool) == Some(true);
        // rust-analyzer recomputes health only when a workspace load finishes.
        // A status sent while it is still working (`quiescent: false`, as right
        // after a reload starts) repeats the previous load's failure, so a
        // failure is final only once the server is quiescent. Taking it earlier
        // made an inspect that had just asked for a reload stop waiting and
        // report the failure the reload was meant to replace.
        let failure = (reports_failure && quiescent).then(|| message.to_string());
        if let Some(client) = self.clients.get_mut(&key) {
            client.rust_analyzer_warning = (health == Some("warning") && !reports_failure)
                .then(|| format!("rust-analyzer warning: {message}"));
            let failure = failure
                .clone()
                .map(|failure| rust_failure_with_root_cause(&failure, &client.stderr_tail()));
            client.set_diagnostic_failure(failure);
        }
        if failure.is_some() {
            // Earlier reports cannot certify a workspace whose metadata or check failed.
            self.diagnostics.clear_server_instance(&key);
            return;
        }
        if !quiescent {
            return;
        }
        let became_quiescent = self
            .clients
            .get_mut(&key)
            .is_some_and(|client| client.set_rust_analyzer_quiescent(true));
        if became_quiescent {
            self.diagnostics.promote_provisional_for_server(&key);
        }
    }

    /// The newest Cargo manifest, lockfile, toolchain file, or Cargo config
    /// of a running rust-analyzer's workspace that was modified after the
    /// server last started loading the workspace, if any.
    ///
    /// rust-analyzer re-reads these files only when the client reports a
    /// change to them or asks for a reload. Changes made outside AFT (for
    /// example `cargo update` run in a shell) are reported by neither, so
    /// without this check a load that failed on a stale `Cargo.lock` stays
    /// failed after the lockfile is fixed, and every later inspect repeats
    /// the old error.
    ///
    /// The workspace root's files are always checked. Member-crate manifests
    /// are checked for the directories between the workspace root and each
    /// of `scope_roots`, which covers the crate that owns a scoped file
    /// without walking the whole workspace on every inspect.
    pub(crate) fn rust_manifest_changed_since_load(
        &self,
        key: &ServerKey,
        scope_roots: &[PathBuf],
    ) -> Option<PathBuf> {
        if key.kind != ServerKind::Rust {
            return None;
        }
        let loaded_at = self.clients.get(key)?.workspace_loaded_at();
        rust_workspace_manifest_paths(&key.root, scope_roots)
            .into_iter()
            .filter_map(|path| {
                let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
                (modified > loaded_at).then_some((modified, path))
            })
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, path)| path)
    }

    /// Ask a running rust-analyzer to reload its workspace. Events already
    /// queued are applied first, so a status report that describes the old
    /// load cannot land after the reset. The server's previous analysis
    /// result is then dropped and it counts as warming until it reports
    /// quiescence for the new load; a blocking inspect waits for that report
    /// instead of repeating the old failure. Returns the pending request and
    /// the dropped state, which [`Self::finish_rust_workspace_reload`] puts
    /// back if the server rejects the request.
    pub(crate) fn begin_rust_workspace_reload(
        &mut self,
        key: &ServerKey,
    ) -> Result<
        Option<(
            crate::lsp::client::PendingLspRequest,
            crate::lsp::client::RustWorkspaceState,
        )>,
        LspError,
    > {
        if key.kind != ServerKind::Rust {
            return Ok(None);
        }
        self.drain_events();
        let Some(client) = self.clients.get_mut(key) else {
            return Ok(None);
        };
        let previous = client.begin_rust_workspace_reload(std::time::SystemTime::now());
        match client.start_request::<RustAnalyzerReloadWorkspace>(()) {
            Ok(pending) => {
                // Reports from the old load cannot certify files under the
                // new manifests; the reloaded server republishes them.
                self.diagnostics.clear_server_instance(key);
                self.latest_pull_for_rust
                    .retain(|(server, _), _| server != key);
                self.latest_push_for_pull_servers
                    .retain(|(server, _), _| server != key);
                Ok(Some((pending, previous)))
            }
            Err(error) => {
                client.restore_rust_workspace_state(previous);
                Err(error)
            }
        }
    }

    /// Complete a reload started by [`Self::begin_rust_workspace_reload`].
    /// A rejected request leaves no new load running, so the saved state is
    /// restored rather than leaving the server warming forever.
    pub(crate) fn finish_rust_workspace_reload(
        &mut self,
        key: &ServerKey,
        result: &Result<serde_json::Value, LspError>,
        previous: crate::lsp::client::RustWorkspaceState,
    ) {
        let Err(error) = result else {
            slog_info!(
                "lsp_protocol server=rust root={} method=rust-analyzer/reloadWorkspace event=accepted",
                key.root.display()
            );
            return;
        };
        crate::slog_warn!(
            "rust-analyzer in {} did not accept a workspace reload: {error}",
            key.root.display()
        );
        if let Some(client) = self.clients.get_mut(key) {
            client.restore_rust_workspace_state(previous);
        }
    }

    fn reap_unreferenced_children_for(&self, key: &ServerKey) {
        // A server starting without the lock has a registered child but no
        // client yet; it is not an orphan.
        if self.starting.contains_key(key) {
            return;
        }
        let live_pids = self
            .clients
            .values()
            .map(LspClient::child_pid)
            .collect::<HashSet<_>>();
        let orphans = self
            .child_registry
            .pids_for_server(&key.root, &key.kind)
            .into_iter()
            .filter(|pid| !live_pids.contains(pid))
            .collect::<Vec<_>>();
        if !orphans.is_empty() {
            self.child_registry.reap_pids(&orphans);
        }
    }

    fn spawn_server(
        &mut self,
        def: &ServerDef,
        root: &Path,
        source_file: &Path,
        config: &Config,
    ) -> Result<LspClient, StartFailure> {
        self.spawn_server_with_timeout(def, root, source_file, config, None)
    }

    fn spawn_server_with_timeout(
        &mut self,
        def: &ServerDef,
        root: &Path,
        source_file: &Path,
        config: &Config,
        initialize_timeout: Option<std::time::Duration>,
    ) -> Result<LspClient, StartFailure> {
        let prepared = self
            .prepare_spawn(def, root, source_file, config)
            .map_err(prepare_failure)?;
        prepared
            .run(initialize_timeout)
            .map_err(|failure| self.absorb_spawn_failure(failure))
    }

    /// Gather everything a spawn needs from manager state, so the spawn and
    /// its `initialize` handshake can run without the manager lock.
    fn prepare_spawn(
        &self,
        def: &ServerDef,
        root: &Path,
        source_file: &Path,
        config: &Config,
    ) -> Result<PreparedSpawn, LspError> {
        let mut resolution_config = config.clone();
        if let Some(paths) = &self.pushed_search_paths {
            resolution_config.lsp_paths_extra.clone_from(paths);
        }
        let mut initialization_options =
            initialization_options_for_spawn(def, source_file, root, &resolution_config)?;
        let has_binary_override = self.binary_overrides.contains_key(&def.kind)
            || env_binary_override(&def.kind).is_some();
        let mut runtime_note = None;
        let mut project_typescript = None;
        let binary = if let ServerKind::TypeScriptNative(package_dir) = &def.kind {
            let project =
                ProjectTypeScript::read(package_dir).unwrap_or_else(|| ProjectTypeScript {
                    version: "unknown".into(),
                    package_dir: package_dir.to_path_buf(),
                });
            // Start the platform binary itself, never node_modules/.bin/tsc:
            // that is a Node wrapper script. A missing platform package is a
            // named gap reported before anything is spawned.
            let binary = if has_binary_override {
                self.resolve_binary(def, root, config)?
            } else {
                resolve_native_binary(&project).map_err(|cause| {
                    LspError::ServerNotReady(native_server_unavailable_reason(&project, &cause))
                })?
            };
            runtime_note = Some(format!(
                "TypeScript {}: native language server (project installation) ({})",
                project.version,
                binary.display()
            ));
            project_typescript = Some(project);
            binary
        } else {
            self.resolve_binary(def, root, config)?
        };
        // Explicit binary overrides may wrap their own SDK discovery.
        if def.kind == ServerKind::TypeScript && !has_binary_override {
            let (options, note) = if def
                .args
                .iter()
                .any(|arg| arg == "--tsserver-path" || arg.starts_with("--tsserver-path="))
            {
                (initialization_options.unwrap_or_else(|| serde_json::json!({})), "TypeScript: explicit --tsserver-path override (version managed by configuration)".into())
            } else {
                typescript_runtime_options(
                    initialization_options,
                    source_file,
                    root,
                    &resolution_config,
                )?
            };
            initialization_options = Some(options);
            runtime_note = Some(note);
        }
        if def.kind == ServerKind::TypeScript {
            let boundary = config
                .project_root
                .as_deref()
                .filter(|p| source_file.starts_with(p))
                .unwrap_or(root);
            project_typescript = find_project_typescript_package(source_file, boundary);
        }

        // Merge the server-defined env with our test-injected env.
        // `extra_env` is empty in production; tests use it to drive fake
        // server variants (AFT_FAKE_LSP_PULL=1, etc.).
        let mut env = def.env.clone();
        for (key, value) in &self.extra_env {
            env.insert(key.clone(), value.clone());
        }

        // A server may use a nested language workspace, but the reclaim marker
        // belongs to the configured session project. Only register that broader
        // root when it contains the server root; otherwise retain the server root
        // so an unrelated configuration path cannot reap this child.
        let reclaim_root = config
            .project_root
            .as_deref()
            .map(crate::inspect::job::canonicalize_normalized)
            .filter(|project_root| root.starts_with(project_root))
            .unwrap_or_else(|| root.to_path_buf());

        Ok(PreparedSpawn {
            kind: def.kind.clone(),
            binary_name: def.binary.clone(),
            root: root.to_path_buf(),
            args: def.spawn_args_for_binary(&binary),
            binary,
            env,
            event_tx: self.event_tx.clone(),
            child_registry: self.child_registry.clone(),
            reclaim_root,
            initialization_options,
            runtime_note,
            project_typescript,
            storage_root: crate::bash_background::storage_dir(config.storage_dir.as_deref()),
        })
    }

    /// Record a failed spawn's exit details the way an inline spawn does, and
    /// return the failure to report.
    fn absorb_spawn_failure(&mut self, failure: SpawnFailure) -> StartFailure {
        match failure {
            SpawnFailure::NotStarted {
                error,
                exit_log_line,
            } => {
                self.record_exit_log_line(exit_log_line);
                let durability = process_start_durability(&error);
                StartFailure { error, durability }
            }
            SpawnFailure::Initialize {
                reason,
                report,
                durability,
            } => {
                // Dropping the client in `PreparedSpawn::run` killed a
                // still-running server, so the report is complete either way.
                self.remember_exit_report(*report);
                StartFailure {
                    error: LspError::ServerNotReady(reason),
                    durability,
                }
            }
        }
    }

    /// Keep a dead server's report until its reader's exit event is drained.
    fn remember_exit_report(&mut self, report: ServerExitReport) {
        if self.pending_exit_reports.len() >= PENDING_EXIT_REPORTS_CAP {
            // Never expected: each report is consumed by its reader's exit
            // event. Log directly rather than grow without bound.
            self.record_exit_log_line(report.log_line("exit event not yet seen"));
            return;
        }
        self.pending_exit_reports.insert(report.pid, report);
    }

    fn resolve_binary(
        &self,
        def: &ServerDef,
        root: &Path,
        config: &Config,
    ) -> Result<PathBuf, LspError> {
        if let Some(path) = self.binary_overrides.get(&def.kind) {
            if path.exists() {
                return Ok(path.clone());
            }
            return Err(LspError::NotFound(format!(
                "override binary for {:?} not found: {}",
                def.kind,
                path.display()
            )));
        }

        if let Some(path) = env_binary_override(&def.kind) {
            if path.exists() {
                return Ok(path);
            }
            return Err(LspError::NotFound(format!(
                "environment override binary for {:?} not found: {}",
                def.kind,
                path.display()
            )));
        }

        let mut pushed_config;
        let resolution_config = if let Some(paths) = self.pushed_search_paths.as_ref() {
            pushed_config = config.clone();
            pushed_config.lsp_paths_extra.clone_from(paths);
            &pushed_config
        } else {
            config
        };
        resolve_server_binary(def, Some(root), resolution_config).ok_or_else(|| {
            let searched = if matches!(def.kind, ServerKind::Python | ServerKind::Ty) {
                "the workspace virtualenv, node_modules/.bin, lsp_paths_extra, or PATH"
            } else {
                "node_modules/.bin, lsp_paths_extra, or PATH"
            };
            LspError::NotFound(format!(
                "language server binary '{}' not found in {searched}",
                def.binary,
            ))
        })
    }

    fn server_key_for_file(&self, file_path: &Path, config: &Config) -> Option<ServerKey> {
        for def in servers_for_file(file_path, config) {
            let key = server_key_for_definition(&def, file_path, config)?;
            if self.clients.contains_key(&key) {
                return Some(key);
            }
        }
        None
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

fn typescript_runtime_options(
    configured: Option<serde_json::Value>,
    source_file: &Path,
    server_root: &Path,
    config: &Config,
) -> Result<(serde_json::Value, String), LspError> {
    let mut options = configured.unwrap_or_else(|| serde_json::json!({}));
    if options
        .pointer("/tsserver/path")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|p| !p.is_empty())
    {
        return Ok((
            options,
            "TypeScript: explicit tsserver.path override (version managed by configuration)".into(),
        ));
    }
    let boundary = config
        .project_root
        .as_deref()
        .filter(|p| source_file.starts_with(p))
        .unwrap_or(server_root);
    let (lib, fallback) = match find_project_typescript_package(source_file, boundary) {
        // Reached only when the `typescript` server runs a program the user
        // chose; the built-in one is swapped for the native language server
        // before this point. A TypeScript 7+ project must still not be
        // served by the cached 5.x SDK: its diagnostics would silently follow
        // another compiler's defaults and accept options TypeScript 7
        // removed. Leave SDK discovery to the configured server.
        Some(project) if project.is_native_compiler() => {
            return Ok((
                options,
                format!(
                    "TypeScript {}: native compiler at {}, which has no tsserver; AFT cache fallback not used",
                    project.version,
                    project.package_dir.display()
                ),
            ));
        }
        Some(project) if project.major().is_some() && project.has_tsserver() => {
            (project.package_dir.join("lib"), false)
        }
        // An unreadable version, or TypeScript 6 or earlier without
        // tsserver.js: typescript-language-server would fail in initialize,
        // and serving the project with the cached SDK would report another
        // compiler's results. Name the installation instead of spawning.
        Some(project) => {
            return Err(LspError::ServerNotReady(unservable_typescript_reason(
                &project,
            )));
        }
        None => {
            let Some(lib) = config
                .lsp_paths_extra
                .iter()
                .filter_map(|bin| bin.parent())
                .map(|modules| modules.join("typescript").join("lib"))
                .find(|lib| lib.join("tsserver.js").is_file())
            else {
                // typescript-language-server can still find a global or
                // bundled SDK that AFT does not resolve, so let it try.
                return Ok((
                    options,
                    "TypeScript: server-managed SDK resolution (version not reported by AFT)"
                        .into(),
                ));
            };
            (lib, true)
        }
    };
    let version = std::fs::read(lib.parent().unwrap().join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|json| {
            json.get("version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".into());
    let path = lib.join("tsserver.js");
    merge_json_override(
        &mut options,
        serde_json::json!({"tsserver": {"path": path}}),
    );
    let source = if fallback {
        "AFT cache fallback; not the project's pinned TypeScript"
    } else {
        "project installation"
    };
    Ok((
        options,
        format!("TypeScript {version}: {source} ({})", path.display()),
    ))
}

/// When rust-analyzer fails to load a workspace, its `experimental/serverStatus`
/// message says only "Failed to load workspaces."; the cause (for example
/// Cargo's "cannot update the lock file ... because --locked was passed") goes
/// to the server's stderr. Append the last Cargo `error:` line from that
/// stderr so the failure names what to fix. A message that already carries an
/// `error:` line is kept as it is.
pub(crate) fn rust_failure_with_root_cause(message: &str, stderr_tail: &str) -> String {
    let message = message.trim_end();
    if message.contains("error:") {
        return message.to_string();
    }
    let cause = stderr_tail
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("error:"));
    match cause {
        Some(cause) => format!("{message} {cause}"),
        None => message.to_string(),
    }
}

fn typescript_initialize_failure_reason(
    reason: String,
    project_typescript: Option<&ProjectTypeScript>,
) -> String {
    use super::environmental::{TS_LS5_NO_INSTALLATION, TS_LS6_NO_TSSERVER, TS_NATIVE_NO_TSSERVER};

    let no_usable_sdk =
        reason.contains(TS_LS5_NO_INSTALLATION) || reason.contains(TS_LS6_NO_TSSERVER);
    if !no_usable_sdk {
        return reason;
    }
    // Installing dependencies cannot help a TypeScript 7 project: the native
    // compiler never ships the tsserver.js typescript-language-server loads.
    if let Some(project) = project_typescript.filter(|ts| ts.is_native_compiler()) {
        return format!(
            "TypeScript unavailable: this project uses TypeScript {} (native compiler) at {}, {TS_NATIVE_NO_TSSERVER}. Run the project's tsc --noEmit for type errors. {reason}",
            project.version,
            project.package_dir.display(),
        );
    }
    if reason.contains(TS_LS6_NO_TSSERVER) {
        // The server found a TypeScript package without tsserver.js. That is
        // TypeScript 7 or later somewhere AFT did not look, or a broken
        // install; either way running an install is not the fix.
        return format!("TypeScript SDK unavailable: the TypeScript installation the language server found has no tsserver.js (TypeScript 7 and later, the native compiler, ship none), so typescript-language-server can't serve it. Check which TypeScript the project resolves; run the project's tsc --noEmit for type errors. {reason}");
    }
    format!("TypeScript SDK unavailable: the language server could not find a valid TypeScript installation; run bun install or enable LSP auto-install (AFT never installs into the worktree). {reason}")
}

fn biome_unavailable_reason(reason: &str) -> String {
    format!("Biome unavailable: {reason}. Check the project's Biome version and configuration; if biome is not installed in this worktree, run bun install (AFT does not install into the worktree).")
}

const ASTRO_TSDK_UNAVAILABLE: &str = "astro-ls requires a project TypeScript install; none found";

fn initialization_options_for_spawn(
    def: &ServerDef,
    source_file: &Path,
    server_root: &Path,
    config: &Config,
) -> Result<Option<serde_json::Value>, LspError> {
    if def.kind == ServerKind::Rust {
        // Cargo holds its target directory lock throughout a check, including
        // time spent waiting for compiler resources. Isolate analyzer checks
        // so they cannot block the user's builds and tests. `true` nests under
        // Cargo's resolved target directory, including environment/config overrides.
        let mut options = serde_json::json!({"cargo": {"targetDir": true}});
        if let Some(configured) = def.initialization_options.clone() {
            merge_json_override(&mut options, configured);
        }
        return Ok(Some(options));
    }
    if def.kind != ServerKind::Astro {
        return Ok(def.initialization_options.clone());
    }

    if def
        .initialization_options
        .as_ref()
        .and_then(|options| options.pointer("/typescript/tsdk"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|tsdk| !tsdk.is_empty())
    {
        return Ok(def.initialization_options.clone());
    }

    // astro-ls does not discover TypeScript itself. Resolve the nearest project
    // installation for the triggering .astro file without walking above the
    // configured project, so monorepo members use their own TypeScript SDK.
    let project_root = config.project_root.as_deref().unwrap_or(server_root);
    let boundary = if source_file.starts_with(project_root) {
        project_root
    } else {
        server_root
    };
    let tsdk = find_project_typescript_sdk(source_file, boundary)
        .ok_or_else(|| LspError::ServerNotReady(ASTRO_TSDK_UNAVAILABLE.to_string()))?;
    let mut options = serde_json::json!({
        "typescript": {
            "tsdk": tsdk.to_string_lossy(),
        }
    });
    if let Some(configured) = def.initialization_options.clone() {
        merge_json_override(&mut options, configured);
    }
    Ok(Some(options))
}

fn find_project_typescript_sdk(source_file: &Path, project_root: &Path) -> Option<PathBuf> {
    let mut directory = source_file.parent()?;
    loop {
        let lib = directory
            .join("node_modules")
            .join("typescript")
            .join("lib");
        if lib.join("tsserverlibrary.js").is_file() || lib.join("typescript.js").is_file() {
            return Some(lib);
        }
        if directory == project_root {
            return None;
        }
        let parent = directory.parent()?;
        if !parent.starts_with(project_root) {
            return None;
        }
        directory = parent;
    }
}

fn merge_json_override(base: &mut serde_json::Value, override_value: serde_json::Value) {
    match (base, override_value) {
        (serde_json::Value::Object(base), serde_json::Value::Object(override_fields)) => {
            for (key, value) in override_fields {
                if let Some(existing) = base.get_mut(&key) {
                    merge_json_override(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, value) => *base = value,
    }
}

fn recoverable_pull_rejection(err: &LspError) -> bool {
    matches!(
        err,
        LspError::ServerError {
            code: -32601 | -32602,
            ..
        }
    )
}

fn server_attempt_result_reason(result: &ServerAttemptResult) -> String {
    match result {
        ServerAttemptResult::SpawnFailed { binary, reason } => {
            format!("spawn_failed: {binary} ({reason})")
        }
        ServerAttemptResult::BinaryNotInstalled { binary } => {
            format!("binary_not_installed: {binary}")
        }
        ServerAttemptResult::NoRootMarker { looked_for } => {
            format!("no_root_marker (looked for: {})", looked_for.join(", "))
        }
        ServerAttemptResult::Ok { .. } => "ok".to_string(),
    }
}

fn format_stderr_tail_for_reason(stderr_tail: &str) -> String {
    truncate_stderr_tail_for_reason(stderr_tail)
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_stderr_tail_for_reason(stderr_tail: &str) -> String {
    if stderr_tail.len() <= STDERR_REASON_BYTES {
        return stderr_tail.to_string();
    }

    let ellipsis = "...";
    let target_len = STDERR_REASON_BYTES.saturating_sub(ellipsis.len());
    let mut start = stderr_tail.len() - target_len;
    while start < stderr_tail.len() && !stderr_tail.is_char_boundary(start) {
        start += 1;
    }
    format!("{ellipsis}{}", &stderr_tail[start..])
}

/// Full failure text for a server that died during `initialize`. The first
/// line stands alone (inspect reports only that line); the stderr tail and a
/// remediation hint follow for callers that show the whole text.
fn format_initialize_failure_reason(
    binary: &str,
    report: &ServerExitReport,
    err: &LspError,
) -> String {
    let mut reason = format!(
        "server crashed during initialize ({}): {err}",
        report.short_cause()
    );
    append_stderr_block(&mut reason, binary, &report.stderr_tail);
    reason
}

fn format_post_initialize_exit_reason(
    binary: &str,
    report: &ServerExitReport,
    err: &LspError,
) -> String {
    let mut reason = format!(
        "server exited after initialize ({}): {err}",
        report.short_cause()
    );
    append_stderr_block(&mut reason, binary, &report.stderr_tail);
    reason
}

fn append_stderr_block(reason: &mut String, binary: &str, stderr_tail: &str) {
    if stderr_tail.is_empty() {
        return;
    }
    reason.push_str("\nstderr (last 64 lines):\n");
    reason.push_str(&format_stderr_tail_for_reason(stderr_tail));
    reason.push_str("\n\n");
    reason.push_str(&failure_hint(binary, stderr_tail));
}

/// Single-line failure summary for inspect output when a language server could
/// not start: the self-contained first line of the reason, cut to
/// `PRODUCER_FAILURE_REASON_BYTES`.
fn summarize_failure_reason(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or_default().trim_end();
    if first_line.len() <= PRODUCER_FAILURE_REASON_BYTES {
        return first_line.to_string();
    }
    let mut end = PRODUCER_FAILURE_REASON_BYTES;
    while end > 0 && !first_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &first_line[..end])
}

fn failure_hint(binary: &str, stderr_tail: &str) -> String {
    if stderr_tail.contains("MODULE_NOT_FOUND") || stderr_tail.contains("Cannot find module") {
        let package_manager = infer_package_manager(stderr_tail);
        format!(
            "Your package-manager shim resolves to a missing file. Try reinstalling: {package_manager} install -g {binary} --force. Common cause: hard-link breakage from fs migration or store prune."
        )
    } else if let Some(component) = rustup_missing_component(stderr_tail) {
        // The binary on PATH is rustup's proxy shim, but the toolchain
        // component isn't installed, so rustup rejects the dispatch with
        // "Unknown binary '<name>' in ... toolchain". The actionable fix is to
        // add the component, not anything about the binary itself.
        format!("'{component}' is a rustup proxy but the component is not installed. Install it: rustup component add {component}")
    } else {
        format!("Hint: see stderr above for '{binary}' failure details.")
    }
}

/// Detect the rustup "proxy shim without installed component" failure and
/// return the component name to add. rustup prints
/// `error: Unknown binary '<name>' in official toolchain '<triple>'` when a
/// `~/.cargo/bin/<name>` proxy is on PATH but the component was never installed
/// (the canonical case is `rust-analyzer`, which ships as an opt-in component).
fn rustup_missing_component(stderr_tail: &str) -> Option<String> {
    let marker = "Unknown binary '";
    let start = stderr_tail.find(marker)? + marker.len();
    let rest = &stderr_tail[start..];
    let end = rest.find('\'')?;
    let name = &rest[..end];
    // Only treat it as a rustup-component issue when the toolchain phrasing is
    // present, so an unrelated "Unknown binary" message doesn't mislead.
    if name.is_empty() || !stderr_tail.contains("toolchain") {
        return None;
    }
    Some(name.to_string())
}

fn infer_package_manager(stderr_tail: &str) -> &'static str {
    let lower = stderr_tail.to_ascii_lowercase();
    if lower.contains(".pnpm/") || lower.contains(".pnpm\\") || lower.contains("/pnpm/") {
        "pnpm"
    } else if lower.contains(".yarn/")
        || lower.contains(".yarn\\")
        || lower.contains("/yarn/")
        || lower.contains("yarn")
    {
        "yarn"
    } else {
        "npm"
    }
}

fn canonicalize_for_lsp(file_path: &Path) -> Result<PathBuf, LspError> {
    // The whole LSP subsystem must agree on ONE canonical form. Workspace
    // roots are normalized (verbatim prefix stripped on Windows) because
    // CreateProcess rejects verbatim cwds; document and watched-file paths
    // are compared against those roots with starts_with, so a bare
    // fs::canonicalize here would produce verbatim paths on Windows that
    // never match any client root.
    std::fs::canonicalize(file_path)
        .map(|canonical| crate::inspect::job::normalize_path(&canonical))
        .map_err(LspError::from)
}

fn resolve_for_lsp_uri(file_path: &Path) -> PathBuf {
    // Same normalized form as canonicalize_for_lsp and the client roots;
    // see the comment there.
    if let Ok(path) = std::fs::canonicalize(file_path) {
        return crate::inspect::job::normalize_path(&path);
    }

    let mut existing = file_path.to_path_buf();
    let mut missing = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        missing.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent.to_path_buf();
    }

    let mut resolved = std::fs::canonicalize(&existing)
        .map(|canonical| crate::inspect::job::normalize_path(&canonical))
        .unwrap_or(existing);
    for segment in missing.into_iter().rev() {
        resolved.push(segment);
    }
    resolved
}

fn language_id_for_extension(ext: &str) -> &'static str {
    match ext {
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "rs" => "rust",
        "go" => "go",
        "html" | "htm" => "html",
        "md" | "markdown" | "mdx" | "mkd" | "mkdn" | "mdown" | "mdwn" | "qmd" | "rmd" => "markdown",
        _ => "plaintext",
    }
}

fn log_did_open_sent(key: &ServerKey, file: &Path, language_id: &str) {
    slog_info!(
        "lsp_protocol server={} root={} method=textDocument/didOpen event=sent file={} language_id={} version=0",
        key.kind.id_str(),
        key.root.display(),
        file.display(),
        language_id
    );
}

/// Tell `client` that the document at `uri` was saved with `content`, if
/// the server asked to hear about saves (`textDocumentSync.save`). Servers
/// do on-save work only then; rust-analyzer's `cargo check`, the source of
/// its compiler errors, is the case that matters.
fn send_did_save(
    client: &mut LspClient,
    uri: &lsp_types::Uri,
    content: &str,
) -> Result<(), LspError> {
    let Some(save) = client.save_notification() else {
        return Ok(());
    };
    client.send_did_save_borrowed(
        uri,
        (save == SaveNotification::IncludeText).then_some(content),
    )?;
    client.record_save_sent(uri);
    slog_info!(
        "lsp_protocol server={} root={} method=textDocument/didSave event=sent uri={}",
        client.kind().id_str(),
        client.root().display(),
        uri.as_str()
    );
    Ok(())
}

fn normalize_lookup_path(path: &Path) -> PathBuf {
    // Normalized like every other LSP-subsystem path (see canonicalize_for_lsp):
    // store keys and lookups must share one canonical form or Windows verbatim
    // spellings silently miss.
    std::fs::canonicalize(path)
        .map(|canonical| crate::inspect::job::normalize_path(&canonical))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn diagnostic_path_candidates(file: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    let mut add = |candidate: PathBuf| {
        if !candidates.iter().any(|existing| existing == &candidate) {
            candidates.push(candidate);
        }
    };

    // Existing files normalize through the same URI path used when publishing
    // diagnostics. A deleted file cannot be canonicalized, so retain the
    // watcher spelling as a candidate too.
    add(file.to_path_buf());
    add(normalize_lookup_path(file));

    // The parent survives a file deletion. Rebuild both forms used by the
    // diagnostics store: raw filesystem canonicalization (legacy/direct
    // publishers) and its non-verbatim normalized spelling (LSP URI events).
    if let (Some(parent), Some(name)) = (file.parent(), file.file_name()) {
        if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
            let reconstructed = canonical_parent.join(name);
            add(reconstructed.clone());
            add(crate::inspect::job::normalize_path(&reconstructed));
        }
    }

    candidates
}

/// Everything needed to spawn and initialize one language server, gathered
/// from `LspManager` state so the slow part can run without the manager lock.
struct PreparedSpawn {
    kind: ServerKind,
    /// The installable binary name, used in failure reasons.
    binary_name: String,
    root: PathBuf,
    binary: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    event_tx: Sender<LspEvent>,
    child_registry: LspChildRegistry,
    reclaim_root: PathBuf,
    initialization_options: Option<serde_json::Value>,
    runtime_note: Option<String>,
    /// For the TypeScript server, the project's own TypeScript package, used
    /// to explain an initialize failure.
    project_typescript: Option<ProjectTypeScript>,
    storage_root: PathBuf,
}

/// Why a prepared spawn produced no client. The manager records the details
/// with [`LspManager::absorb_spawn_failure`] once it holds its lock again.
enum SpawnFailure {
    /// The process never started.
    NotStarted {
        error: LspError,
        exit_log_line: String,
    },
    /// The process started but the `initialize` handshake failed.
    Initialize {
        reason: String,
        report: Box<ServerExitReport>,
        durability: FailureDurability,
    },
}

impl PreparedSpawn {
    fn inspect_initialize_timeout(&self) -> Duration {
        self.test_initialize_timeout()
            .unwrap_or(super::client::HANDSHAKE_REQUEST_TIMEOUT)
    }

    /// Debug-build test hook: `AFT_TEST_LSP_INITIALIZE_TIMEOUT_MS` in the
    /// server's environment replaces the handshake budget, so a test can run
    /// a handshake past it without waiting the real budget out.
    fn test_initialize_timeout(&self) -> Option<Duration> {
        #[cfg(debug_assertions)]
        if let Some(timeout) = self
            .env
            .get("AFT_TEST_LSP_INITIALIZE_TIMEOUT_MS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|millis| *millis > 0)
        {
            return Some(Duration::from_millis(timeout));
        }
        None
    }

    /// Start the process and run the `initialize` handshake. Touches no
    /// manager state, so it runs without the manager lock.
    fn run(self, initialize_timeout: Option<Duration>) -> Result<LspClient, SpawnFailure> {
        let initialize_timeout = initialize_timeout.or_else(|| self.test_initialize_timeout());
        let completed_rust_check = (self.kind == ServerKind::Rust)
            .then(|| {
                super::completed_rust_check::CompletedRustCheck::new(
                    &self.root,
                    &self.reclaim_root,
                    &self.storage_root,
                    super::completed_rust_check::Runtime {
                        binary: self.binary.clone(),
                        args: self.args.clone(),
                        env: self.env.clone(),
                        options: self.initialization_options.clone(),
                        launch_env: None,
                    },
                )
            })
            .flatten();
        let mut client = match LspClient::spawn_with_reclaim_root(
            self.kind.clone(),
            self.root.clone(),
            &self.binary,
            &self.args,
            &self.env,
            self.event_tx,
            self.child_registry,
            Some(&self.reclaim_root),
        ) {
            Ok(client) => client,
            Err(err) => {
                let exit_log_line = format!(
                    "exited {:?} {} (spawn failed): pid=none phase={} status=never started elapsed=0.0s error={:?}",
                    self.kind,
                    self.root.display(),
                    ServerPhase::Spawn,
                    err.to_string()
                );
                return Err(SpawnFailure::NotStarted {
                    error: err.into(),
                    exit_log_line,
                });
            }
        };
        client.runtime_note = self.runtime_note;
        client.completed_rust_check = completed_rust_check;
        let initialize = match initialize_timeout {
            Some(timeout) => {
                client.initialize_with_timeout(&self.root, self.initialization_options, timeout)
            }
            None => client.initialize(&self.root, self.initialization_options),
        };
        if let Err(err) = &initialize {
            let phase = client.phase();
            // A timeout means the server is alive but slow; anything else
            // (broken pipe, closed stream) means it is exiting, so wait a
            // moment for its real exit status.
            let status = if matches!(err, LspError::Timeout(_)) {
                client.child_exit_status()
            } else {
                client.wait_for_exit(INITIALIZE_EXIT_WAIT)
            };
            let report = client.exit_report(phase, status, status.is_none());
            let reason = if status.is_some() || !report.stderr_tail.is_empty() {
                format_initialize_failure_reason(&self.binary_name, &report, &err)
            } else {
                format!("server failed during initialize: {err}")
            };
            let reason = if self.kind == ServerKind::Biome {
                biome_unavailable_reason(&reason)
            } else if self.kind == ServerKind::TypeScript {
                typescript_initialize_failure_reason(reason, self.project_typescript.as_ref())
            } else {
                reason
            };
            if matches!(err, LspError::Timeout(_)) {
                slog_info!(
                    "lsp initialize cancelled server={} root={} reason=initialize handshake timeout",
                    self.kind.id_str(),
                    self.root.display()
                );
            }
            // A handshake that ran past its budget says only that the server
            // was slow this time (a cold JDTLS on a large Maven repository
            // can be); it is killed below and may start fine later.
            let durability = if matches!(err, LspError::Timeout(_)) {
                FailureDurability::Transient
            } else {
                exit_durability(&reason, &report.stderr_tail)
            };
            // Dropping the client here kills a still-running server.
            return Err(SpawnFailure::Initialize {
                reason,
                report: Box::new(report),
                durability,
            });
        }
        // The native server is found by package layout alone, so confirm the
        // binary really is TypeScript's language server before trusting its
        // diagnostics for the project.
        if let (ServerKind::TypeScriptNative(package_dir), Ok(result)) = (&self.kind, &initialize) {
            let reported = result.server_info.as_ref().map(|info| info.name.as_str());
            if reported != Some(NATIVE_SERVER_INFO_NAME) {
                let reason = native_server_misidentified_reason(
                    self.project_typescript.as_ref(),
                    package_dir,
                    &self.binary,
                    reported,
                );
                slog_info!(
                    "lsp native TypeScript server rejected root={} reason={reason}",
                    self.root.display()
                );
                let _ = client.shutdown();
                let status = client.wait_for_exit(INITIALIZE_EXIT_WAIT);
                let report = client.exit_report(ServerPhase::Initialize, status, status.is_none());
                // AFT's own check refused this binary; starting it again
                // would be refused the same way.
                return Err(SpawnFailure::Initialize {
                    reason,
                    report: Box::new(report),
                    durability: FailureDurability::Permanent,
                });
            }
        }
        Ok(client)
    }
}

/// Completion flag for a server start running without the manager lock. A
/// second start of the same server waits on it instead of spawning a twin.
#[derive(Default)]
struct StartSignal {
    finished: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}

impl StartSignal {
    fn finish(&self) {
        *self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }

    /// Wait for the start to finish. False when `deadline` passed first.
    fn wait_until(&self, deadline: Instant) -> bool {
        let mut finished = self
            .finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if *finished {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            finished = self
                .changed
                .wait_timeout(finished, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

/// A server whose start is running without the manager lock.
struct StartReservation {
    /// Events from this server that arrived before its client was published.
    /// Handling them earlier would find no client for the key, so a
    /// rust-analyzer quiescence notice, for example, would be lost; they are
    /// replayed once the start finishes.
    deferred_events: Vec<LspEvent>,
    /// `LspManager::clients_generation` when the start began.
    clients_generation: u64,
    signal: Arc<StartSignal>,
}

/// What an unlocked start does next, decided under the manager lock.
enum StartNext {
    /// Another thread is starting this server; wait for it, then look again.
    Wait(Arc<StartSignal>),
    /// This thread reserved the server and spawns it without the lock.
    Spawn(PreparedSpawn),
}

/// Start one inspect producer without holding the manager lock across the
/// spawn and the `initialize` handshake.
///
/// The handshake can take seconds (rust-analyzer on a large workspace), and
/// while it held the lock every other manager user waited, including the
/// standalone request loop, which drains LSP events and renders the status
/// bar under that lock; a sibling `read` then waited for the handshake. The
/// start reserves the server under the lock, spawns and initializes it
/// unlocked, and publishes the client under the lock. A concurrent start of
/// the same server waits for the reservation instead of spawning a second
/// process. Outcomes match [`LspManager::start_applicable_servers`] for one
/// server under a request deadline.
pub fn start_applicable_server_unlocked(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    snapshot: &ApplicableServerSnapshot,
    server: &ServerKey,
    config: &Config,
    deadline: Instant,
) -> ApplicableServerStartOutcomes {
    let mut outcomes = ApplicableServerStartOutcomes {
        failures: snapshot
            .producer_failures
            .iter()
            .filter(|failure| failure.server_key == *server)
            .cloned()
            .collect(),
        ..ApplicableServerStartOutcomes::default()
    };
    let Some(candidate) = snapshot
        .candidates
        .iter()
        .find(|candidate| candidate.key == *server)
    else {
        return outcomes;
    };

    let mut waited = false;
    loop {
        if Instant::now() >= deadline {
            outcomes.deadline_exceeded = Some(candidate.key.clone());
            return outcomes;
        }
        let next =
            manager
                .lock()
                .begin_unlocked_start(candidate, config, waited, deadline, &mut outcomes);
        match next {
            None => return outcomes,
            Some(StartNext::Wait(signal)) => {
                if !signal.wait_until(deadline) {
                    outcomes.deadline_exceeded = Some(candidate.key.clone());
                    return outcomes;
                }
                waited = true;
            }
            Some(StartNext::Spawn(prepared)) => {
                // The inspect wait budget is not an initialize cancellation. Keep
                // the reserved start alive after the caller's deadline, so a
                // slow handshake can finish without being killed by AFT.
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                let initialize_timeout = prepared.inspect_initialize_timeout();
                let manager = Arc::clone(manager);
                let candidate = candidate.clone();
                std::thread::spawn(move || {
                    let mut reservation = ReservationGuard {
                        manager: &manager,
                        key: Some(candidate.key.clone()),
                    };
                    let result = prepared.run(Some(initialize_timeout));
                    let mut completed = ApplicableServerStartOutcomes::default();
                    manager.lock().finish_unlocked_start(
                        &candidate,
                        result,
                        deadline,
                        &mut completed,
                    );
                    reservation.key = None;
                    let _ = tx.send(completed);
                });
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(completed) => {
                        outcomes.successful.extend(completed.successful);
                        outcomes.failures.extend(completed.failures);
                        outcomes.deadline_exceeded = completed.deadline_exceeded;
                    }
                    Err(_) => outcomes.deadline_exceeded = Some(server.clone()),
                }
                return outcomes;
            }
        }
    }
}

/// Releases a reservation whose start unwound before publishing, so waiters
/// are not left blocked on a start that will never finish.
struct ReservationGuard<'a> {
    manager: &'a parking_lot::Mutex<LspManager>,
    key: Option<ServerKey>,
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.manager.lock().release_start_reservation(&key);
        }
    }
}

/// What [`ensure_server_for_file_detailed_unlocked`] does next for one
/// server, decided under the manager lock.
enum FileServerStart {
    /// A client is running.
    Running,
    /// The server cannot start; report this.
    Failed(ServerAttemptResult),
    /// Another thread is starting this server; wait for it, then look again.
    Wait(Arc<StartSignal>),
    /// This thread reserved the server and starts it without the lock.
    Spawn(Box<PreparedSpawn>),
}

/// How long one wait for another thread's start of the same server lasts
/// before the waiter looks at the manager again. A start ends on its own
/// within the `initialize` budget; this only bounds a wait whose signal was
/// lost to a start that unwound.
const FILE_START_WAIT_SLICE: Duration = Duration::from_secs(5);

/// Releases a reservation taken by [`ensure_server_for_file_detailed_unlocked`]
/// if its start unwinds before publishing, so waiters are not left blocked.
struct FileStartGuard<'a, L, G>
where
    L: Fn() -> G,
    G: std::ops::DerefMut<Target = LspManager>,
{
    lock: &'a L,
    key: Option<ServerKey>,
}

impl<L, G> Drop for FileStartGuard<'_, L, G>
where
    L: Fn() -> G,
    G: std::ops::DerefMut<Target = LspManager>,
{
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            (self.lock)().release_start_reservation(&key);
        }
    }
}

/// [`LspManager::ensure_server_for_file_detailed`] with the server spawn and
/// its `initialize` handshake (up to 30 seconds) run without the manager
/// lock. Each server to start is reserved under the lock; a concurrent caller
/// for the same server waits on that one reservation rather than on the
/// whole manager, and callers for other servers or for no server at all are
/// not held up. Outcomes, failure caching and the retry backoff match the
/// locked method. `lock` acquires the manager (for example `|| ctx.lsp()`).
pub fn ensure_server_for_file_detailed_unlocked<G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    config: &Config,
) -> EnsureServerOutcomes
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    let mut outcomes = EnsureServerOutcomes::default();
    for def in servers_for_file(file_path, config) {
        let server_id = def.kind.id_str().to_string();
        let server_name = def.name.to_string();
        let Some(key) = server_key_for_definition(&def, file_path, config) else {
            outcomes.attempts.push(ServerAttempt {
                server_id,
                server_name,
                result: ServerAttemptResult::NoRootMarker {
                    looked_for: def.root_markers.iter().map(|s| s.to_string()).collect(),
                },
            });
            continue;
        };
        let failure = loop {
            let next = lock().begin_file_server_start(&def, &key, file_path, config);
            match next {
                FileServerStart::Running => break None,
                FileServerStart::Failed(result) => break Some(result),
                FileServerStart::Wait(signal) => {
                    signal.wait_until(Instant::now() + FILE_START_WAIT_SLICE);
                }
                FileServerStart::Spawn(prepared) => {
                    let mut guard = FileStartGuard {
                        lock: &lock,
                        key: Some(key.clone()),
                    };
                    let result = prepared.run(None);
                    guard.key = None;
                    break lock().finish_file_server_start(&def, &key, result);
                }
            }
        };
        match failure {
            Some(result) => outcomes.attempts.push(ServerAttempt {
                server_id,
                server_name,
                result,
            }),
            None => {
                outcomes.attempts.push(ServerAttempt {
                    server_id,
                    server_name,
                    result: ServerAttemptResult::Ok {
                        server_key: key.clone(),
                    },
                });
                outcomes.successful.push(key);
            }
        }
    }
    outcomes
}

/// Start the servers for `file_path` without the manager lock (see
/// [`ensure_server_for_file_detailed_unlocked`]), for a caller that then
/// notifies or pulls under the lock. The path is canonicalized first, as the
/// locked methods do; an unreadable path starts nothing.
pub fn start_servers_for_file_unlocked<G>(lock: impl Fn() -> G, file_path: &Path, config: &Config)
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    if let Ok(canonical_path) = canonicalize_for_lsp(file_path) {
        ensure_server_for_file_detailed_unlocked(lock, &canonical_path, config);
    }
}

/// The servers that would serve `file_path`, found without the manager lock
/// and without starting anything. A caller that could not reach the manager
/// in time reports these as pending rather than claiming nothing to wait for.
pub fn expected_server_keys_for_file(file_path: &Path, config: &Config) -> Vec<ServerKey> {
    let Ok(canonical_path) = canonicalize_for_lsp(file_path) else {
        return Vec::new();
    };
    servers_for_file(&canonical_path, config)
        .into_iter()
        .filter_map(|def| server_key_for_definition(&def, &canonical_path, config))
        .collect()
}

/// [`LspManager::ensure_file_open`] with any server start run without the
/// manager lock (see [`ensure_server_for_file_detailed_unlocked`]). The
/// document is opened under the lock once its servers run.
pub fn ensure_file_open_unlocked<G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    config: &Config,
) -> Result<EnsureFileOpenResult, LspError>
where
    G: std::ops::DerefMut<Target = LspManager>,
{
    let canonical_path = canonicalize_for_lsp(file_path)?;
    ensure_server_for_file_detailed_unlocked(&lock, &canonical_path, config);
    // Every server is now running or has a remembered failure, so this
    // starts nothing under the lock unless a client vanished in between.
    lock().ensure_file_open(&canonical_path, config)
}

/// rust-analyzer's own LSP request (not part of the LSP specification) that
/// re-runs `cargo metadata` and rebuilds its crate graph from the current
/// manifests.
enum RustAnalyzerReloadWorkspace {}

impl lsp_types::request::Request for RustAnalyzerReloadWorkspace {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "rust-analyzer/reloadWorkspace";
}

/// rust-analyzer answers a reload request as soon as it has queued the
/// reload; the load itself is reported later through server status.
const RUST_RELOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The files whose contents decide how Cargo, and so rust-analyzer, loads
/// the workspace at `root`: the manifest and Cargo config of the root and of
/// every directory between it and each scope root, plus the root's lockfile
/// and toolchain files.
fn rust_workspace_manifest_paths(root: &Path, scope_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    for scope in scope_roots {
        for dir in scope.ancestors() {
            if dir == root || !dir.starts_with(root) {
                break;
            }
            if !dirs.iter().any(|known| known == dir) {
                dirs.push(dir.to_path_buf());
            }
        }
    }
    let mut paths = Vec::new();
    for dir in &dirs {
        paths.push(dir.join("Cargo.toml"));
        paths.push(dir.join(".cargo").join("config"));
        paths.push(dir.join(".cargo").join("config.toml"));
    }
    for name in ["Cargo.lock", "rust-toolchain", "rust-toolchain.toml"] {
        paths.push(root.join(name));
    }
    paths
}

/// Make a running rust-analyzer reload its workspace when a manifest,
/// lockfile, toolchain file, or Cargo config changed after its last load
/// (see [`LspManager::rust_manifest_changed_since_load`]). Returns true when
/// a reload was requested and accepted, after which the server counts as
/// warming until it reports quiescence for the new load. The manager lock is
/// not held while waiting for the server's answer.
pub fn reload_rust_workspace_if_manifests_changed(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    key: &ServerKey,
    scope_roots: &[PathBuf],
) -> bool {
    let started = {
        let mut lsp = manager.lock();
        let Some(changed) = lsp.rust_manifest_changed_since_load(key, scope_roots) else {
            return false;
        };
        slog_info!(
            "lsp_protocol server=rust root={} method=rust-analyzer/reloadWorkspace event=requested changed={}",
            key.root.display(),
            changed.display()
        );
        match lsp.begin_rust_workspace_reload(key) {
            Ok(Some(started)) => started,
            Ok(None) => return false,
            Err(error) => {
                crate::slog_warn!(
                    "could not ask rust-analyzer in {} to reload its workspace: {error}",
                    key.root.display()
                );
                return false;
            }
        }
    };
    let (pending, previous) = started;
    let result = pending.wait(RUST_RELOAD_REQUEST_TIMEOUT);
    let accepted = result.is_ok();
    manager
        .lock()
        .finish_rust_workspace_reload(key, &result, previous);
    accepted
}

/// Most file-system events forwarded to one server in one
/// `workspace/didChangeWatchedFiles` notification. Past this a batch is a
/// bulk change (a checkout, a generated tree), where per-file events cost the
/// server more than they tell it; see
/// [`LspManager::forward_watcher_file_events`].
pub const WATCHED_FILE_FORWARD_CAP: usize = 512;

/// How many distinct watcher paths may wait for the LSP manager lock (see
/// [`WatcherForwardBacklog`]). One drain batch holds at most this many, so
/// a single contended batch never overflows on its own; each notification
/// is still bounded per server by [`WATCHED_FILE_FORWARD_CAP`].
pub const WATCHER_FORWARD_BACKLOG_CAP: usize = 2_048;

/// The per-context slot holding watcher changes queued for the LSP manager.
/// `Some` exactly while the one helper thread serving it exists.
pub type WatcherForwardSlot = Arc<parking_lot::Mutex<Option<WatcherForwardBacklog>>>;

/// Watcher changes merged across drains until the LSP manager lock is free.
///
/// Paths are deduplicated. Past [`WATCHER_FORWARD_BACKLOG_CAP`] the backlog
/// follows the same overflow rule as a single oversized forward: only
/// project configuration files are kept (none if even those overflow), and
/// every running rust-analyzer gets a manifest-gated reload check in place
/// of the dropped paths.
#[derive(Debug, Default)]
pub struct WatcherForwardBacklog {
    paths: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
    /// Paths were dropped, so rust-analyzer must check its manifests itself.
    overflowed: bool,
    /// The watcher lost events; same consequence as `overflowed`.
    reload_all_rust: bool,
    extra_config_markers: Vec<String>,
}

impl WatcherForwardBacklog {
    /// Add one drain's changes. `extra_config_markers` replaces the previous
    /// value, so the latest configuration decides what counts as config.
    pub(crate) fn merge(
        &mut self,
        paths: &[PathBuf],
        reload_all_rust: bool,
        extra_config_markers: Vec<String>,
    ) {
        self.extra_config_markers = extra_config_markers;
        self.reload_all_rust |= reload_all_rust;
        for path in paths {
            if self.overflowed && !self.is_config(path) {
                continue;
            }
            if !self.seen.insert(path.clone()) {
                continue;
            }
            self.paths.push(path.clone());
            if self.paths.len() > WATCHER_FORWARD_BACKLOG_CAP {
                self.overflow();
            }
        }
    }

    fn is_config(&self, path: &Path) -> bool {
        is_config_file_path_with_custom(path, &self.extra_config_markers)
    }

    fn overflow(&mut self) {
        self.overflowed = true;
        let markers = std::mem::take(&mut self.extra_config_markers);
        self.paths
            .retain(|path| is_config_file_path_with_custom(path, &markers));
        self.extra_config_markers = markers;
        if self.paths.len() > WATCHER_FORWARD_BACKLOG_CAP {
            self.paths.clear();
        }
        self.seen = self.paths.iter().cloned().collect();
    }

    fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.overflowed && !self.reload_all_rust
    }

    /// Forward the backlog (see [`LspManager::forward_watcher_file_events`])
    /// and return the rust-analyzer reloads to start once the lock is
    /// released. Change kinds are read from disk now, not when queued, so a
    /// file created and deleted while waiting is reported as deleted.
    pub(crate) fn apply(self, lsp: &mut LspManager) -> Vec<(ServerKey, Vec<PathBuf>)> {
        let events: Vec<(PathBuf, FileChangeType)> = self
            .paths
            .into_iter()
            .map(|path| {
                let typ = if path.exists() {
                    FileChangeType::CHANGED
                } else {
                    FileChangeType::DELETED
                };
                (path, typ)
            })
            .collect();
        let mut reloads = lsp.forward_watcher_file_events(&events, &self.extra_config_markers);
        if self.overflowed || self.reload_all_rust {
            for key in lsp.rust_server_keys() {
                if !reloads.iter().any(|(known, _)| known == &key) {
                    reloads.push((key, Vec::new()));
                }
            }
        }
        reloads
    }
}

/// Body of the one helper thread serving a [`WatcherForwardSlot`]: wait for
/// the manager lock, forward everything queued, and repeat while drains
/// queued more meanwhile. Clearing the slot under its lock when nothing is
/// left is what lets the next contended drain start a new helper, so no
/// change is stranded and at most one helper exists at a time.
pub(crate) fn run_watcher_forward_helper(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    slot: &WatcherForwardSlot,
) {
    loop {
        let mut lsp = manager.lock();
        let backlog = {
            let mut guard = slot.lock();
            let Some(backlog) = guard.as_mut() else {
                return;
            };
            if backlog.is_empty() {
                *guard = None;
                return;
            }
            std::mem::take(backlog)
        };
        let reloads = backlog.apply(&mut lsp);
        drop(lsp);
        spawn_watcher_rust_workspace_reloads(manager, reloads);
    }
}

/// Start [`spawn_watcher_rust_workspace_reload`] for each returned server.
pub(crate) fn spawn_watcher_rust_workspace_reloads(
    manager: &Arc<parking_lot::Mutex<LspManager>>,
    reloads: Vec<(ServerKey, Vec<PathBuf>)>,
) {
    for (key, manifests) in reloads {
        spawn_watcher_rust_workspace_reload(Arc::clone(manager), key, manifests);
    }
}

/// How long a watcher-started rust-analyzer reload waits before checking the
/// manifests, so the several files one `cargo` command or checkout writes
/// lead to one reload rather than one per file.
const WATCHER_RUST_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// Whether `path` is one of the files that decide how Cargo loads the
/// workspace at `root` (the set [`rust_workspace_manifest_paths`] checks):
/// a `Cargo.toml`, `Cargo.lock`, toolchain file, or `.cargo/config(.toml)`.
/// Files under a build or VCS directory never are, whatever their name.
fn is_rust_workspace_manifest(root: &Path, path: &Path) -> bool {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let is_manifest = match file_name {
        "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml" => true,
        "config" | "config.toml" => path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir == ".cargo"),
        _ => false,
    };
    is_manifest
        && !path
            .strip_prefix(root)
            .unwrap_or(path)
            .components()
            .any(|component| {
                let name = component.as_os_str();
                name == "target" || name == ".git"
            })
}

/// Reload a running rust-analyzer's workspace on a helper thread after the
/// file watcher saw Cargo manifests, lockfiles, toolchain files or Cargo
/// configs change without rust-analyzer being told through a watcher it
/// registered. `manifests` lists those changed paths; it is empty when the
/// watcher's change set was too large to inspect per file or was lost
/// (a watcher overflow), in which case only the workspace root's files are
/// checked.
///
/// rust-analyzer re-reads its Cargo manifests only when it hears about a
/// change it registered for or is asked to reload. It registers no watcher
/// for toolchain files or Cargo config, and none at all while no workspace
/// is loaded. Without this, such a change made outside AFT (a `cargo`
/// command, a branch switch) left the old workspace, and its errors, in
/// place until the next `aft_inspect`, which runs the same reload check
/// before collecting diagnostics.
///
/// The reload itself is [`reload_rust_workspace_if_manifests_changed`], so
/// it happens only when a manifest really is newer than the server's last
/// load. Debounced per server: one thread per server at a time, and changes
/// seen while it runs are checked once more when it finishes. The thread
/// waits for the manager lock and the server's answer so the watcher drain
/// never does.
pub fn spawn_watcher_rust_workspace_reload(
    manager: Arc<parking_lot::Mutex<LspManager>>,
    key: ServerKey,
    manifests: Vec<PathBuf>,
) {
    let spawned = std::thread::Builder::new()
        .name("aft-rust-reload".into())
        .spawn(move || {
            if !manager.lock().claim_watcher_rust_reload(&key, &manifests) {
                return;
            }
            let mut scope_roots = manifests;
            loop {
                std::thread::sleep(WATCHER_RUST_RELOAD_DEBOUNCE);
                reload_rust_workspace_if_manifests_changed(&manager, &key, &scope_roots);
                match manager.lock().take_queued_watcher_rust_reload(&key) {
                    Some(queued) => scope_roots = queued,
                    None => return,
                }
            }
        });
    if let Err(error) = spawned {
        crate::slog_warn!("could not start a rust-analyzer workspace reload thread: {error}");
    }
}

/// Walk the inspected area and record, per server key, the first file a server
/// could analyze, plus the first root marker seen per server kind.
///
/// Selection is by source files only: a server is a candidate when a walked
/// file has one of its extensions and a workspace root can be found for that
/// file. A root marker such as `package.json` never selects a server by
/// itself; a marker whose server ends up with no file is reported in
/// `not_applicable` instead. The walked area is the scope when one is given
/// (each scope root, file or directory) and the whole project otherwise.
///
/// This reads no `LspManager` state, so callers run it without the manager
/// lock (see [`ApplicabilityWalk`]).
pub fn walk_applicable_area(
    project_root: &Path,
    scope_roots: Option<&[PathBuf]>,
    config: &Config,
    deadline: Option<std::time::Instant>,
) -> Result<ApplicabilityWalk, ApplicabilityResolutionError> {
    if !project_root.is_dir() {
        return Err(ApplicabilityResolutionError::RootUnreadable {
            root: project_root.to_path_buf(),
            reason: "project root is not a directory".to_string(),
        });
    }
    delay_applicability_walk_for_test();

    let walk_roots = match scope_roots.filter(|roots| !roots.is_empty()) {
        Some(roots) => roots.to_vec(),
        None => vec![project_root.to_path_buf()],
    };

    let mut seen = HashSet::<ServerKey>::new();
    let mut candidates = Vec::new();
    let mut markers = HashMap::<ServerKind, (ServerDef, PathBuf, String)>::new();
    // Prevent a disappearing child mount from making ReadDir::drop abort on ENXIO.
    let mut builder = ignore::WalkBuilder::new(&walk_roots[0]);
    for root in &walk_roots[1..] {
        builder.add(root);
    }
    let walker = builder
        .same_file_system(true)
        .standard_filters(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            !crate::lsp::roots::skip_in_server_walk(
                entry.file_name().to_string_lossy().as_ref(),
                entry.depth(),
            )
        })
        .build();

    for entry in walker {
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(ApplicabilityResolutionError::RequestDeadline {
                root: project_root.to_path_buf(),
            });
        }
        let entry = entry.map_err(|error| ApplicabilityResolutionError::RootUnreadable {
            root: project_root.to_path_buf(),
            reason: error.to_string(),
        })?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let file = entry.path();
        for definition in servers_with_root_marker(file, config) {
            if markers.contains_key(&definition.kind) {
                continue;
            }
            let marker_dir = file.parent().unwrap_or(project_root).to_path_buf();
            let marker_name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            markers.insert(
                definition.kind.clone(),
                (definition, marker_dir, marker_name),
            );
        }
        for definition in servers_for_file(file, config) {
            let Some(key) = server_key_for_definition(&definition, file, config) else {
                continue;
            };
            // Whether a key starts or fails depends only on the key and its
            // definition, so the first file seen for it decides, as it did
            // when the classification ran inside this loop.
            if !seen.insert(key.clone()) {
                continue;
            }
            candidates.push(ApplicableServerCandidate {
                key,
                definition,
                source_file: file.to_path_buf(),
            });
        }
    }

    Ok(ApplicabilityWalk {
        candidates,
        markers,
    })
}

/// Test hook: stretch the applicability walk so a test can prove the walk
/// does not block other users of the language-server manager. Debug builds
/// only. `AFT_TEST_APPLICABILITY_WALK_DELAY_MS` is the delay in milliseconds;
/// when `AFT_TEST_APPLICABILITY_WALK_SIGNAL` names a file, it is written as the
/// delay starts so the test knows the walk is under way.
fn delay_applicability_walk_for_test() {
    #[cfg(debug_assertions)]
    if let Some(delay_ms) = std::env::var("AFT_TEST_APPLICABILITY_WALK_DELAY_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        if let Some(signal) = std::env::var_os("AFT_TEST_APPLICABILITY_WALK_SIGNAL") {
            let _ = std::fs::write(signal, b"walking");
        }
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
}

/// Classify an error returned by `spawn_server` into a structured
/// `ServerAttemptResult`. The two interesting cases for callers are:
/// - `BinaryNotInstalled` — the server's binary couldn't be resolved on PATH
///   or via override. The agent can be told "install bash-language-server".
/// - `SpawnFailed` — binary was found but spawning/initializing failed
///   (permissions, missing runtime, server crashed during initialize, etc.).
fn classify_spawn_error(binary: &str, err: &LspError) -> ServerAttemptResult {
    match err {
        // resolve_binary returns NotFound for both missing override paths and
        // missing PATH binaries. The "override missing" case is rare in
        // practice (only set in tests / env vars); we report all NotFound as
        // BinaryNotInstalled so the user sees an actionable install hint.
        LspError::NotFound(_) => ServerAttemptResult::BinaryNotInstalled {
            binary: binary.to_string(),
        },
        other => ServerAttemptResult::SpawnFailed {
            binary: binary.to_string(),
            reason: other.to_string(),
        },
    }
}

fn env_binary_override(kind: &ServerKind) -> Option<PathBuf> {
    env_binary_override_from(kind, |key| std::env::var_os(key))
}

/// Classify an error raised while preparing a start, before any process
/// exists. Those errors are a binary that does not resolve or a check of
/// AFT's own refusing the server (an unusable project TypeScript, no
/// TypeScript SDK for astro-ls, a missing native TypeScript package); neither
/// changes until the environment does. Anything else is treated as transient.
fn prepare_failure(error: LspError) -> StartFailure {
    let durability = match error {
        LspError::NotFound(_) | LspError::ServerNotReady(_) => FailureDurability::Permanent,
        _ => FailureDurability::Transient,
    };
    StartFailure { error, durability }
}

/// Classify a failure to start the server process itself. A binary that is
/// missing or that the system refuses to execute stays that way; any other
/// I/O error may not recur.
fn process_start_durability(error: &LspError) -> FailureDurability {
    match error {
        LspError::NotFound(_) => FailureDurability::Permanent,
        LspError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            FailureDurability::Permanent
        }
        _ => FailureDurability::Transient,
    }
}

/// Classify a server that died or failed during or after `initialize` from
/// its failure text and stderr. Only configuration errors AFT recognises are
/// permanent: typescript-language-server finding no usable TypeScript SDK,
/// and a rustup proxy whose component is not installed. Anything else is
/// transient, because wrongly calling a failure permanent leaves the server
/// off for the whole session while a needless retry costs seconds.
fn exit_durability(reason: &str, stderr_tail: &str) -> FailureDurability {
    use super::environmental::{TS_LS5_NO_INSTALLATION, TS_LS6_NO_TSSERVER};

    let names_typescript_sdk_error = [reason, stderr_tail]
        .iter()
        .any(|text| text.contains(TS_LS5_NO_INSTALLATION) || text.contains(TS_LS6_NO_TSSERVER));
    if names_typescript_sdk_error || rustup_missing_component(stderr_tail).is_some() {
        FailureDurability::Permanent
    } else {
        FailureDurability::Transient
    }
}

fn env_binary_override_from(
    kind: &ServerKind,
    lookup: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let id = kind.id_str();
    let suffix: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let key = format!("AFT_LSP_{suffix}_BINARY");
    lookup(&key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod env_binary_override_tests {
    use super::*;

    #[test]
    fn empty_lsp_binary_override_is_unset_without_mutating_the_process_environment() {
        let kind = ServerKind::TypeScript;
        assert_eq!(
            env_binary_override_from(&kind, |key| {
                assert_eq!(key, "AFT_LSP_TYPESCRIPT_BINARY");
                Some(std::ffi::OsString::new())
            }),
            None
        );
        assert_eq!(
            env_binary_override_from(&kind, |_| Some(std::ffi::OsString::from("/bin/lsp"))),
            Some(PathBuf::from("/bin/lsp"))
        );
    }
}

#[cfg(test)]
mod language_id_tests {
    use super::language_id_for_extension;

    #[test]
    fn markdown_extensions_use_markdown_language_id() {
        for extension in [
            "md", "markdown", "mdx", "mkd", "mkdn", "mdown", "mdwn", "qmd", "rmd",
        ] {
            assert_eq!(language_id_for_extension(extension), "markdown");
        }
    }
}

#[cfg(all(test, windows))]
mod windows_server_key_tests {
    use std::fs;
    use std::os::windows::ffi::OsStrExt;

    use super::{canonicalize_for_lsp, server_key_for_definition};
    use crate::config::{Config, UserServerDef};
    use crate::lsp::registry::servers_for_file;

    #[test]
    fn normalized_and_verbatim_inputs_produce_identical_server_key_material() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("workspace");
        let source = root.join("src").join("main.customts");
        fs::create_dir_all(source.parent().expect("source parent")).expect("create source dir");
        fs::write(root.join("custom-root.json"), "{}\n").expect("write root marker");
        fs::write(&source, "export const value = 1;\n").expect("write source");

        let config = Config {
            project_root: Some(root),
            lsp_servers: vec![UserServerDef {
                id: "custom-ts".to_string(),
                extensions: vec!["customts".to_string()],
                binary: "custom-ts-lsp".to_string(),
                args: Vec::new(),
                root_markers: vec!["custom-root.json".to_string()],
                env: Default::default(),
                initialization_options: None,
                disabled: false,
            }],
            ..Config::default()
        };

        let normalized_input = canonicalize_for_lsp(&source).expect("normalized source path");
        let bare_canonical_input = fs::canonicalize(&source).expect("canonical source path");
        let key_for = |path: &std::path::Path| {
            let def = servers_for_file(path, &config)
                .into_iter()
                .find(|def| def.kind.id_str() == "custom-ts")
                .expect("custom server definition");
            server_key_for_definition(&def, path, &config).expect("custom server root")
        };

        let key_material = |key: &crate::lsp::roots::ServerKey| {
            let root_bytes = key
                .root
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            (key.kind.id_str().to_string(), root_bytes)
        };
        let ensure_key = key_for(&normalized_input);
        let running_lookup_key = key_for(&bare_canonical_input);

        assert_eq!(key_material(&ensure_key), key_material(&running_lookup_key));
    }
}

#[cfg(test)]
mod failure_hint_tests {
    use super::{failure_hint, rust_failure_with_root_cause, rustup_missing_component};

    /// rust-analyzer's bare "Failed to load workspaces." status gains the
    /// Cargo error line from its stderr; a status that already quotes Cargo's
    /// error is left alone.
    #[test]
    fn failed_workspace_load_quotes_the_cargo_error_line() {
        let stderr = "WARN `cargo metadata` failed\n\
            error: cannot update the lock file /x/Cargo.lock because --locked was passed to prevent this\n\
            help: to generate the lock file without accessing the network, remove the --locked flag\n\
            Stack backtrace:\n   0: frame";
        assert_eq!(
            rust_failure_with_root_cause("Failed to load workspaces.\n\n", stderr),
            "Failed to load workspaces. error: cannot update the lock file /x/Cargo.lock because --locked was passed to prevent this"
        );
        let detailed = "Failed to read Cargo metadata: error: cannot update the lock file";
        assert_eq!(rust_failure_with_root_cause(detailed, stderr), detailed);
        assert_eq!(
            rust_failure_with_root_cause("Failed to load workspaces.", ""),
            "Failed to load workspaces."
        );
    }

    #[test]
    fn detects_rustup_proxy_without_component() {
        // The exact rustup stderr for a proxy shim whose component is missing.
        let stderr = "error: Unknown binary 'rust-analyzer' in official toolchain 'stable-aarch64-apple-darwin'.";
        assert_eq!(
            rustup_missing_component(stderr).as_deref(),
            Some("rust-analyzer")
        );
        let hint = failure_hint("rust-analyzer", stderr);
        assert!(
            hint.contains("rustup component add rust-analyzer"),
            "expected actionable rustup hint, got: {hint}"
        );
    }

    #[test]
    fn ignores_unknown_binary_without_toolchain_phrasing() {
        // "Unknown binary" without the rustup toolchain phrasing must not be
        // misattributed to a rustup component issue.
        let stderr = "fatal: Unknown binary 'foo' was requested by the linker.";
        assert_eq!(rustup_missing_component(stderr), None);
        assert!(failure_hint("foo", stderr).starts_with("Hint: see stderr"));
    }

    #[test]
    fn npm_module_not_found_still_wins() {
        // The existing package-manager-shim case is unaffected.
        let stderr = "Error: Cannot find module '/x/typescript-language-server/lib/cli.mjs'";
        let hint = failure_hint("typescript-language-server", stderr);
        assert!(hint.contains("install -g"), "got: {hint}");
    }
}

#[cfg(test)]
mod transient_retry_tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;

    fn key() -> ServerKey {
        ServerKey {
            kind: ServerKind::Rust,
            root: PathBuf::from("/tmp/transient-retry-root"),
        }
    }

    fn spawn_failed() -> ServerAttemptResult {
        ServerAttemptResult::SpawnFailed {
            binary: "rust-analyzer".to_string(),
            reason: "server not ready: server failed during initialize: timeout".to_string(),
        }
    }

    /// A manager whose retry clock stands still until the returned offset
    /// (milliseconds) is advanced.
    fn manager_with_clock() -> (LspManager, Arc<AtomicU64>) {
        let origin = Instant::now();
        let offset = Arc::new(AtomicU64::new(0));
        let reader = Arc::clone(&offset);
        let mut manager = LspManager::new();
        manager.set_retry_clock(RetryClock::from_fn(move || {
            origin + Duration::from_millis(reader.load(Ordering::SeqCst))
        }));
        (manager, offset)
    }

    fn reason(result: &ServerAttemptResult) -> &str {
        match result {
            ServerAttemptResult::SpawnFailed { reason, .. } => reason,
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    /// Record one transient failure, then return the window it opened by
    /// stepping the clock until the failure stops being replayed.
    fn record_and_measure_window(manager: &mut LspManager, offset: &AtomicU64) -> Duration {
        let key = key();
        manager.record_failed_spawn(&key, spawn_failed(), FailureDurability::Transient);
        let started = offset.load(Ordering::SeqCst);
        let mut waited = 0;
        while manager.failure_to_replay(&key).is_some() {
            offset.fetch_add(1_000, Ordering::SeqCst);
            waited += 1_000;
            assert!(waited <= 3_600_000, "retry window never closed");
        }
        assert_eq!(offset.load(Ordering::SeqCst), started + waited);
        Duration::from_millis(waited)
    }

    #[test]
    fn transient_backoff_doubles_caps_and_resets_after_a_successful_start() {
        let (mut manager, offset) = manager_with_clock();
        let windows = (0..7)
            .map(|_| record_and_measure_window(&mut manager, &offset).as_secs())
            .collect::<Vec<_>>();
        assert_eq!(windows, vec![30, 60, 120, 240, 480, 600, 600]);

        manager.note_start_succeeded(&key());
        assert!(manager.failure_to_replay(&key()).is_none());
        assert_eq!(
            record_and_measure_window(&mut manager, &offset),
            Duration::from_secs(30),
            "a successful start must reset the backoff"
        );
    }

    #[test]
    fn transient_failure_reason_names_the_next_attempt() {
        let (mut manager, offset) = manager_with_clock();
        let reported =
            manager.record_failed_spawn(&key(), spawn_failed(), FailureDurability::Transient);
        assert_eq!(
            reason(&reported),
            "transient failure, will retry after 30s: server not ready: server failed during initialize: timeout"
        );
        offset.fetch_add(29_500, Ordering::SeqCst);
        let replayed = manager
            .failure_to_replay(&key())
            .expect("inside the window");
        assert!(
            reason(&replayed).starts_with("transient failure, will retry after 1s: "),
            "{replayed:?}"
        );
        // The wait shown in the first line survives inspect's one-line summary.
        assert!(replayed
            .failure_reason()
            .starts_with("transient failure, will retry after 1s: "));
        let at_boundary = FailedSpawn {
            result: spawn_failed(),
            retry_at: Some(Instant::now()),
        };
        assert!(reason(&at_boundary.reported_result(Instant::now()))
            .starts_with("transient failure, retrying now: "));
    }

    #[test]
    fn permanent_failure_replays_unchanged_long_after_any_backoff_window() {
        let (mut manager, offset) = manager_with_clock();
        let missing = ServerAttemptResult::BinaryNotInstalled {
            binary: "rust-analyzer".to_string(),
        };
        manager.record_failed_spawn(&key(), missing, FailureDurability::Permanent);
        offset.fetch_add(24 * 3_600_000, Ordering::SeqCst);
        assert!(matches!(
            manager.failure_to_replay(&key()),
            Some(ServerAttemptResult::BinaryNotInstalled { .. })
        ));
        // Configuration changes still clear it, together with any backoff.
        assert_eq!(manager.clear_failed_spawns(), 1);
        assert!(manager.failure_to_replay(&key()).is_none());
    }

    #[test]
    fn retry_delays_render_in_whole_seconds_rounded_up() {
        assert_eq!(format_retry_delay(Duration::from_millis(1)), "1s");
        assert_eq!(format_retry_delay(Duration::from_secs(45)), "45s");
        assert_eq!(format_retry_delay(Duration::from_secs(120)), "2m");
        assert_eq!(format_retry_delay(Duration::from_secs(270)), "4m 30s");
    }

    #[test]
    fn failure_durability_classification() {
        use FailureDurability::{Permanent, Transient};

        // Before any process starts.
        let not_found = LspError::NotFound("rust-analyzer".into());
        assert_eq!(prepare_failure(not_found).durability, Permanent);
        let refused = LspError::ServerNotReady("TypeScript unavailable".into());
        assert_eq!(prepare_failure(refused).durability, Permanent);

        // Starting the process.
        let io = |kind| LspError::Io(std::io::Error::from(kind));
        assert_eq!(
            process_start_durability(&io(std::io::ErrorKind::NotFound)),
            Permanent
        );
        assert_eq!(
            process_start_durability(&io(std::io::ErrorKind::PermissionDenied)),
            Permanent
        );
        assert_eq!(
            process_start_durability(&io(std::io::ErrorKind::BrokenPipe)),
            Transient
        );

        // A server that died during or after initialize.
        assert_eq!(
            exit_durability(
                "server failed during initialize: server error -32603: Could not find a valid TypeScript installation",
                ""
            ),
            Permanent
        );
        assert_eq!(
            exit_durability(
                "server crashed during initialize (exit 1)",
                "provides no tsserver.js"
            ),
            Permanent
        );
        assert_eq!(
            exit_durability(
                "server crashed during initialize (exit 1)",
                "error: Unknown binary 'rust-analyzer' in official toolchain 'stable-x86_64'"
            ),
            Permanent
        );
        assert_eq!(
            exit_durability("server crashed during initialize (exit 3)", "fatal: oops"),
            Transient
        );
        assert_eq!(
            exit_durability("server exited after initialize (signal 9): broken pipe", ""),
            Transient
        );
    }
}

#[cfg(test)]
mod diagnostic_capacity_tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{LspManager, INSPECT_CLOSED_DOCUMENTS_CAP};
    use crate::config::Config;
    use crate::lsp::registry::ServerKind;
    use crate::lsp::roots::ServerKey;

    // The lsp.diagnostic_cache_size config knob must actually take effect:
    // set_diagnostic_capacity (called at AppContext construction with the config
    // value) propagates the cap to the underlying DiagnosticsStore. Before this
    // wiring the field was parsed but never applied (always the hardcoded 5000).
    #[test]
    fn set_diagnostic_capacity_propagates_to_store() {
        let mut manager = LspManager::new();
        manager.set_diagnostic_capacity(7);
        assert_eq!(manager.diagnostics_store_for_test().capacity_for_test(), 7);
        manager.set_diagnostic_capacity(0); // 0 = unbounded
        assert_eq!(manager.diagnostics_store_for_test().capacity_for_test(), 0);
    }

    // A change sent to several servers records the file's disk state once,
    // not once per server.
    #[test]
    fn a_change_for_several_servers_reads_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("main.ts");
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        let keys: Vec<ServerKey> = [
            ServerKind::TypeScript,
            ServerKind::Biome,
            ServerKind::Oxlint,
        ]
        .into_iter()
        .map(|kind| ServerKey {
            kind,
            root: root.clone(),
        })
        .collect();
        let mut manager = LspManager::new();
        let reads = || crate::lsp::document::CONTENT_READS.with(std::cell::Cell::get);

        let before = reads();
        manager
            .notify_file_changed_for_server_keys(
                file.clone(),
                "export const x = 1;\n",
                keys.clone(),
            )
            .unwrap();
        assert_eq!(reads() - before, 1, "opening in three stores");

        std::fs::write(&file, "export const x = 2;\n").unwrap();
        let before = reads();
        manager
            .notify_file_changed_for_server_keys(file, "export const x = 2;\n", keys)
            .unwrap();
        assert_eq!(reads() - before, 1, "changing in three stores");
    }

    // Documents closed by scoped inspects are remembered only up to a cap,
    // newest kept, so a long session does not keep one entry per file.
    #[test]
    fn inspect_closed_documents_stay_bounded_and_keep_the_newest() {
        let mut manager = LspManager::new();
        let key = ServerKey {
            kind: ServerKind::TypeScript,
            root: PathBuf::from("/work"),
        };
        let total = INSPECT_CLOSED_DOCUMENTS_CAP * 3;
        for index in 0..total {
            manager.note_inspect_closed_document(&key, Path::new(&format!("/work/f{index}.ts")));
        }
        assert!(manager.inspect_closed_documents.len() <= INSPECT_CLOSED_DOCUMENTS_CAP);
        let newest = (
            key.clone(),
            PathBuf::from(format!("/work/f{}.ts", total - 1)),
        );
        assert!(manager.inspect_closed_documents.contains_key(&newest));
        let oldest = (key, PathBuf::from("/work/f0.ts"));
        assert!(!manager.inspect_closed_documents.contains_key(&oldest));
    }

    // configure clears cached spawn failures so a just-installed server retries
    // without a full restart.
    #[test]
    fn clear_failed_spawns_empties_the_cache() {
        let mut manager = LspManager::new();
        assert_eq!(manager.clear_failed_spawns(), 0);
        manager.insert_failed_spawn_for_test();
        assert_eq!(manager.clear_failed_spawns(), 1);
        assert_eq!(manager.clear_failed_spawns(), 0);
    }

    #[test]
    fn pushed_search_paths_make_new_binary_visible_to_stale_config() {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let binary_name = "aft-test-pushed-lsp";
        let binary = bin_dir.path().join(binary_name);
        fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config = Config {
            project_root: Some(root.path().to_path_buf()),
            lsp_servers: vec![crate::config::UserServerDef {
                id: "pushed-search-path-test".to_string(),
                extensions: vec!["pushedpath".to_string()],
                binary: binary_name.to_string(),
                args: Vec::new(),
                root_markers: Vec::new(),
                env: Default::default(),
                initialization_options: None,
                disabled: false,
            }],
            ..Config::default()
        };
        let file = root.path().join("sample.pushedpath");
        fs::write(&file, "test\n").unwrap();
        let definition = crate::lsp::registry::servers_for_file(&file, &config)
            .into_iter()
            .find(|definition| definition.kind.id_str() == "pushed-search-path-test")
            .unwrap();
        let mut manager = LspManager::new();

        assert!(manager
            .resolve_binary(&definition, root.path(), &config)
            .is_err());
        assert!(manager.set_search_paths(vec![bin_dir.path().to_path_buf()]));
        assert_eq!(
            manager
                .resolve_binary(&definition, root.path(), &config)
                .unwrap(),
            binary
        );
    }

    #[test]
    fn post_write_notification_does_not_start_a_cold_server() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.ts");
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        fs::write(&file, "export const value = 1;\n").unwrap();

        let mut manager = LspManager::new();
        manager
            .notify_file_changed_if_running(&file, "export const value = 1;\n", &Config::default())
            .unwrap();
        assert!(manager.clients.is_empty());
    }
}

#[cfg(test)]
mod unlocked_start_tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{LspManager, StartReservation, StartSignal};
    use crate::lsp::client::{LspEvent, ServerExitReason};
    use crate::lsp::registry::ServerKind;
    use crate::lsp::roots::ServerKey;

    // A server started without the manager lock has no client until its
    // handshake ends. Its events drained meanwhile must be kept, not handled
    // against a missing client (a rust-analyzer quiescence notice handled
    // then would be lost and the server would read as warming forever).
    #[test]
    fn events_for_a_starting_server_wait_for_its_start_to_finish() {
        let mut manager = LspManager::new();
        let key = ServerKey {
            kind: ServerKind::Rust,
            root: PathBuf::from("/held-start-root"),
        };
        let signal = Arc::new(StartSignal::default());
        manager.starting.insert(
            key.clone(),
            StartReservation {
                deferred_events: Vec::new(),
                clients_generation: manager.clients_generation,
                signal: Arc::clone(&signal),
            },
        );
        manager.enqueue_event_for_test(LspEvent::ServerExited {
            server_kind: key.kind.clone(),
            root: key.root.clone(),
            pid: 4242,
            reason: ServerExitReason::Eof,
        });

        manager.drain_events();
        assert!(
            manager.recent_server_exit_log_lines().is_empty(),
            "an event for a starting server was handled before its start finished"
        );
        assert!(!signal.wait_until(Instant::now() + Duration::from_millis(10)));

        manager.release_start_reservation(&key);
        let lines = manager.recent_server_exit_log_lines();
        assert_eq!(lines.len(), 1, "held event was not replayed: {lines:?}");
        assert!(
            lines[0].contains("pid=4242"),
            "unexpected line: {}",
            lines[0]
        );
        assert!(signal.wait_until(Instant::now()), "waiters were not woken");
    }
}

#[cfg(test)]
mod post_edit_waiter_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::LspManager;
    use crate::lsp::client::LspEvent;
    use crate::lsp::registry::ServerKind;

    #[test]
    fn draining_an_event_wakes_registered_post_edit_waiter() {
        let mut manager = LspManager::new();
        let mut wait = manager.start_post_edit_diagnostics_wait(
            PathBuf::from("/workspace/src/main.rs").as_path(),
            &[],
            &HashMap::new(),
            Duration::from_secs(2),
        );
        manager.enqueue_event_for_test(LspEvent::Notification {
            server_kind: ServerKind::Rust,
            root: PathBuf::from("/workspace"),
            method: "custom/drainedElsewhere".to_string(),
            params: None,
        });

        assert_eq!(manager.drain_events().events.len(), 1);
        let started = Instant::now();
        assert!(wait.next_event().is_none());
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a competing drain did not wake the parked post-edit waiter"
        );
        let _ = manager.poll_post_edit_diagnostics_wait(&mut wait, None);
        let _ = manager.finish_post_edit_diagnostics_wait(wait);
    }
}

#[cfg(test)]
mod clear_diagnostics_tests {
    use std::path::PathBuf;

    use super::{LspManager, PUBLISH_DIAGNOSTICS_PARSES};
    use crate::lsp::client::LspEvent;
    use crate::lsp::diagnostics::{DiagnosticSeverity, StoredDiagnostic};
    use crate::lsp::position::uri_for_path;
    use crate::lsp::registry::ServerKind;
    use crate::lsp::roots::ServerKey;

    fn err_diag(file: &PathBuf) -> StoredDiagnostic {
        StoredDiagnostic {
            file: file.clone(),
            line: 1,
            column: 1,
            end_line: 1,
            end_column: 2,
            severity: DiagnosticSeverity::Error,
            message: "boom".into(),
            code: None,
            source: None,
        }
    }

    // A just-deleted file can no longer be canonicalized directly, but its
    // store key was the canonical path from publish time. The manager must
    // reconstruct that key via the still-present parent dir so symlink-aliased
    // roots (macOS /var -> /private/var) still match and the diagnostic clears.
    #[test]
    fn clear_diagnostics_for_deleted_file_matches_canonical_key() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize the parent the way publish time would have.
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_file = canonical_dir.join("gone.ts");
        // Write then remove the file so its parent exists but the file does not,
        // mirroring the post-delete state the watcher observes.
        std::fs::write(&canonical_file, "x").unwrap();

        let mut manager = LspManager::new();
        let key = ServerKey {
            kind: ServerKind::TypeScript,
            root: canonical_dir.clone(),
        };
        manager.diagnostics_store_mut_for_test().publish(
            key,
            canonical_file.clone(),
            vec![err_diag(&canonical_file)],
        );
        assert_eq!(manager.warm_error_warning_counts(), (1, 0));

        std::fs::remove_file(&canonical_file).unwrap();

        // Clear by the NON-canonical path the watcher might hand us (the raw
        // tempdir path, which on macOS differs from the canonical /private form).
        let watcher_path = dir.path().join("gone.ts");
        let removed = manager.clear_diagnostics_for_file(&watcher_path);

        assert!(removed, "expected the deleted file's diagnostic to clear");
        assert_eq!(manager.warm_error_warning_counts(), (0, 0));
    }

    #[cfg(windows)]
    #[test]
    fn clear_diagnostics_for_deleted_file_matches_normalized_publish_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("normalized-gone.ts");
        std::fs::write(&file, "x").unwrap();
        let normalized_file = crate::inspect::job::canonicalize_normalized(&file);

        let mut manager = LspManager::new();
        let key = ServerKey {
            kind: ServerKind::TypeScript,
            root: normalized_file.parent().unwrap().to_path_buf(),
        };
        manager.diagnostics_store_mut_for_test().publish(
            key,
            normalized_file.clone(),
            vec![err_diag(&normalized_file)],
        );
        std::fs::remove_file(&file).unwrap();

        assert!(manager.clear_diagnostics_for_file(&file));
        assert_eq!(manager.warm_error_warning_counts(), (0, 0));
    }

    #[cfg(windows)]
    #[test]
    fn stale_diagnostics_for_deleted_file_matches_normalized_publish_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("normalized-stale.ts");
        std::fs::write(&file, "x").unwrap();
        let normalized_file = crate::inspect::job::canonicalize_normalized(&file);

        let mut manager = LspManager::new();
        let key = ServerKey {
            kind: ServerKind::TypeScript,
            root: normalized_file.parent().unwrap().to_path_buf(),
        };
        manager.diagnostics_store_mut_for_test().publish(
            key,
            normalized_file.clone(),
            vec![err_diag(&normalized_file)],
        );
        std::fs::remove_file(&file).unwrap();

        let result = manager.mark_diagnostics_stale_for_file(&file);
        assert!(result.had_entries);
        assert!(result.changed);
        assert_eq!(manager.warm_error_warning_counts(), (0, 0));
    }

    #[test]
    fn clear_diagnostics_for_unknown_file_is_noop() {
        let mut manager = LspManager::new();
        assert!(!manager.clear_diagnostics_for_file(&PathBuf::from("/nope/missing.ts")));
        assert_eq!(manager.warm_error_warning_counts(), (0, 0));
    }

    #[test]
    fn drain_events_reports_publish_diagnostics_updates() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let file = root.join("main.ts");
        std::fs::write(&file, "const x: number = 'nope';").unwrap();

        let mut manager = LspManager::new();
        let diagnostic = lsp_types::Diagnostic {
            range: lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: Some("test".into()),
            message: "boom".into(),
            related_information: None,
            tags: None,
            data: None,
        };
        let params = serde_json::to_value(lsp_types::PublishDiagnosticsParams {
            uri: uri_for_path(&file).unwrap(),
            diagnostics: vec![diagnostic],
            version: Some(1),
        })
        .unwrap();
        manager
            .event_tx
            .send(LspEvent::Notification {
                server_kind: ServerKind::TypeScript,
                root,
                method: "textDocument/publishDiagnostics".into(),
                params: Some(params),
            })
            .unwrap();

        let parses_before = PUBLISH_DIAGNOSTICS_PARSES.with(std::cell::Cell::get);
        let drained = manager.drain_events();
        let parses = PUBLISH_DIAGNOSTICS_PARSES.with(std::cell::Cell::get) - parses_before;

        assert!(drained.diagnostics_changed);
        assert_eq!(drained.events.len(), 1);
        assert_eq!(manager.warm_error_warning_counts(), (1, 0));
        // Storing the publish and building its accepted snapshot share one
        // typed parse of the payload.
        assert_eq!(parses, 1, "publishDiagnostics was parsed {parses} times");
    }
}

#[cfg(test)]
mod inspect_path_tests {
    use super::{walk_applicable_area, ApplicabilityWalk, LspManager};
    use crate::config::{Config, UserServerDef};
    use crate::lsp::registry::ServerKind;

    #[test]
    fn applicability_resolution_does_not_start_or_open_a_server() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        std::fs::create_dir_all(&root).expect("project root");
        std::fs::write(root.join("inspect-root.json"), "{}\n").expect("root marker");
        std::fs::write(root.join("input.inspectlang"), "value\n").expect("source file");

        let config = Config {
            project_root: Some(root.clone()),
            lsp_servers: vec![UserServerDef {
                id: "inspect-test".to_string(),
                extensions: vec!["inspectlang".to_string()],
                binary: "inspect-test-lsp".to_string(),
                args: Vec::new(),
                root_markers: vec!["inspect-root.json".to_string()],
                env: Default::default(),
                initialization_options: None,
                disabled: false,
            }],
            ..Config::default()
        };
        let mut manager = LspManager::new();
        manager.override_binary(
            ServerKind::Custom("inspect-test".into()),
            std::env::current_exe().expect("current executable"),
        );

        let snapshot = manager
            .resolve_applicable_servers_for_root(&root, &config)
            .expect("resolution succeeds without spawning");
        assert_eq!(snapshot.server_keys.len(), 1);
        assert_eq!(snapshot.server_keys[0].kind.id_str(), "inspect-test");
        assert_eq!(manager.server_count(), 0);
        assert!(!manager.document_is_open_for_test(&root.join("input.inspectlang")));
    }

    #[test]
    fn applicability_resolution_preserves_an_empty_snapshot() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        std::fs::create_dir_all(&root).expect("project root");
        std::fs::write(root.join("notes.txt"), "plain text\n").expect("fixture file");

        let snapshot = LspManager::new()
            .resolve_applicable_servers_for_root(&root, &Config::default())
            .expect("an empty applicability set is valid");

        assert!(snapshot.server_keys.is_empty());
        assert!(snapshot.candidates.is_empty());
    }

    /// The whole-project walk skips test fixtures, spikes, and ignored
    /// directories, each of which here is its own Cargo workspace that would
    /// otherwise get its own rust-analyzer. A request whose scope is one of
    /// those directories still starts the server for it.
    #[test]
    fn applicability_walk_skips_fixtures_spikes_and_ignored_directories() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = crate::inspect::job::canonicalize_normalized(temp_dir.path());
        let write = |relative: &str, contents: &str| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        let standalone = "[workspace]\n[package]\nname = \"x\"\nversion = \"0.1.0\"\n";
        write(
            "Cargo.toml",
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
        );
        write("src/lib.rs", "pub fn root() {}\n");
        for dir in [
            "spikes/foreign",
            "crates/app/tests/fixtures/bin_targets",
            "ignored/standalone",
        ] {
            write(&format!("{dir}/Cargo.toml"), standalone);
            write(&format!("{dir}/src/lib.rs"), "pub fn x() {}\n");
        }
        write(".aftignore", "ignored/\n");
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        let rust_roots = |walk: ApplicabilityWalk| {
            walk.candidates
                .into_iter()
                .filter(|candidate| candidate.key.kind == ServerKind::Rust)
                .map(|candidate| candidate.key.root)
                .collect::<Vec<_>>()
        };

        let whole = walk_applicable_area(&root, None, &config, None).expect("whole walk");
        assert_eq!(rust_roots(whole), vec![root.clone()]);

        let spike = root.join("spikes/foreign");
        let scoped = walk_applicable_area(&root, Some(&[spike.clone()]), &config, None)
            .expect("scoped walk");
        assert_eq!(rust_roots(scoped), vec![spike]);
    }
}

// Every test in this module spawns a real child process, so the module is Unix-only.
#[cfg(all(test, unix))]
mod server_exit_reap_tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::LspManager;
    use crate::config::Config;
    use crate::lsp::child_registry::LspChildRegistry;
    use crate::lsp::client::{LspClient, LspEvent, ServerExitReason};
    use crate::lsp::registry::ServerKind;

    #[cfg(unix)]
    fn spawn_session_sleep() -> Child {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().expect("spawn session-leader sleep")
    }

    #[cfg(unix)]
    fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if !crate::bash_background::process::is_process_alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[cfg(unix)]
    fn spawn_malformed_then_sleep_client(
        event_tx: crossbeam_channel::Sender<LspEvent>,
        registry: LspChildRegistry,
        root: std::path::PathBuf,
    ) -> LspClient {
        LspClient::spawn(
            ServerKind::TypeScript,
            root,
            Path::new("sh"),
            &[
                "-c".to_string(),
                "printf 'Content-Length: 3\r\n\r\n{{{'; exec sleep 60".to_string(),
            ],
            &HashMap::new(),
            event_tx,
            registry,
        )
        .expect("spawn malformed-then-sleep stand-in")
    }

    #[cfg(unix)]
    #[test]
    fn server_exited_handler_kills_live_child_and_untracks() {
        let registry = LspChildRegistry::new();
        let mut manager = LspManager::new();
        manager.set_child_registry(registry.clone());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mut client = spawn_malformed_then_sleep_client(
            manager.event_sender_for_test(),
            registry.clone(),
            root.clone(),
        );
        let pid = client.child_pid();
        client.suppress_kill_on_drop_for_test();
        manager.insert_client_for_test(client);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_exit = false;
        while Instant::now() < deadline {
            let drained = manager.drain_events();
            if drained.events.iter().any(|event| {
                matches!(
                    event,
                    LspEvent::ServerExited {
                        reason: ServerExitReason::ReadError(_),
                        ..
                    }
                )
            }) {
                saw_exit = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(saw_exit, "reader must emit ServerExited with ReadError");
        assert!(
            wait_until_dead(pid, Duration::from_secs(5)),
            "ServerExited handler must kill the still-running child"
        );
        assert!(
            !registry.pids().contains(&pid),
            "ServerExited handler must untrack the child"
        );
        assert_eq!(manager.active_client_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_census_reports_dropped_client_until_reaper_cleans_child() {
        let registry = LspChildRegistry::new();
        let (events, _event_rx) = crossbeam_channel::unbounded();
        let root = tempfile::tempdir().expect("tempdir");
        let mut client =
            spawn_malformed_then_sleep_client(events, registry.clone(), root.path().to_path_buf());
        client.suppress_kill_on_drop_for_test();
        drop(client);

        let leaked = registry.health_snapshot();
        assert_eq!(leaked.children_without_client, 1);
        assert_eq!(leaked.children_total, 1);
        assert_eq!(registry.reap_children_without_client(), 1);
        assert_eq!(registry.health_snapshot().children_without_client, 0);
        assert_eq!(registry.health_snapshot().children_total, 0);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_server_reaps_unreferenced_children_before_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspace");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        let main_rs = src.join("main.rs");
        std::fs::write(&main_rs, "fn main() {}\n").unwrap();
        let config = Config::default();
        let rust_def = crate::lsp::registry::servers_for_file(&main_rs, &config)
            .into_iter()
            .find(|def| matches!(def.kind, ServerKind::Rust))
            .expect("rust server applies to main.rs");
        let key = super::server_key_for_definition(&rust_def, &main_rs, &config)
            .expect("rust workspace root");

        let registry = LspChildRegistry::new();
        let mut first = spawn_session_sleep();
        let mut second = spawn_session_sleep();
        let pid1 = first.id();
        let pid2 = second.id();
        registry.track_child(pid1, Some(&key.root), Some(&key.root), Some(&key.kind));
        registry.track_child(pid2, Some(&key.root), Some(&key.root), Some(&key.kind));

        let mut manager = LspManager::new();
        manager.set_child_registry(registry.clone());
        manager.override_binary(ServerKind::Rust, Path::new("false").to_path_buf());
        assert_eq!(
            registry.pids_for_server(&key.root, &key.kind).len(),
            2,
            "both orphans must be registered under the spawn key"
        );
        let _ = manager.ensure_server_for_file(&main_rs, &config);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let first_exited = first.try_wait().ok().flatten().is_some();
            let second_exited = second.try_wait().ok().flatten().is_some();
            if first_exited && second_exited {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "orphans must exit after rebind reap (first alive={}, second alive={})",
                crate::bash_background::process::is_process_alive(pid1),
                crate::bash_background::process::is_process_alive(pid2)
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !registry.pids().contains(&pid1) && !registry.pids().contains(&pid2),
            "reaped orphans must be untracked before the new spawn is tracked"
        );
    }
}

#[cfg(test)]
mod typescript_worktree_tests {
    use super::*;

    #[test]
    fn biome_initialize_failure_preserves_cause_and_explains_worktree_remedy() {
        let reason = biome_unavailable_reason("server closed stream during initialize");
        assert!(reason.contains("Biome unavailable: server closed stream during initialize"));
        assert!(reason.contains("biome is not installed in this worktree, run bun install"));
    }

    #[test]
    fn typescript_sdk_unavailable_requires_actual_server_error() {
        let missing = typescript_initialize_failure_reason("initialize failed: Could not find a valid TypeScript installation. Please ensure that the typescript dependency is installed".into(), None);
        assert!(missing.starts_with("TypeScript SDK unavailable:"));
        assert!(missing.contains("run bun install"));
        let unrelated = "initialize failed: connection closed";
        assert_eq!(
            typescript_initialize_failure_reason(unrelated.into(), None),
            unrelated
        );
    }

    const LS6_NO_TSSERVER: &str = "server failed during initialize: server error -32603: Request initialize failed with message: The TypeScript of the workspace (TypeScript 7.0.2 at \"/repo/node_modules/typescript/lib\") provides no tsserver.js. No other valid TypeScript installation was found. Exiting.";
    const LS5_NO_INSTALLATION: &str = "server failed during initialize: server error -32603: Request initialize failed with message: Could not find a valid TypeScript installation. Please ensure that the \"typescript\" dependency is installed in the workspace or that a valid `tsserver.path` is specified. Exiting.";

    fn project_ts(version: &str) -> ProjectTypeScript {
        ProjectTypeScript {
            version: version.into(),
            package_dir: PathBuf::from("/repo/node_modules/typescript"),
        }
    }

    #[test]
    fn typescript_language_server_6_wording_is_explained_without_install_advice() {
        let reason = typescript_initialize_failure_reason(LS6_NO_TSSERVER.into(), None);
        assert!(
            reason.starts_with("TypeScript SDK unavailable:"),
            "{reason}"
        );
        assert!(reason.contains("has no tsserver.js"), "{reason}");
        assert!(!reason.contains("run bun install"), "{reason}");
        assert!(reason.ends_with(LS6_NO_TSSERVER), "{reason}");
    }

    #[test]
    fn typescript_7_project_is_named_for_both_server_wordings() {
        let ts7 = project_ts("7.0.2");
        for raw in [LS5_NO_INSTALLATION, LS6_NO_TSSERVER] {
            let reason = typescript_initialize_failure_reason(raw.into(), Some(&ts7));
            assert!(
                reason.starts_with("TypeScript unavailable: this project uses TypeScript 7.0.2 (native compiler) at /repo/node_modules/typescript, which has no tsserver; typescript-language-server can't serve it."),
                "{reason}"
            );
            assert!(!reason.contains("run bun install"), "{reason}");
            assert!(!reason.contains("auto-install"), "{reason}");
            assert!(reason.ends_with(raw), "{reason}");
        }
        // Development builds of the native compiler count too.
        let dev = typescript_initialize_failure_reason(
            LS6_NO_TSSERVER.into(),
            Some(&project_ts("7.1.0-dev.20260929.1")),
        );
        assert!(dev.contains("(native compiler)"), "{dev}");
    }

    #[test]
    fn typescript_5_project_keeps_the_install_advice() {
        let reason = typescript_initialize_failure_reason(
            LS5_NO_INSTALLATION.into(),
            Some(&project_ts("5.9.3")),
        );
        assert!(
            reason.starts_with("TypeScript SDK unavailable:"),
            "{reason}"
        );
        assert!(reason.contains("run bun install"), "{reason}");
    }

    #[test]
    fn project_typescript_package_is_read_from_node_modules_not_the_lockfile() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let file = project.join("src").join("index.ts");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();
        // A lockfile alone is not an installation.
        std::fs::write(
            project.join("bun.lock"),
            r#"{"packages":{"typescript":["typescript@7.0.2"]}}"#,
        )
        .unwrap();
        assert_eq!(find_project_typescript_package(&file, &project), None);

        // TypeScript 7 ships no lib/*.js the SDK probe needs, only package.json.
        let package_dir = project.join("node_modules").join("typescript");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"typescript","version":"7.0.2"}"#,
        )
        .unwrap();
        let found = find_project_typescript_package(&file, &project).unwrap();
        assert_eq!(found.version, "7.0.2");
        assert_eq!(found.major(), Some(7));
        assert_eq!(found.package_dir, package_dir);
    }

    fn sdk(root: &Path, version: &str) -> PathBuf {
        let lib = root.join("node_modules").join("typescript").join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("typescript.js"), "").unwrap();
        std::fs::write(lib.join("tsserver.js"), "").unwrap();
        std::fs::write(
            lib.parent().unwrap().join("package.json"),
            format!(r#"{{"version":"{version}"}}"#),
        )
        .unwrap();
        lib
    }

    #[test]
    fn fresh_worktree_typescript_uses_cache_without_writing_project() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("fresh");
        std::fs::create_dir(&project).unwrap();
        let file = project.join("index.ts");
        std::fs::write(&file, "const x: number = 'wrong';").unwrap();
        let cache = temp.path().join("cache");
        let lib = sdk(&cache, "5.9.3");
        let config = Config {
            project_root: Some(project.clone()),
            lsp_paths_extra: vec![cache.join("node_modules").join(".bin")],
            ..Config::default()
        };
        let (options, note) = typescript_runtime_options(None, &file, &project, &config).unwrap();
        assert_eq!(
            options["tsserver"]["path"],
            lib.join("tsserver.js").to_str().unwrap()
        );
        assert!(
            note.contains(
                "TypeScript 5.9.3: AFT cache fallback; not the project's pinned TypeScript"
            ),
            "{note}"
        );
        assert_eq!(std::fs::read_dir(&project).unwrap().count(), 1);
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "const x: number = 'wrong';"
        );
    }

    #[test]
    fn typescript_7_project_does_not_fall_back_to_the_cached_sdk() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("ts7");
        // TypeScript 7 installs package.json but none of the lib/*.js files.
        let package_dir = project.join("node_modules").join("typescript");
        std::fs::create_dir_all(package_dir.join("lib")).unwrap();
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"typescript","version":"7.0.2"}"#,
        )
        .unwrap();
        let file = project.join("index.ts");
        let cache = temp.path().join("cache");
        sdk(&cache, "5.9.3");
        let config = Config {
            project_root: Some(project.clone()),
            lsp_paths_extra: vec![cache.join("node_modules").join(".bin")],
            ..Config::default()
        };
        let (options, note) = typescript_runtime_options(None, &file, &project, &config).unwrap();
        assert!(options.pointer("/tsserver/path").is_none(), "{options}");
        assert!(
            note.starts_with("TypeScript 7.0.2: native compiler at "),
            "{note}"
        );
        assert!(note.contains("AFT cache fallback not used"), "{note}");

        // The same layout on TypeScript 5 is a broken install without
        // tsserver.js. The cached SDK would report another compiler's
        // results for it, so the installation is named instead.
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"typescript","version":"5.9.3"}"#,
        )
        .unwrap();
        let error = typescript_runtime_options(None, &file, &project, &config).unwrap_err();
        let LspError::ServerNotReady(reason) = error else {
            panic!("expected a named gap, got {error}");
        };
        assert!(
            reason.contains("TypeScript 5.9.3 at ") && reason.contains("has no lib/tsserver.js"),
            "{reason}"
        );

        // An unreadable version is reported as a named gap, never guessed.
        std::fs::write(package_dir.join("package.json"), r#"{"name":"typescript"}"#).unwrap();
        let error = typescript_runtime_options(None, &file, &project, &config).unwrap_err();
        assert!(
            error.to_string().contains("reports version \"unknown\""),
            "{error}"
        );
    }

    #[test]
    fn project_typescript_precedes_cache_and_explicit_override_precedes_both() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let local = sdk(&project, "5.8.3");
        let cache = temp.path().join("cache");
        sdk(&cache, "5.9.3");
        let file = project.join("index.ts");
        let config = Config {
            project_root: Some(project.clone()),
            lsp_paths_extra: vec![cache.join("node_modules").join(".bin")],
            ..Config::default()
        };
        let (options, note) = typescript_runtime_options(None, &file, &project, &config).unwrap();
        assert_eq!(
            options["tsserver"]["path"],
            local.join("tsserver.js").to_str().unwrap()
        );
        assert!(note.contains("5.8.3: project installation"));
        let configured = serde_json::json!({"tsserver":{"path":"/custom/tsserver.js"},"preferences":{"quotePreference":"single"}});
        let (options, _) =
            typescript_runtime_options(Some(configured.clone()), &file, &project, &config).unwrap();
        assert_eq!(options, configured);
    }

    #[test]
    fn missing_local_and_cached_sdk_preserves_server_discovery_without_writing_project() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            project_root: Some(temp.path().to_path_buf()),
            ..Config::default()
        };
        let configured = serde_json::json!({"preferences": {"quotePreference": "single"}});
        let (options, note) = typescript_runtime_options(
            Some(configured.clone()),
            &temp.path().join("index.ts"),
            temp.path(),
            &config,
        )
        .unwrap();
        assert_eq!(options, configured);
        assert!(note.contains("server-managed SDK resolution"), "{note}");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    /// Cost of choosing the TypeScript server for every file of a real
    /// project, through the same applicability walk `aft_inspect` runs. Opt-in:
    /// `AFT_TS_SELECTION_CORPUS=<project root> cargo test -p agent-file-tools
    /// --lib measure_typescript_selection -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_typescript_selection_on_a_corpus() {
        let Some(root) = std::env::var_os("AFT_TS_SELECTION_CORPUS").map(PathBuf::from) else {
            eprintln!("SKIP: AFT_TS_SELECTION_CORPUS not set");
            return;
        };
        let root = crate::inspect::job::canonicalize_normalized(&root);
        let config = Config {
            project_root: Some(root.clone()),
            ..Config::default()
        };
        for round in 1..=3 {
            let reads = crate::lsp::typescript_project::package_json_reads_on_this_thread();
            let started = Instant::now();
            let walk = walk_applicable_area(&root, None, &config, None).unwrap();
            eprintln!(
                "round {round}: walk {:?}, {} server keys, {} package.json reads",
                started.elapsed(),
                walk.candidates.len(),
                crate::lsp::typescript_project::package_json_reads_on_this_thread() - reads
            );
        }
    }
}

#[cfg(test)]
mod rust_initialization_options_tests {
    use super::*;

    #[test]
    fn rust_target_dir_default_preserves_configured_options() {
        let config = Config::default();
        let source = Path::new("src/lib.rs");
        let root = Path::new(".");
        let mut def = super::super::registry::servers_for_file(source, &config)
            .into_iter()
            .find(|def| def.kind == ServerKind::Rust)
            .expect("built-in rust-analyzer");
        let options = initialization_options_for_spawn(&def, source, root, &config)
            .unwrap()
            .unwrap();
        assert_eq!(options["cargo"]["targetDir"], true);
        assert_eq!(
            options["cargo"]["extraArgs"],
            serde_json::json!(["--locked"])
        );
        assert_eq!(
            options["cargo"]["metadataExtraArgs"],
            serde_json::json!(["--locked"])
        );
        for target in [serde_json::json!("custom-target"), serde_json::json!(false)] {
            def.initialization_options = Some(serde_json::json!({
                "cargo": {"targetDir": target, "extraArgs": ["--offline"]},
                "checkOnSave": false,
                "check": {"workspace": false}
            }));
            let options = initialization_options_for_spawn(&def, source, root, &config)
                .unwrap()
                .unwrap();
            assert_eq!(options, def.initialization_options.clone().unwrap());
        }
        def.kind = ServerKind::Python;
        def.initialization_options = None;
        assert_eq!(
            initialization_options_for_spawn(&def, source, root, &config).unwrap(),
            None
        );
    }
}
