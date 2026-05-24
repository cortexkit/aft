use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::lsp::child_registry::LspChildRegistry;
use crate::lsp::jsonrpc::{
    Notification, Request, RequestId, Response as JsonRpcResponse, ServerMessage,
};
use crate::lsp::position::path_to_uri;
use crate::lsp::registry::ServerKind;
use crate::lsp::{transport, LspError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

type PendingMap = HashMap<RequestId, Sender<JsonRpcResponse>>;
type WatchedFileRegistrations = Arc<Mutex<HashSet<String>>>;

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
#[derive(Debug)]
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
    },
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

/// A client connected to one language server process.
pub struct LspClient {
    kind: ServerKind,
    root: PathBuf,
    state: ServerState,
    child: Child,
    /// Child PID captured at spawn time. Used by Drop to untrack the
    /// PID from the shared registry; we capture once rather than reading
    /// `child.id()` later because Drop ordering with the Child can race.
    child_pid: u32,
    writer: Arc<Mutex<BufWriter<std::process::ChildStdin>>>,

    /// Pending request responses, keyed by request ID.
    pending: Arc<Mutex<PendingMap>>,
    /// Next request ID counter.
    next_id: AtomicI64,
    /// Diagnostic capabilities reported by the server in its initialize response.
    /// `None` until `initialize()` succeeds; conservative defaults thereafter
    /// when the server doesn't advertise diagnosticProvider.
    diagnostic_caps: Option<ServerDiagnosticCapabilities>,
    /// Whether the server advertised `workspace.didChangeWatchedFiles` support
    /// during `initialize`. When `false` (or `None` pre-init), we skip sending
    /// `workspace/didChangeWatchedFiles` notifications to avoid spec violations.
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
        let mut command = Command::new(binary);
        command
            .args(args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Use null() instead of piped() to prevent deadlock when the server
            // writes more than ~64KB to stderr (piped buffer fills, server blocks)
            .stderr(Stdio::null());
        for (key, value) in env {
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
                    // If aft is killed with SIGKILL, Rust cleanup and our
                    // signal-handler thread never run. Ask the kernel to kill
                    // the LSP process group as soon as the parent dies. This is
                    // best-effort Linux coverage for the otherwise unhandleable
                    // parent-death path.
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

        let mut child = child_registry.spawn_tracked(&mut command)?;
        let child_pid = child.id();

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("language server missing stdout pipe"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("language server missing stdin pipe"))?;

        let writer = Arc::new(Mutex::new(BufWriter::new(stdin)));
        let pending = Arc::new(Mutex::new(PendingMap::new()));
        let watched_file_registrations = Arc::new(Mutex::new(HashSet::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_writer = Arc::clone(&writer);
        let reader_watched_file_registrations = Arc::clone(&watched_file_registrations);
        let reader_kind = kind.clone();
        let reader_root = root.clone();

        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match transport::read_message(&mut reader) {
                    Ok(Some(ServerMessage::Response(response))) => {
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
                            });
                            break;
                        }
                    }
                    Ok(Some(ServerMessage::Notification { method, params })) => {
                        let _ = event_tx.send(LspEvent::Notification {
                            server_kind: reader_kind.clone(),
                            root: reader_root.clone(),
                            method,
                            params,
                        });
                    }
                    Ok(Some(ServerMessage::Request { id, method, params })) => {
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
                            // Return an array of null configs — one per requested item.
                            // Servers fall back to filesystem config (tsconfig, pyrightconfig, etc.)
                            let item_count = params
                                .as_ref()
                                .and_then(|p| p.get("items"))
                                .and_then(|items| items.as_array())
                                .map_or(1, |arr| arr.len());
                            serde_json::Value::Array(vec![serde_json::Value::Null; item_count])
                        } else {
                            serde_json::Value::Null
                        };
                        if let Ok(mut w) = reader_writer.lock() {
                            let response = super::jsonrpc::OutgoingResponse::success(
                                id.clone(),
                                response_value,
                            );
                            let _ = transport::write_response(&mut *w, &response);
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
                    Ok(None) | Err(_) => {
                        if let Ok(mut guard) = reader_pending.lock() {
                            guard.clear();
                        }
                        let _ = event_tx.send(LspEvent::ServerExited {
                            server_kind: reader_kind.clone(),
                            root: reader_root.clone(),
                        });
                        break;
                    }
                }
            }
        });

        Ok(Self {
            kind,
            root,
            state: ServerState::Starting,
            child,
            child_pid,
            writer,
            pending,
            next_id: AtomicI64::new(1),
            diagnostic_caps: None,
            supports_watched_files: false,
            watched_file_registrations,
            child_registry,
        })
    }

    /// Send the initialize request and wait for response. Transition to Ready.
    pub fn initialize(
        &mut self,
        workspace_root: &Path,
        initialization_options: Option<serde_json::Value>,
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
        if let Some(initialization_options) = initialization_options {
            params_value["initializationOptions"] = initialization_options;
        }

        let params = serde_json::from_value::<lsp_types::InitializeParams>(params_value)?;

        let result_value = self.send_request_value(
            <lsp_types::request::Initialize as lsp_types::request::Request>::METHOD,
            params,
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

        // Capture whether the server supports workspace/didChangeWatchedFiles.
        // Missing capability is unsupported by default; callers must not send
        // notifications unless the server explicitly opted in.
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

    /// Whether the server supports `workspace/didChangeWatchedFiles`.
    /// Captured from the `initialize` response. Default `false` (conservative).
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
        self.send_request_value_with_timeout(method, params, REQUEST_TIMEOUT)
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
        self.ensure_can_send()?;

        let id = RequestId::Int(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = bounded(1);
        {
            let mut pending = self.lock_pending()?;
            pending.insert(id.clone(), tx);
        }

        let request = Request::new(id.clone(), method, Some(serde_json::to_value(params)?));
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| LspError::ServerNotReady("writer lock poisoned".to_string()))?;
            if let Err(err) = transport::write_request(&mut *writer, &request) {
                self.remove_pending(&id);
                return Err(err.into());
            }
        }

        let response = match rx.recv_timeout(timeout) {
            Ok(response) => response,
            Err(RecvTimeoutError::Timeout) => {
                self.remove_pending(&id);
                self.send_cancel_request(&id)?;
                return Err(LspError::Timeout(format!(
                    "timed out waiting for '{}' response from {:?}",
                    method, self.kind
                )));
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.remove_pending(&id);
                return Err(LspError::ServerNotReady(format!(
                    "language server {:?} disconnected while waiting for '{}'",
                    self.kind, method
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

    /// Send a notification (fire-and-forget).
    pub fn send_notification<N>(&mut self, params: N::Params) -> Result<(), LspError>
    where
        N: lsp_types::notification::Notification,
        N::Params: serde::Serialize,
    {
        self.ensure_can_send()?;
        let notification = Notification::new(N::METHOD, Some(serde_json::to_value(params)?));
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| LspError::ServerNotReady("writer lock poisoned".to_string()))?;
        transport::write_notification(&mut *writer, &notification)?;
        Ok(())
    }

    /// Graceful shutdown: send shutdown request, then exit notification.
    pub fn shutdown(&mut self) -> Result<(), LspError> {
        if self.state == ServerState::Exited {
            self.child_registry.untrack(self.child_pid);
            return Ok(());
        }

        if self.child.try_wait()?.is_some() {
            self.state = ServerState::Exited;
            self.child_registry.untrack(self.child_pid);
            return Ok(());
        }

        if let Err(err) = self.send_request::<lsp_types::request::Shutdown>(()) {
            self.state = ServerState::ShuttingDown;
            if self.child.try_wait()?.is_some() {
                self.state = ServerState::Exited;
                return Ok(());
            }
            return Err(err);
        }

        self.state = ServerState::ShuttingDown;

        if let Err(err) = self.send_notification::<lsp_types::notification::Exit>(()) {
            if self.child.try_wait()?.is_some() {
                self.state = ServerState::Exited;
                return Ok(());
            }
            return Err(err);
        }

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
                return Err(LspError::Timeout(format!(
                    "timed out waiting for {:?} to exit",
                    self.kind
                )));
            }
            thread::sleep(EXIT_POLL_INTERVAL);
        }
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

    fn send_cancel_request(&mut self, id: &RequestId) -> Result<(), LspError> {
        let notification = Notification::new("$/cancelRequest", Some(json!({ "id": id })));
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| LspError::ServerNotReady("writer lock poisoned".to_string()))?;
        transport::write_notification(&mut *writer, &notification)?;
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Untrack first so the signal handler can't race with this kill and
        // try to SIGKILL a PID that's already been reaped.
        self.child_registry.untrack(self.child_pid);
        kill_lsp_child_group(&mut self.child);
    }
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
                            guard.insert(id.to_string());
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
}
