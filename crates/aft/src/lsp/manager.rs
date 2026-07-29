use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument,
};
use lsp_types::{
    DidChangeTextDocumentParams, DidChangeWatchedFilesParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, FileChangeType, FileEvent, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, VersionedTextDocumentIdentifier,
};

use crate::config::Config;
use crate::lsp::child_registry::LspChildRegistry;
use crate::lsp::client::{LspClient, LspEvent, ServerState};
use crate::lsp::diagnostics::{
    from_lsp_diagnostics, DiagnosticEntry, DiagnosticsStore, StoredDiagnostic,
};
use crate::lsp::document::DocumentStore;
use crate::lsp::position::{uri_for_path, uri_to_path};
use crate::lsp::pull_params::{
    AftDocumentDiagnosticParams, AftDocumentDiagnosticRequest, AftWorkspaceDiagnosticParams,
    AftWorkspaceDiagnosticRequest,
};
use crate::lsp::registry::{resolve_lsp_binary, servers_for_file, ServerDef, ServerKind};
use crate::lsp::roots::ServerKey;
use crate::lsp::LspError;
use crate::slog_error;

const STDERR_REASON_BYTES: usize = 2 * 1024;

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
/// `diagnostics` only contains entries from servers that proved freshness
/// (version-match preferred, epoch-fallback for unversioned servers).
/// Pre-edit cached entries are NEVER included — that's the whole point of
/// this type.
#[derive(Debug, Clone, Default)]
pub struct PostEditWaitOutcome {
    /// Diagnostics from servers whose response we verified is FOR the
    /// post-edit document version (or whose epoch we saw advance after our
    /// pre-edit snapshot, for unversioned servers).
    pub diagnostics: Vec<StoredDiagnostic>,
    /// Servers we expected to publish but didn't before the deadline.
    /// Reported to the agent via `pending_lsp_servers` so they understand
    /// the result is partial.
    pub pending_servers: Vec<ServerKey>,
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
}

pub(crate) struct TrackedPullFileResult {
    pub results: Vec<PullFileResult>,
    pub newly_opened: Vec<ServerKey>,
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
    pub has_more: bool,
}

impl IntoIterator for DrainedLspEvents {
    type Item = LspEvent;
    type IntoIter = std::vec::IntoIter<LspEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.events.into_iter()
    }
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
    /// Optional binary path overrides used by integration tests.
    binary_overrides: HashMap<ServerKind, PathBuf>,
    /// Extra env vars merged into every spawned LSP child. Used in tests to
    /// drive the fake server's behavioral variants (`AFT_FAKE_LSP_PULL=1`,
    /// `AFT_FAKE_LSP_WORKSPACE=1`, etc.). Production code does not set this.
    extra_env: HashMap<String, String>,
    /// Per-(kind,root) cache of spawn failures. Once a server fails to spawn
    /// for a workspace root, we remember why and skip subsequent attempts for
    /// the lifetime of this AFT process. Without this, every file open or
    /// didChange retries `spawn_server` and logs a fresh ERROR — visible as
    /// repeated `failed to spawn TypeScript Language Server: Could not find a
    /// valid TypeScript installation` lines per edit.
    ///
    /// Entries are NEVER evicted automatically. The expected recovery path is
    /// for the user to fix their environment (install the missing binary or
    /// add a `tsconfig.json` / `package.json` with the right dependency) and
    /// restart OpenCode/Pi, which spawns a fresh `aft` process with an empty
    /// cache. We deliberately don't auto-retry on file events: the failure
    /// modes we track here (binary not installed, init handshake failure)
    /// don't fix themselves at runtime.
    failed_spawns: HashMap<ServerKey, ServerAttemptResult>,
    /// Server/root pairs for which we already logged that watched-file
    /// notifications are skipped because the capability is absent.
    watched_file_skip_logged: HashSet<ServerKey>,
    /// Tracks PIDs of spawned LSP child processes so the signal handler can
    /// kill them on SIGTERM/SIGINT before aft exits, preventing orphans.
    /// Defaults to empty; production wires this from `AppContext`.
    child_registry: LspChildRegistry,
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
            binary_overrides: HashMap::new(),
            extra_env: HashMap::new(),
            failed_spawns: HashMap::new(),
            watched_file_skip_logged: HashSet::new(),
            child_registry: LspChildRegistry::new(),
        }
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
        let mut keys = Vec::new();
        for def in servers_for_file(file_path, config) {
            let Some(root) = def.workspace_root_for_file(file_path) else {
                continue;
            };
            let key = ServerKey {
                kind: def.kind.clone(),
                root,
            };
            if self.clients.contains_key(&key) {
                keys.push(key);
            }
        }
        keys
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

            let Some(root) = def.workspace_root_for_file(file_path) else {
                outcomes.attempts.push(ServerAttempt {
                    server_id,
                    server_name,
                    result: ServerAttemptResult::NoRootMarker {
                        looked_for: def.root_markers.iter().map(|s| s.to_string()).collect(),
                    },
                });
                continue;
            };

            let key = ServerKey {
                kind: def.kind.clone(),
                root,
            };

            if !self.clients.contains_key(&key) {
                // If we already tried and failed to spawn this server for this
                // root, return the cached classification without retrying or
                // re-logging. This prevents per-edit ERROR spam when the user's
                // environment is missing a dependency the LSP needs (the
                // typescript-language-server "Could not find a valid TypeScript
                // installation" case is the canonical example).
                if let Some(cached) = self.failed_spawns.get(&key) {
                    outcomes.attempts.push(ServerAttempt {
                        server_id,
                        server_name,
                        result: cached.clone(),
                    });
                    continue;
                }

                match self.spawn_server(&def, &key.root, config) {
                    Ok(client) => {
                        self.clients.insert(key.clone(), client);
                        self.server_binaries.insert(key.clone(), def.binary.clone());
                        self.documents.entry(key.clone()).or_default();
                    }
                    Err(err) => {
                        slog_error!("failed to spawn {}: {}", def.name, err);
                        let result = classify_spawn_error(&def.binary, &err);
                        // Remember the failure so subsequent file events skip
                        // this (kind, root) pair instead of producing a fresh
                        // spawn attempt + ERROR log per request.
                        self.failed_spawns.insert(key.clone(), result.clone());
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
        let initial_content = needs_content
            .then(|| std::fs::read_to_string(&canonical_path).map_err(LspError::Io))
            .transpose()?;
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
                let send_result = if let Some(client) = self.clients.get_mut(key) {
                    client.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                        text_document: TextDocumentItem::new(
                            uri.clone(),
                            language_id.clone(),
                            0,
                            content.clone(),
                        ),
                    })
                } else {
                    Ok(())
                };
                if let Err(err) = send_result {
                    let _ = self.close_file_for_servers(&canonical_path, &newly_opened);
                    return Err(err);
                }
                self.documents
                    .entry(key.clone())
                    .or_default()
                    .open(canonical_path.clone());
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
                .is_some_and(|store| store.is_stale_on_disk(&canonical_path));
            if drifted {
                let content = match std::fs::read_to_string(&canonical_path) {
                    Ok(content) => content,
                    Err(err) => {
                        let _ = self.close_file_for_servers(&canonical_path, &newly_opened);
                        return Err(LspError::Io(err));
                    }
                };
                let next_version = self
                    .documents
                    .get(key)
                    .and_then(|store| store.version(&canonical_path))
                    .map(|v| v + 1)
                    .unwrap_or(1);
                let send_result = if let Some(client) = self.clients.get_mut(key) {
                    client.send_notification::<DidChangeTextDocument>(DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier::new(
                            uri.clone(),
                            next_version,
                        ),
                        content_changes: vec![TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: content,
                        }],
                    })
                } else {
                    Ok(())
                };
                if let Err(err) = send_result {
                    let _ = self.close_file_for_servers(&canonical_path, &newly_opened);
                    return Err(err);
                }
                if let Some(store) = self.documents.get_mut(key) {
                    store.bump_version(&canonical_path);
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
    /// `0`. Servers that don't honor versioned text document sync will not
    /// echo this back on `publishDiagnostics`; the caller is expected to
    /// fall back to the epoch-delta path for those.
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
        let canonical_path = canonicalize_for_lsp(file_path)?;
        let server_keys = self.running_server_keys_for_file(&canonical_path, config);
        self.notify_file_changed_for_server_keys(canonical_path, content, server_keys)
            .map(|_| ())
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

        let uri = uri_for_path(&canonical_path)?;
        let language_id = language_id_for_extension(
            canonical_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default(),
        )
        .to_string();

        let mut versions: Vec<(ServerKey, i32)> = Vec::with_capacity(server_keys.len());

        for key in server_keys {
            let current_version = self
                .documents
                .get(&key)
                .and_then(|store| store.version(&canonical_path));

            if let Some(version) = current_version {
                let next_version = version + 1;
                if let Some(client) = self.clients.get_mut(&key) {
                    client.send_notification::<DidChangeTextDocument>(
                        DidChangeTextDocumentParams {
                            text_document: VersionedTextDocumentIdentifier::new(
                                uri.clone(),
                                next_version,
                            ),
                            content_changes: vec![TextDocumentContentChangeEvent {
                                range: None,
                                range_length: None,
                                text: content.to_string(),
                            }],
                        },
                    )?;
                }
                if let Some(store) = self.documents.get_mut(&key) {
                    store.bump_version(&canonical_path);
                }
                versions.push((key, next_version));
                continue;
            }

            if let Some(client) = self.clients.get_mut(&key) {
                client.send_notification::<DidOpenTextDocument>(DidOpenTextDocumentParams {
                    text_document: TextDocumentItem::new(
                        uri.clone(),
                        language_id.clone(),
                        0,
                        content.to_string(),
                    ),
                })?;
            }
            self.documents
                .entry(key.clone())
                .or_default()
                .open(canonical_path.clone());
            // didOpen carries version 0 — that's the version the server
            // will echo on its first publishDiagnostics for this document.
            versions.push((key, 0));
        }

        Ok(versions)
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
        if paths.is_empty() {
            return Ok(());
        }

        let mut canonical_events = Vec::with_capacity(paths.len());
        for (path, typ) in paths {
            let canonical_path = resolve_for_lsp_uri(path);
            canonical_events.push((canonical_path, *typ));
        }

        let keys: Vec<ServerKey> = self.clients.keys().cloned().collect();
        for key in keys {
            let mut changes = Vec::new();
            for (path, typ) in &canonical_events {
                if !path.starts_with(&key.root) {
                    continue;
                }
                changes.push(FileEvent::new(uri_for_path(path)?, *typ));
            }

            if changes.is_empty() {
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
                    if self.watched_file_skip_logged.insert(key.clone()) {
                        log::debug!(
                            "skipping didChangeWatchedFiles for {:?} (not supported or registered)",
                            key
                        );
                    }
                    continue;
                }
                client.send_notification::<DidChangeWatchedFiles>(DidChangeWatchedFilesParams {
                    changes,
                })?;
            }
        }

        Ok(())
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
        while events.len() < max_events {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            if self.handle_event(&event).is_some() {
                diagnostics_changed = true;
            }
            events.push(event);
        }
        let has_more = events.len() >= max_events && !self.event_rx.is_empty();
        DrainedLspEvents {
            events,
            diagnostics_changed,
            has_more,
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
    pub fn enqueue_event_for_test(&self, event: LspEvent) {
        self.event_tx
            .send(event)
            .expect("LSP event receiver should remain connected");
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

    /// Wait for FRESH per-server diagnostics that match the just-sent
    /// document version. This is the v0.17.3 post-edit path that fixes the
    /// stale-diagnostics bug: instead of returning whatever is in the cache
    /// when the deadline hits, we only return entries whose `version`
    /// matches the post-edit target version (or, for servers that don't
    /// participate in versioned sync, whose `epoch` was bumped after the
    /// pre-edit snapshot).
    ///
    /// `expected_versions` should come from `notify_file_changed_versioned`
    /// — one `(ServerKey, target_version)` per server we sent didChange/
    /// didOpen to.
    ///
    /// `pre_snapshot` is the per-server epoch BEFORE the notification was
    /// sent; it gates the epoch-fallback path so an old-version publish
    /// arriving after `drain_events` and before `didChange` cannot be
    /// mistaken for a fresh response.
    ///
    /// Returns a per-server tri-state: `Fresh` (publish matched target
    /// version OR epoch advanced past snapshot for an unversioned server),
    /// `Pending` (deadline hit before this server published anything we
    /// could verify), or `Exited` (server died between notification and
    /// deadline).
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
        let lookup_path = normalize_lookup_path(file_path);
        let deadline = std::time::Instant::now() + timeout;

        // Drain any events that arrived while we were sending didChange.
        // The publishDiagnostics handler stores the version, so even
        // pre-snapshot publishes that landed late won't be mistaken for
        // fresh — the version-match check will reject them.
        let _ = self.drain_events_for_file(&lookup_path);

        let mut fresh: HashMap<ServerKey, Vec<StoredDiagnostic>> = HashMap::new();
        let mut exited: Vec<ServerKey> = Vec::new();

        loop {
            // Check freshness for every expected server. A server is fresh
            // if its current entry for this file satisfies either:
            //   1. version-match: entry.version == Some(target_version), OR
            //   2. push-only freshness: entry.version is None AND entry.epoch
            //      advanced strictly after the pre-edit snapshot. Versioned
            //      publishes must be >= the post-edit target version.
            // Servers whose process has exited are reported separately.
            for (key, target_version) in expected_versions {
                if fresh.contains_key(key) || exited.contains(key) {
                    continue;
                }
                if !self.clients.contains_key(key) {
                    exited.push(key.clone());
                    continue;
                }
                if let Some(entry) = self
                    .diagnostics
                    .entries_for_file(&lookup_path)
                    .into_iter()
                    .find_map(|(k, e)| if k == key { Some(e) } else { None })
                {
                    let pre = pre_snapshot.get(key).copied().unwrap_or_default();
                    let is_fresh = post_edit_entry_is_fresh(entry, *target_version, pre);
                    if is_fresh {
                        fresh.insert(key.clone(), entry.diagnostics.clone());
                    }
                }
            }

            // All accounted for? Done.
            if fresh.len() + exited.len() == expected_versions.len() {
                break;
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }

            let timeout = deadline.saturating_duration_since(now);
            match self.event_rx.recv_timeout(timeout) {
                Ok(event) => {
                    self.handle_event(&event);
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        // Pending = expected but neither fresh nor exited.
        let pending: Vec<ServerKey> = expected_versions
            .iter()
            .filter(|(k, _)| !fresh.contains_key(k) && !exited.contains(k))
            .map(|(k, _)| k.clone())
            .collect();

        // Build deduplicated, sorted diagnostics from the fresh servers only.
        // Stale or pending servers contribute zero diagnostics.
        let mut diagnostics: Vec<StoredDiagnostic> = fresh
            .into_iter()
            .flat_map(|(_, diags)| diags.into_iter())
            .collect();
        diagnostics.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(a.column.cmp(&b.column))
                .then(a.message.cmp(&b.message))
        });

        PostEditWaitOutcome {
            diagnostics,
            pending_servers: pending,
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
        self.pull_file_diagnostics_tracked(file_path, config)
            .map(|tracked| tracked.results)
    }

    pub(crate) fn pull_file_diagnostics_tracked(
        &mut self,
        file_path: &Path,
        config: &Config,
    ) -> Result<TrackedPullFileResult, LspError> {
        let canonical_path = canonicalize_for_lsp(file_path)?;
        // Make sure servers are running and the document is open with fresh
        // content (handles disk-drift via DocumentStore::is_stale_on_disk).
        let opened = self.ensure_file_open(&canonical_path, config)?;
        if opened.server_keys.is_empty() {
            return Ok(TrackedPullFileResult {
                results: Vec::new(),
                newly_opened: opened.newly_opened,
            });
        }

        let uri = uri_for_path(&canonical_path)?;
        let mut results = Vec::with_capacity(opened.server_keys.len());

        for key in opened.server_keys {
            let supports_pull = self
                .clients
                .get(&key)
                .and_then(|c| c.diagnostic_capabilities())
                .is_some_and(|caps| caps.pull_diagnostics);

            if !supports_pull {
                results.push(PullFileResult {
                    server_key: key.clone(),
                    outcome: PullFileOutcome::PullNotSupported,
                });
                continue;
            }

            // Look up previous resultId for incremental requests.
            let previous_result_id = self
                .diagnostics
                .entries_for_file(&canonical_path)
                .into_iter()
                .find(|(k, _)| **k == key)
                .and_then(|(_, entry)| entry.result_id.clone());

            let identifier = self
                .clients
                .get(&key)
                .and_then(|c| c.diagnostic_capabilities())
                .and_then(|caps| caps.identifier.clone());

            let params = AftDocumentDiagnosticParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                identifier,
                previous_result_id,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };

            let outcome = match self.send_pull_request(&key, params) {
                Ok(report) => {
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
                    self.ingest_document_report(&key, &canonical_path, report)
                }
                Err(err) => {
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
            };

            results.push(PullFileResult {
                server_key: key,
                outcome,
            });
        }

        Ok(TrackedPullFileResult {
            results,
            newly_opened: opened.newly_opened,
        })
    }

    /// Issue a `workspace/diagnostic` request to a specific server. Cancels
    /// internally if `timeout` elapses before the server responds. Cached
    /// entries from the response are stored so directory-mode queries pick
    /// them up.
    pub fn pull_workspace_diagnostics(
        &mut self,
        server_key: &ServerKey,
        timeout: Option<std::time::Duration>,
    ) -> Result<PullWorkspaceResult, LspError> {
        let timeout = timeout.unwrap_or(Self::PULL_WORKSPACE_TIMEOUT);

        let supports_workspace = self
            .clients
            .get(server_key)
            .and_then(|c| c.diagnostic_capabilities())
            .is_some_and(|caps| caps.workspace_diagnostics);

        if !supports_workspace {
            return Ok(PullWorkspaceResult {
                server_key: server_key.clone(),
                files_reported: Vec::new(),
                complete: false,
                cancelled: false,
                supports_workspace: false,
            });
        }

        let identifier = self
            .clients
            .get(server_key)
            .and_then(|c| c.diagnostic_capabilities())
            .and_then(|caps| caps.identifier.clone());

        let params = AftWorkspaceDiagnosticParams {
            identifier,
            previous_result_ids: Vec::new(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result = match self
            .clients
            .get_mut(server_key)
            .ok_or_else(|| LspError::ServerNotReady("server not found".into()))?
            .send_request_with_timeout::<AftWorkspaceDiagnosticRequest>(params, timeout)
        {
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
        let (status, stderr_tail) = {
            let client = self.clients.get_mut(key)?;
            let mut status = client.child_exit_status();
            for _ in 0..10 {
                if status.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                status = client.child_exit_status();
            }
            let status = status?;
            wait_for_stderr_tail(client);
            (status, client.stderr_tail())
        };
        let reason = format_post_initialize_exit_reason(&binary, status, &stderr_tail, err);
        let result = ServerAttemptResult::SpawnFailed { binary, reason };
        self.clients.remove(key);
        self.server_binaries.remove(key);
        self.documents.remove(key);
        self.diagnostics.clear_for_server(key);
        self.failed_spawns.insert(key.clone(), result.clone());
        Some(result)
    }

    /// Issue the per-file diagnostic request and return the report.
    fn send_pull_request(
        &mut self,
        key: &ServerKey,
        params: AftDocumentDiagnosticParams,
    ) -> Result<lsp_types::DocumentDiagnosticReportResult, LspError> {
        let client = self
            .clients
            .get_mut(key)
            .ok_or_else(|| LspError::ServerNotReady("server not found".into()))?;
        // Use the documented 10s pull cap, not the global 30s request timeout —
        // a stalled pull server must not blow the scoped aft_inspect 8s budget
        // (or the lsp_diagnostics wait caps) all the way out to 30s.
        client.send_request_with_timeout::<AftDocumentDiagnosticRequest>(
            params,
            Self::PULL_FILE_TIMEOUT,
        )
    }

    /// Store the result of a per-file pull request and return a structured
    /// outcome the caller can inspect.
    fn ingest_document_report(
        &mut self,
        key: &ServerKey,
        canonical_path: &Path,
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
                );
                let count = stored.len();
                self.diagnostics.publish_with_result_id(
                    key.clone(),
                    canonical_path.to_path_buf(),
                    stored,
                    result_id,
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
                    self.diagnostics
                        .mark_fresh_for_server_file(key, canonical_path);
                    PullFileOutcome::Unchanged
                } else {
                    PullFileOutcome::RequestFailed {
                        reason: "no_cache_for_unchanged".to_string(),
                    }
                }
            }
        }
    }

    /// Shutdown all servers gracefully.
    pub fn shutdown_all(&mut self) {
        for (key, mut client) in self.clients.drain() {
            if let Err(err) = client.shutdown() {
                slog_error!("error shutting down {:?}: {}", key, err);
            }
        }
        self.server_binaries.clear();
        self.documents.clear();
        self.diagnostics = DiagnosticsStore::new();
    }

    /// Check if any server is active.
    pub fn has_active_servers(&self) -> bool {
        self.clients
            .values()
            .any(|client| client.state() == ServerState::Ready)
    }

    /// Active server keys (running clients). Used by `lsp_diagnostics`
    /// directory mode to know which servers to ask for workspace pull.
    pub fn active_server_keys(&self) -> Vec<ServerKey> {
        self.clients.keys().cloned().collect()
    }

    pub fn get_diagnostics_for_file(&self, file: &Path) -> Vec<&StoredDiagnostic> {
        let normalized = normalize_lookup_path(file);
        self.diagnostics.for_file(&normalized)
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
            ServerAttemptResult::SpawnFailed {
                binary: "rust-analyzer".to_string(),
                reason: "test".to_string(),
            },
        );
    }

    pub fn clear_diagnostics_for_file(&mut self, file: &Path) -> bool {
        let mut removed = self.diagnostics.clear_for_file(file);

        let normalized = normalize_lookup_path(file);
        if normalized != file {
            removed |= self.diagnostics.clear_for_file(&normalized);
        }

        // Reconstruct the canonical key via the parent dir (which still exists
        // for a just-deleted file) so symlink-aliased roots still match.
        if let (Some(parent), Some(name)) = (file.parent(), file.file_name()) {
            if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
                let reconstructed = canonical_parent.join(name);
                if reconstructed != file && reconstructed != normalized {
                    removed |= self.diagnostics.clear_for_file(&reconstructed);
                }
            }
        }

        removed
    }

    /// Mark cached diagnostics for this file stale after a watcher-observed
    /// external edit. The same path aliases as deletion are checked so canonical
    /// publish keys are found even when the watcher reports a symlinked path.
    pub fn mark_diagnostics_stale_for_file(&mut self, file: &Path) -> StaleDiagnosticsMark {
        let mut candidates = vec![file.to_path_buf()];
        let normalized = normalize_lookup_path(file);
        if !candidates.iter().any(|candidate| candidate == &normalized) {
            candidates.push(normalized.clone());
        }

        if let (Some(parent), Some(name)) = (file.parent(), file.file_name()) {
            if let Ok(canonical_parent) = std::fs::canonicalize(parent) {
                let reconstructed = canonical_parent.join(name);
                if !candidates
                    .iter()
                    .any(|candidate| candidate == &reconstructed)
                {
                    candidates.push(reconstructed);
                }
            }
        }

        let mut result = StaleDiagnosticsMark::default();
        for candidate in candidates {
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

    pub fn get_all_diagnostics(&self) -> Vec<&StoredDiagnostic> {
        self.diagnostics.all()
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

    fn handle_event(&mut self, event: &LspEvent) -> Option<PathBuf> {
        match event {
            LspEvent::Notification {
                server_kind,
                root,
                method,
                params: Some(params),
            } if method == "textDocument/publishDiagnostics" => {
                self.handle_publish_diagnostics(server_kind.clone(), root.clone(), params)
            }
            LspEvent::ServerExited { server_kind, root } => {
                let key = ServerKey {
                    kind: server_kind.clone(),
                    root: root.clone(),
                };
                self.clients.remove(&key);
                self.server_binaries.remove(&key);
                self.documents.remove(&key);
                self.diagnostics.clear_for_server(&key);
                None
            }
            _ => None,
        }
    }

    fn handle_publish_diagnostics(
        &mut self,
        server: ServerKind,
        root: PathBuf,
        params: &serde_json::Value,
    ) -> Option<PathBuf> {
        if let Ok(publish_params) =
            serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params.clone())
        {
            let file = uri_to_path(&publish_params.uri)?;
            let stored = from_lsp_diagnostics(file.clone(), publish_params.diagnostics);
            // v0.17.3: store with real ServerKey { kind, root } and capture
            // the document `version` (when the server provided one) so the
            // post-edit waiter can reject stale publishes deterministically
            // via version-match (preferred) or epoch-delta (fallback). The
            // earlier `publish_with_kind` path silently dropped both.
            let key = ServerKey { kind: server, root };
            self.diagnostics
                .publish_full(key, file.clone(), stored, None, publish_params.version);
            return Some(file);
        }
        None
    }

    fn spawn_server(
        &self,
        def: &ServerDef,
        root: &Path,
        config: &Config,
    ) -> Result<LspClient, LspError> {
        let binary = self.resolve_binary(def, config)?;

        // Merge the server-defined env with our test-injected env.
        // `extra_env` is empty in production; tests use it to drive fake
        // server variants (AFT_FAKE_LSP_PULL=1, etc.).
        let mut merged_env = def.env.clone();
        for (key, value) in &self.extra_env {
            merged_env.insert(key.clone(), value.clone());
        }

        let mut client = LspClient::spawn(
            def.kind.clone(),
            root.to_path_buf(),
            &binary,
            &def.args,
            &merged_env,
            self.event_tx.clone(),
            self.child_registry.clone(),
        )?;
        if let Err(err) = client.initialize(root, def.initialization_options.clone()) {
            wait_for_stderr_tail(&mut client);
            let stderr_tail = client.stderr_tail();
            let reason = if client.child_exited() || !stderr_tail.is_empty() {
                format_initialize_failure_reason(&def.binary, &stderr_tail, &err)
            } else {
                format!("server failed during initialize: {err}")
            };
            return Err(LspError::ServerNotReady(reason));
        }
        Ok(client)
    }

    fn resolve_binary(&self, def: &ServerDef, config: &Config) -> Result<PathBuf, LspError> {
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

        // Layered resolution:
        //   1. <project_root>/node_modules/.bin/<binary>
        //   2. config.lsp_paths_extra (plugin auto-install cache, etc.)
        //   3. PATH via `which`
        resolve_lsp_binary(
            &def.binary,
            config.project_root.as_deref(),
            &config.lsp_paths_extra,
        )
        .ok_or_else(|| {
            LspError::NotFound(format!(
                "language server binary '{}' not found in node_modules/.bin, lsp_paths_extra, or PATH",
                def.binary
            ))
        })
    }

    fn server_key_for_file(&self, file_path: &Path, config: &Config) -> Option<ServerKey> {
        for def in servers_for_file(file_path, config) {
            let root = def.workspace_root_for_file(file_path)?;
            let key = ServerKey {
                kind: def.kind.clone(),
                root,
            };
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

fn wait_for_stderr_tail(client: &mut LspClient) {
    for _ in 0..10 {
        if !client.stderr_tail().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
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

fn format_initialize_failure_reason(binary: &str, stderr_tail: &str, err: &LspError) -> String {
    let mut reason = format!("server crashed during initialize: {err}");
    if !stderr_tail.is_empty() {
        reason.push_str("; stderr (last 64 lines):\n");
        reason.push_str(&format_stderr_tail_for_reason(stderr_tail));
        reason.push_str("\n\n");
        reason.push_str(&failure_hint(binary, stderr_tail));
    }
    reason
}

fn format_post_initialize_exit_reason(
    binary: &str,
    status: std::process::ExitStatus,
    stderr_tail: &str,
    err: &LspError,
) -> String {
    let code = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal/unknown".to_string());
    let mut reason = format!("server exited after initialize (code {code}): {err}");
    if !stderr_tail.is_empty() {
        reason.push_str("; stderr (last 64 lines):\n");
        reason.push_str(&format_stderr_tail_for_reason(stderr_tail));
        reason.push_str("\n\n");
        reason.push_str(&failure_hint(binary, stderr_tail));
    }
    reason
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
    std::fs::canonicalize(file_path).map_err(LspError::from)
}

fn resolve_for_lsp_uri(file_path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(file_path) {
        return path;
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

    let mut resolved = std::fs::canonicalize(&existing).unwrap_or(existing);
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
        _ => "plaintext",
    }
}

fn normalize_lookup_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    std::env::var_os(key).map(PathBuf::from)
}

#[cfg(test)]
mod failure_hint_tests {
    use super::{failure_hint, rustup_missing_component};

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
mod diagnostic_capacity_tests {
    use std::fs;

    use super::LspManager;
    use crate::config::Config;

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
mod clear_diagnostics_tests {
    use std::path::PathBuf;

    use super::LspManager;
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

        let drained = manager.drain_events();

        assert!(drained.diagnostics_changed);
        assert_eq!(drained.events.len(), 1);
        assert_eq!(manager.warm_error_warning_counts(), (1, 0));
    }
}
