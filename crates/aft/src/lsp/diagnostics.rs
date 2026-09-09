use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::lsp::registry::ServerKind;
use crate::lsp::roots::ServerKey;

/// A single diagnostic from an LSP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDiagnostic {
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub code: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
        }
    }
}

/// One server's published diagnostics for one file, plus bookkeeping that
/// distinguishes "checked clean" (`diagnostics.is_empty()` AND
/// `epoch.is_some()`) from "never checked" (entry not present).
#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    pub diagnostics: Vec<StoredDiagnostic>,
    /// Monotonic epoch when this entry was last replaced by a publish or
    /// pull response. Used by callers to tell "fresh" results apart from
    /// stale cache contents.
    pub epoch: u64,
    /// Optional resultId from a pull response. Sent back as `previousResultId`
    /// on the next pull request to enable `kind: "unchanged"` short-circuiting.
    pub result_id: Option<String>,
    /// Document version this report was proven against. Versioned
    /// `publishDiagnostics` payloads provide it directly; a synchronous pull
    /// response is tagged with the client's current document version. Post-edit
    /// waiters use it to reject stale publishes deterministically instead of
    /// relying on epoch ordering, which has a race when an old-version publish
    /// arrives after the pre-edit drain. `None` means no version proof exists.
    pub version: Option<i32>,
    /// True after the filesystem watcher sees this file change outside AFT's
    /// text sync path and before a publish or pull response proves the cached
    /// diagnostics still describe the current file contents. Stale entries stay
    /// in the store so resultIds and server coverage are not lost, but warm
    /// readers must not count or display them as current diagnostics.
    pub stale: bool,
    /// True while rust-analyzer has not reported that its workspace analysis is
    /// quiescent. These diagnostics are useful leads, but are not authoritative
    /// enough to contribute to counts until the server has settled.
    pub provisional: bool,
}

/// Stores diagnostics from all LSP servers, keyed per `(ServerKey, file)`.
///
/// Key design points (driven by the v0.16 LSP audit):
///
/// 1. **Per-server state.** A single file can be served by multiple LSP
///    servers (e.g., pyright + ty, or tsserver + ESLint). The cache key is
///    `(ServerKey, PathBuf)` so each server's view is tracked independently.
///
/// 2. **Empty publishes are kept.** Earlier the store deleted entries on
///    empty publishes, making "checked clean" indistinguishable from "never
///    checked". Now we preserve the entry with `epoch = ...` so callers can
///    answer the question honestly.
///
/// 3. **LRU cap.** `capacity` (default 5000, configurable via
///    `Config::diagnostic_cache_size`) bounds memory. Set to 0 to disable.
///    On insert when at capacity, the least-recently-touched entry is
///    evicted. Eviction is tracked so directory-mode callers can list
///    those files as `unchecked` rather than silently lose them.
pub struct DiagnosticsStore {
    /// Primary store keyed by `(ServerKey, canonical file path)`.
    entries: HashMap<(ServerKey, PathBuf), DiagnosticEntry>,
    /// Secondary lookup from a file to every server with a cached report.
    /// Every mutation of `entries` must update this index in the same operation.
    by_file: HashMap<PathBuf, HashSet<ServerKey>>,
    /// Insertion/access order for LRU eviction. Most-recently-touched
    /// entries are at the END of the vector.
    order: Vec<(ServerKey, PathBuf)>,
    /// Maximum number of entries before LRU eviction kicks in. 0 = no cap.
    capacity: usize,
    /// Monotonic epoch counter. Incremented on every publish.
    next_epoch: u64,
    /// Monotonic identity for any diagnostics-store mutation. Status-bar
    /// aggregation uses it to skip unchanged project filtering work.
    generation: u64,
    /// Last time a server published/replaced diagnostics for a specific file.
    /// Used as a per-file freshness proof for push-only servers.
    last_publish_at_for_file: HashMap<(ServerKey, PathBuf), Instant>,
}

impl DiagnosticsStore {
    pub fn new() -> Self {
        Self::with_capacity(5000)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            by_file: HashMap::new(),
            order: Vec::new(),
            capacity,
            next_epoch: 0,
            generation: 0,
            last_publish_at_for_file: HashMap::new(),
        }
    }

    /// Set or change the LRU cap. If the new cap is smaller than the
    /// current entry count, the oldest entries are evicted immediately
    /// to fit.
    pub fn set_capacity(&mut self, capacity: usize) {
        if self.capacity != capacity {
            self.generation = self.generation.wrapping_add(1);
        }
        self.capacity = capacity;
        if capacity > 0 {
            while self.entries.len() > capacity {
                self.evict_lru();
            }
        }
        self.debug_assert_index_consistent();
    }

    /// Number of currently-tracked entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let mut diagnostic_count = 0usize;
        let entry_bytes = self
            .entries
            .iter()
            .fold(0u64, |bytes, ((server, path), entry)| {
                diagnostic_count = diagnostic_count.saturating_add(entry.diagnostics.len());
                let diagnostics_bytes = entry.diagnostics.iter().fold(0u64, |bytes, diagnostic| {
                    bytes
                        .saturating_add(std::mem::size_of::<StoredDiagnostic>() as u64)
                        .saturating_add(crate::memory::path_bytes(&diagnostic.file))
                        .saturating_add(crate::memory::usize_to_u64(diagnostic.message.len()))
                        .saturating_add(
                            diagnostic
                                .code
                                .as_ref()
                                .map(|code| crate::memory::usize_to_u64(code.len()))
                                .unwrap_or(0),
                        )
                        .saturating_add(
                            diagnostic
                                .source
                                .as_ref()
                                .map(|source| crate::memory::usize_to_u64(source.len()))
                                .unwrap_or(0),
                        )
                });
                bytes
                    .saturating_add(std::mem::size_of_val(server) as u64)
                    .saturating_add(crate::memory::path_bytes(&server.root))
                    .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                    .saturating_add(crate::memory::path_bytes(path))
                    .saturating_add(std::mem::size_of::<DiagnosticEntry>() as u64)
                    .saturating_add(
                        entry
                            .result_id
                            .as_ref()
                            .map(|result_id| crate::memory::usize_to_u64(result_id.len()))
                            .unwrap_or(0),
                    )
                    .saturating_add(diagnostics_bytes)
            });
        let order_bytes = self.order.iter().fold(0u64, |bytes, (server, path)| {
            bytes
                .saturating_add(std::mem::size_of_val(server) as u64)
                .saturating_add(crate::memory::path_bytes(&server.root))
                .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                .saturating_add(crate::memory::path_bytes(path))
        });
        let publish_bytes =
            self.last_publish_at_for_file
                .iter()
                .fold(0u64, |bytes, ((server, path), _)| {
                    bytes
                        .saturating_add(std::mem::size_of_val(server) as u64)
                        .saturating_add(crate::memory::path_bytes(&server.root))
                        .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                        .saturating_add(crate::memory::path_bytes(path))
                        .saturating_add(std::mem::size_of::<Instant>() as u64)
                });
        let by_file_bytes = self.by_file.iter().fold(0u64, |bytes, (path, servers)| {
            let server_bytes = servers.iter().fold(0u64, |server_bytes, server| {
                server_bytes
                    .saturating_add(std::mem::size_of_val(server) as u64)
                    .saturating_add(crate::memory::path_bytes(&server.root))
            });
            bytes
                .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                .saturating_add(crate::memory::path_bytes(path))
                .saturating_add(server_bytes)
        });
        crate::memory::MemoryEstimate::estimated(
            entry_bytes
                .saturating_add(order_bytes)
                .saturating_add(publish_bytes)
                .saturating_add(by_file_bytes),
        )
        .count("diagnostic_entries", self.entries.len())
        .count("diagnostics", diagnostic_count)
    }

    /// The current LRU cap (0 = unbounded). Test-only accessor used to verify
    /// the `lsp.diagnostic_cache_size` config wiring.
    #[cfg(test)]
    pub fn capacity_for_test(&self) -> usize {
        self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True if any entry is currently usable, including an empty checked-clean
    /// report. Watcher-stale entries do not prove current diagnostics.
    pub fn has_any_fresh_report(&self) -> bool {
        self.entries.values().any(|entry| !entry.stale)
    }

    /// Replace diagnostics for a `(server_kind, file)` pair using the
    /// server's lifecycle root from the active manager. Empty diagnostics
    /// are preserved as "checked clean" (NOT deleted as before).
    ///
    /// Note: the `(server, file)` key uses `ServerKey { kind, root }` so
    /// concurrent multi-workspace usage doesn't collapse different roots.
    /// Callers without the root (legacy push handler) should call
    /// `publish_with_kind` which derives the key.
    pub fn publish(
        &mut self,
        server: ServerKey,
        file: PathBuf,
        diagnostics: Vec<StoredDiagnostic>,
    ) {
        self.publish_with_result_id(server, file, diagnostics, None);
    }

    /// Replace diagnostics and record a pull `resultId` for the next
    /// request. Empty diagnostics are preserved as "checked clean".
    pub fn publish_with_result_id(
        &mut self,
        server: ServerKey,
        file: PathBuf,
        diagnostics: Vec<StoredDiagnostic>,
        result_id: Option<String>,
    ) {
        self.publish_full(server, file, diagnostics, result_id, None);
    }

    /// Replace diagnostics with full provenance (resultId + document version).
    /// `version` is either the LSP `publishDiagnostics` version or the current
    /// client document version proven by a synchronous pull response.
    pub fn publish_full(
        &mut self,
        server: ServerKey,
        file: PathBuf,
        diagnostics: Vec<StoredDiagnostic>,
        result_id: Option<String>,
        version: Option<i32>,
    ) {
        self.publish_full_with_provisional(server, file, diagnostics, result_id, version, false);
    }

    /// Replace diagnostics while recording whether the server was still warming
    /// when it produced them.
    pub fn publish_full_with_provisional(
        &mut self,
        server: ServerKey,
        file: PathBuf,
        diagnostics: Vec<StoredDiagnostic>,
        result_id: Option<String>,
        version: Option<i32>,
        provisional: bool,
    ) {
        let key = (server, file);
        self.next_epoch = self.next_epoch.saturating_add(1);
        self.generation = self.generation.wrapping_add(1);
        let entry = DiagnosticEntry {
            diagnostics,
            epoch: self.next_epoch,
            result_id,
            version,
            stale: false,
            provisional,
        };

        self.last_publish_at_for_file
            .insert(key.clone(), Instant::now());

        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), entry);
            self.index_entry(&key);
            self.touch_existing(&key);
        } else {
            // New entry — apply LRU cap before inserting.
            if self.capacity > 0 && self.entries.len() >= self.capacity {
                self.evict_lru();
            }
            self.entries.insert(key.clone(), entry);
            self.index_entry(&key);
            self.order.push(key);
        }
        self.debug_assert_index_consistent();
    }

    /// Compatibility wrapper for the legacy push path that knows only the
    /// `ServerKind`. Builds a `ServerKey` with an empty root, which is
    /// adequate for the single-root-per-kind case the manager currently
    /// uses for push diagnostics. Multi-root callers should use
    /// `publish` directly with a real `ServerKey`.
    pub fn publish_with_kind(
        &mut self,
        kind: ServerKind,
        file: PathBuf,
        diagnostics: Vec<StoredDiagnostic>,
    ) {
        let key = ServerKey {
            kind,
            root: PathBuf::new(),
        };
        self.publish(key, file, diagnostics);
    }

    /// Get current diagnostics for a specific file (across all servers).
    /// Watcher-stale entries are kept for bookkeeping but are not surfaced.
    pub fn for_file(&self, file: &Path) -> Vec<&StoredDiagnostic> {
        let Some(servers) = self.by_file.get(file) else {
            return Vec::new();
        };
        let file = file.to_path_buf();
        let mut diagnostics = Vec::new();
        for server in servers {
            if let Some(entry) = self.entries.get(&(server.clone(), file.clone())) {
                if !entry.stale {
                    diagnostics.extend(&entry.diagnostics);
                }
            }
        }
        diagnostics
    }

    /// Current diagnostics for a file with the entry-level readiness marker.
    /// The marker is kept separate from [`StoredDiagnostic`] because readiness
    /// describes the server report, not an individual diagnostic.
    pub fn for_file_with_provisional(&self, file: &Path) -> Vec<(&StoredDiagnostic, bool)> {
        let Some(servers) = self.by_file.get(file) else {
            return Vec::new();
        };
        let file = file.to_path_buf();
        let mut diagnostics = Vec::new();
        for server in servers {
            if let Some(entry) = self.entries.get(&(server.clone(), file.clone())) {
                if !entry.stale {
                    diagnostics.extend(
                        entry
                            .diagnostics
                            .iter()
                            .map(|diagnostic| (diagnostic, entry.provisional)),
                    );
                }
            }
        }
        diagnostics
    }

    /// Get the full per-server entry for a file. Useful when callers need
    /// to know epoch/resultId, not just the diagnostics array.
    pub fn entries_for_file(&self, file: &Path) -> Vec<(&ServerKey, &DiagnosticEntry)> {
        let Some(servers) = self.by_file.get(file) else {
            return Vec::new();
        };
        let file = file.to_path_buf();
        servers
            .iter()
            .filter_map(|server| {
                self.entries
                    .get_key_value(&(server.clone(), file.clone()))
                    .map(|((stored_server, _), entry)| (stored_server, entry))
            })
            .collect()
    }

    /// True if any server has an entry (fresh or stale) for this file.
    pub fn has_any_report_for_file(&self, file: &Path) -> bool {
        self.by_file.contains_key(file)
    }

    /// True if any server has a non-stale report for this file.
    pub fn has_any_fresh_report_for_file(&self, file: &Path) -> bool {
        let Some(servers) = self.by_file.get(file) else {
            return false;
        };
        let file = file.to_path_buf();
        servers.iter().any(|server| {
            self.entries
                .get(&(server.clone(), file.clone()))
                .is_some_and(|entry| !entry.stale)
        })
    }

    /// True if any server has an authoritative report for this file: an entry
    /// that is neither watcher-stale nor warming-provisional. Empty
    /// checked-clean reports count — they still prove a producer analyzed the
    /// file. Callers must not treat "no authoritative report" as "clean"; it
    /// only ever means the analysis evidence is missing or not yet settled.
    pub fn has_authoritative_report_for_file(&self, file: &Path) -> bool {
        let Some(servers) = self.by_file.get(file) else {
            return false;
        };
        let file = file.to_path_buf();
        servers.iter().any(|server| {
            self.entries
                .get(&(server.clone(), file.clone()))
                .is_some_and(|entry| !entry.stale && !entry.provisional)
        })
    }

    /// True if this server holds any authoritative report: an entry that is
    /// neither watcher-stale nor warming-provisional. Empty checked-clean
    /// reports count — they still prove the producer analyzed a document.
    pub fn has_authoritative_report_for_server(&self, server: &ServerKey) -> bool {
        self.entries
            .iter()
            .any(|((key, _), entry)| key == server && !entry.stale && !entry.provisional)
    }

    /// True if this exact server instance has an entry (fresh or stale) for
    /// this exact file. Pull diagnostics use stale entries as the previous
    /// resultId cache when asking the server whether diagnostics are unchanged.
    pub fn has_report_for_server_file(&self, server: &ServerKey, file: &Path) -> bool {
        self.entries
            .contains_key(&(server.clone(), file.to_path_buf()))
    }

    /// True if this exact server instance has a non-stale report for this file.
    pub fn has_fresh_report_for_server_file(&self, server: &ServerKey, file: &Path) -> bool {
        self.entries
            .get(&(server.clone(), file.to_path_buf()))
            .is_some_and(|entry| !entry.stale)
    }

    /// True if this exact server instance published/replaced diagnostics for
    /// this exact file after `since`. This is intentionally per `(kind, root,
    /// file)`; a publish for another file must not prove freshness here.
    pub fn has_publish_for_file_after(
        &self,
        server: &ServerKey,
        file: &Path,
        since: Instant,
    ) -> bool {
        self.last_publish_at_for_file
            .get(&(server.clone(), file.to_path_buf()))
            .is_some_and(|published_at| {
                *published_at >= since && self.has_fresh_report_for_server_file(server, file)
            })
    }

    /// Get current diagnostics for files under a directory.
    pub fn for_directory(&self, dir: &Path) -> Vec<&StoredDiagnostic> {
        self.entries
            .iter()
            .filter(|((_, stored_file), entry)| stored_file.starts_with(dir) && !entry.stale)
            .flat_map(|(_, entry)| entry.diagnostics.iter())
            .collect()
    }

    /// Current diagnostics under a directory with the entry-level readiness
    /// marker preserved for inspect's provisional framing.
    pub fn for_directory_with_provisional(&self, dir: &Path) -> Vec<(&StoredDiagnostic, bool)> {
        self.entries
            .iter()
            .filter(|((_, stored_file), entry)| stored_file.starts_with(dir) && !entry.stale)
            .flat_map(|(_, entry)| {
                entry
                    .diagnostics
                    .iter()
                    .map(|diagnostic| (diagnostic, entry.provisional))
            })
            .collect()
    }

    /// All current diagnostics, flattened. Watcher-stale entries are hidden.
    pub fn all(&self) -> Vec<&StoredDiagnostic> {
        self.entries
            .values()
            .filter(|entry| !entry.stale)
            .flat_map(|entry| entry.diagnostics.iter())
            .collect()
    }

    /// All current diagnostics with the entry-level readiness marker.
    pub fn all_with_provisional(&self) -> Vec<(&StoredDiagnostic, bool)> {
        self.entries
            .values()
            .filter(|entry| !entry.stale)
            .flat_map(|entry| {
                entry
                    .diagnostics
                    .iter()
                    .map(|diagnostic| (diagnostic, entry.provisional))
            })
            .collect()
    }

    /// Count of errors and warnings across the entire warm set (every file any
    /// server has published for). Allocation-free — the raw, unfiltered union.
    /// Callers that want the agent-status-bar semantics (project-root scoped,
    /// tsconfig-membership filtered, cross-server deduped) should use
    /// [`filtered_error_warning_counts`](Self::filtered_error_warning_counts).
    pub fn error_warning_counts(&self) -> (usize, usize) {
        self.error_warning_counts_with_provisional().0
    }

    /// Raw warm-set counts plus whether any current entry is provisional.
    pub fn error_warning_counts_with_provisional(&self) -> ((usize, usize), bool) {
        let mut errors = 0usize;
        let mut warnings = 0usize;
        let mut has_provisional = false;
        for entry in self.entries.values() {
            if entry.provisional {
                has_provisional = true;
            }
            if entry.stale || entry.provisional {
                continue;
            }
            for diagnostic in &entry.diagnostics {
                match diagnostic.severity {
                    DiagnosticSeverity::Error => errors += 1,
                    DiagnosticSeverity::Warning => warnings += 1,
                    _ => {}
                }
            }
        }
        ((errors, warnings), has_provisional)
    }

    /// Error/warning counts after applying a per-file `keep` predicate,
    /// excluding environmental/setup diagnostics (see `environmental.rs`),
    /// and de-duplicating diagnostics that multiple servers reported for the
    /// same location. This matches `aft_inspect`'s warm semantics
    /// (`inspect/diagnostics_category.rs`: project-root filter +
    /// tsconfig-membership skip + environmental filter + `sort_and_dedup`) so
    /// the agent status bar's E/W agree with `aft_inspect`/`tsc` instead of
    /// counting build-excluded files and double-counting multi-server overlaps.
    ///
    /// The store itself holds no tsconfig/project policy — the caller encodes
    /// it in `keep` (see `LspManager::filtered_error_warning_counts`). `keep`
    /// is `FnMut` because the membership cache resolves lazily.
    pub fn filtered_error_warning_counts(&self, keep: impl FnMut(&Path) -> bool) -> (usize, usize) {
        self.filtered_error_warning_counts_with_provisional(keep).0
    }

    /// Return status-bar counts plus whether a kept entry is still provisional.
    /// The boolean lets the context retain the previous authoritative E/W values
    /// instead of replacing them with zero while an analyzer warms up.
    pub fn filtered_error_warning_counts_with_provisional(
        &self,
        mut keep: impl FnMut(&Path) -> bool,
    ) -> ((usize, usize), bool) {
        // Dedup key mirrors `sort_and_dedup` in inspect/diagnostics_category.rs
        // exactly (file, range, severity, message, source) so the bar and
        // inspect collapse the same multi-server overlaps.
        let mut seen: std::collections::HashSet<(
            &Path,
            u32,
            u32,
            u32,
            u32,
            &str,
            &str,
            Option<&str>,
        )> = std::collections::HashSet::new();
        let mut errors = 0usize;
        let mut warnings = 0usize;
        let mut has_provisional = false;
        for ((_, file), entry) in &self.entries {
            if entry.provisional && keep(file) {
                has_provisional = true;
            }
            if entry.stale || entry.provisional {
                continue;
            }
            // All diagnostics in an entry share the entry's file, so the keep
            // predicate (the cost center: tsconfig resolution) runs once per
            // (server, file) entry, not once per diagnostic.
            if !keep(file) {
                continue;
            }
            for diagnostic in &entry.diagnostics {
                if crate::lsp::environmental::is_environmental_diagnostic(diagnostic) {
                    continue;
                }
                let dedup_key = (
                    diagnostic.file.as_path(),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.end_line,
                    diagnostic.end_column,
                    diagnostic.severity.as_str(),
                    diagnostic.message.as_str(),
                    diagnostic.source.as_deref(),
                );
                if !seen.insert(dedup_key) {
                    continue;
                }
                match diagnostic.severity {
                    DiagnosticSeverity::Error => errors += 1,
                    DiagnosticSeverity::Warning => warnings += 1,
                    _ => {}
                }
            }
        }
        ((errors, warnings), has_provisional)
    }

    /// Drop all entries for a server kind (e.g., on server crash/restart).
    /// Prefer `clear_for_server` for real manager cleanup so peer roots of the
    /// same kind are not wiped.
    pub fn clear_server(&mut self, server: ServerKind) {
        let before = self.entries.len();
        self.entries
            .retain(|(stored_key, _), _| stored_key.kind != server);
        self.order
            .retain(|(stored_key, _)| stored_key.kind != server);
        self.last_publish_at_for_file
            .retain(|(stored_key, _), _| stored_key.kind != server);
        self.by_file.retain(|_, servers| {
            servers.retain(|stored_key| stored_key.kind != server);
            !servers.is_empty()
        });
        if self.entries.len() != before {
            self.generation = self.generation.wrapping_add(1);
        }
        self.debug_assert_index_consistent();
    }

    /// Drop one cached report for a specific server/file pair.
    pub fn clear_for_server_file(&mut self, key: &ServerKey, file: &Path) {
        let cache_key = (key.clone(), file.to_path_buf());
        if self.entries.remove(&cache_key).is_some() {
            self.unindex_entry(&cache_key);
            self.generation = self.generation.wrapping_add(1);
        }
        self.order.retain(|entry_key| entry_key != &cache_key);
        self.last_publish_at_for_file.remove(&cache_key);
        self.debug_assert_index_consistent();
    }

    /// Drop every cached report for a file across all servers. Used when a file
    /// is deleted/renamed away — its diagnostics would otherwise linger in the
    /// warm set forever (no server republishes for a path that no longer
    /// exists), inflating the error/warning counts surfaced in the status bar
    /// and `aft_inspect`. Returns true if any entry was removed.
    pub fn clear_for_file(&mut self, file: &Path) -> bool {
        let Some(servers) = self.by_file.remove(file) else {
            self.debug_assert_index_consistent();
            return false;
        };
        let mut removed = false;
        for server in servers {
            let cache_key = (server, file.to_path_buf());
            removed |= self.entries.remove(&cache_key).is_some();
            self.last_publish_at_for_file.remove(&cache_key);
        }
        if removed {
            self.generation = self.generation.wrapping_add(1);
            self.order.retain(|(_, stored_file)| stored_file != file);
        }
        self.debug_assert_index_consistent();
        removed
    }

    /// Mark every cached report for a file stale without evicting it.
    ///
    /// This is used for watcher-observed external edits: the previous
    /// diagnostics may still be useful as a pull `previousResultId`, but warm
    /// readers must stop counting them until a server publish or pull response
    /// proves freshness. Returns `(had_entries, changed)` where `changed` is true
    /// only if at least one previously-fresh entry became stale.
    pub fn mark_stale_for_file(&mut self, file: &Path) -> (bool, bool) {
        let Some(servers) = self.by_file.get(file) else {
            return (false, false);
        };
        let had_entries = !servers.is_empty();
        let mut changed = false;
        for server in servers {
            let cache_key = (server.clone(), file.to_path_buf());
            if let Some(entry) = self
                .entries
                .get_mut(&cache_key)
                .filter(|entry| !entry.stale)
            {
                entry.stale = true;
                changed = true;
            }
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        self.debug_assert_index_consistent();
        (had_entries, changed)
    }

    /// Mark one cached report fresh after a server response proves it still
    /// describes the current document (for example a pull `kind: unchanged`).
    pub fn mark_fresh_for_server_file(&mut self, key: &ServerKey, file: &Path) -> bool {
        let cache_key = (key.clone(), file.to_path_buf());
        let Some(entry) = self.entries.get_mut(&cache_key) else {
            return false;
        };
        let changed = entry.stale;
        entry.stale = false;
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        self.touch_existing(&cache_key);
        changed
    }

    /// Record that a pull `kind: unchanged` response confirmed the cached report
    /// for a specific in-memory document version. The confirmation is a new
    /// observation, so its epoch advances even when the cached rows did not change.
    pub fn confirm_for_server_file_version(
        &mut self,
        key: &ServerKey,
        file: &Path,
        version: i32,
    ) -> bool {
        let cache_key = (key.clone(), file.to_path_buf());
        let Some(entry) = self.entries.get_mut(&cache_key) else {
            return false;
        };
        self.next_epoch = self.next_epoch.saturating_add(1);
        self.generation = self.generation.wrapping_add(1);
        entry.epoch = self.next_epoch;
        entry.version = Some(version);
        entry.stale = false;
        self.touch_existing(&cache_key);
        true
    }

    /// Promote the latest provisional report for each file when its server reaches
    /// quiescence. Each store entry is already the server's latest replacement
    /// publish, so the settle boundary makes it authoritative without requiring a
    /// later publish. Independent watcher staleness is preserved.
    pub fn promote_provisional_for_server(&mut self, key: &ServerKey) -> bool {
        let mut changed = false;
        for ((stored_key, _), entry) in &mut self.entries {
            if stored_key == key && entry.provisional {
                entry.provisional = false;
                changed = true;
            }
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        self.debug_assert_index_consistent();
        changed
    }

    /// Clear the readiness marker after a pull response is received from a
    /// quiescent server. This is separate from `mark_fresh` because a pull
    /// response received while warming must remain provisional.
    pub fn clear_provisional_for_server_file(&mut self, key: &ServerKey, file: &Path) -> bool {
        let cache_key = (key.clone(), file.to_path_buf());
        let Some(entry) = self.entries.get_mut(&cache_key) else {
            return false;
        };
        if !entry.provisional {
            return false;
        }
        entry.provisional = false;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Drop all entries for a specific server instance.
    pub fn clear_for_server(&mut self, key: &ServerKey) {
        let before = self.entries.len();
        self.entries.retain(|(k, _), _| k != key);
        self.order.retain(|(k, _)| k != key);
        self.last_publish_at_for_file.retain(|(k, _), _| k != key);
        self.by_file.retain(|_, servers| {
            servers.remove(key);
            !servers.is_empty()
        });
        if self.entries.len() != before {
            self.generation = self.generation.wrapping_add(1);
        }
        self.debug_assert_index_consistent();
    }

    /// Backward-compatible alias for tests/callers that already used the
    /// instance-scoped name.
    pub fn clear_server_instance(&mut self, key: &ServerKey) {
        self.clear_for_server(key);
    }

    /// Remove the least-recently-used entry, returning its key for telemetry.
    fn evict_lru(&mut self) -> Option<(ServerKey, PathBuf)> {
        if self.order.is_empty() {
            return None;
        }
        let evicted = self.order.remove(0);
        self.entries.remove(&evicted);
        self.unindex_entry(&evicted);
        self.last_publish_at_for_file.remove(&evicted);
        self.debug_assert_index_consistent();
        Some(evicted)
    }

    fn touch_existing(&mut self, key: &(ServerKey, PathBuf)) {
        if let Some(idx) = self.order.iter().position(|k| k == key) {
            let removed = self.order.remove(idx);
            self.order.push(removed);
        }
    }

    fn index_entry(&mut self, (server, file): &(ServerKey, PathBuf)) {
        self.by_file
            .entry(file.clone())
            .or_default()
            .insert(server.clone());
    }

    fn unindex_entry(&mut self, (server, file): &(ServerKey, PathBuf)) {
        let remove_file = self.by_file.get_mut(file).is_some_and(|servers| {
            servers.remove(server);
            servers.is_empty()
        });
        if remove_file {
            self.by_file.remove(file);
        }
    }

    fn debug_assert_index_consistent(&self) {
        #[cfg(debug_assertions)]
        {
            let indexed_entries = self.by_file.values().map(HashSet::len).sum::<usize>();
            debug_assert_eq!(indexed_entries, self.entries.len());
            for (server, file) in self.entries.keys() {
                debug_assert!(self
                    .by_file
                    .get(file)
                    .is_some_and(|servers| servers.contains(server)));
            }
            for (file, servers) in &self.by_file {
                debug_assert!(!servers.is_empty());
                for server in servers {
                    debug_assert!(self.entries.contains_key(&(server.clone(), file.clone())));
                }
            }
        }
    }

    #[cfg(test)]
    fn mark_stale_for_file_linear_reference(&self, file: &Path) -> (bool, bool) {
        let mut had_entries = false;
        let mut changed = false;
        for ((_, stored_file), entry) in &self.entries {
            if stored_file == file {
                had_entries = true;
                changed |= !entry.stale;
            }
        }
        (had_entries, changed)
    }
}

impl Default for DiagnosticsStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert LSP diagnostics to our stored format.
/// LSP uses 0-based line/character; we convert to 1-based.
pub fn from_lsp_diagnostics(
    file: PathBuf,
    lsp_diagnostics: Vec<lsp_types::Diagnostic>,
) -> Vec<StoredDiagnostic> {
    lsp_diagnostics
        .into_iter()
        .map(|diagnostic| StoredDiagnostic {
            file: file.clone(),
            line: diagnostic.range.start.line + 1,
            column: diagnostic.range.start.character + 1,
            end_line: diagnostic.range.end.line + 1,
            end_column: diagnostic.range.end.character + 1,
            severity: match diagnostic.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => DiagnosticSeverity::Error,
                Some(lsp_types::DiagnosticSeverity::WARNING) => DiagnosticSeverity::Warning,
                Some(lsp_types::DiagnosticSeverity::INFORMATION) => DiagnosticSeverity::Information,
                Some(lsp_types::DiagnosticSeverity::HINT) => DiagnosticSeverity::Hint,
                _ => DiagnosticSeverity::Warning,
            },
            message: diagnostic.message,
            code: diagnostic.code.map(|code| match code {
                lsp_types::NumberOrString::Number(value) => value.to_string(),
                lsp_types::NumberOrString::String(value) => value,
            }),
            source: diagnostic.source,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use lsp_types::{
        Diagnostic, DiagnosticSeverity as LspDiagnosticSeverity, NumberOrString, Position, Range,
    };

    use super::{from_lsp_diagnostics, DiagnosticSeverity, DiagnosticsStore, StoredDiagnostic};
    use crate::lsp::registry::ServerKind;
    use crate::lsp::roots::ServerKey;

    fn server_key(kind: ServerKind) -> ServerKey {
        ServerKey {
            kind,
            root: PathBuf::from("/tmp/repo"),
        }
    }

    fn diag(file: &str, line: u32, msg: &str, sev: DiagnosticSeverity) -> StoredDiagnostic {
        StoredDiagnostic {
            file: PathBuf::from(file),
            line,
            column: 1,
            end_line: line,
            end_column: 2,
            severity: sev,
            message: msg.into(),
            code: None,
            source: None,
        }
    }

    #[test]
    fn diagnostics_memory_estimate_is_zero_when_empty_and_nonzero_when_populated() {
        let mut store = DiagnosticsStore::new();
        assert_eq!(store.estimated_memory().estimated_bytes, Some(0));
        let file = PathBuf::from("/tmp/memory.rs");
        store.publish(
            server_key(ServerKind::Rust),
            file.clone(),
            vec![diag(
                file.to_str().unwrap(),
                1,
                "resident diagnostic message",
                DiagnosticSeverity::Warning,
            )],
        );
        let estimate = store.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert_eq!(estimate.counts["diagnostic_entries"], 1);
        assert_eq!(estimate.counts["diagnostics"], 1);
    }

    #[test]
    fn converts_lsp_positions_to_one_based() {
        let file = PathBuf::from("/tmp/demo.rs");
        let diagnostics = from_lsp_diagnostics(
            file.clone(),
            vec![Diagnostic {
                range: Range::new(Position::new(0, 0), Position::new(1, 4)),
                severity: Some(LspDiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("E1".into())),
                code_description: None,
                source: Some("fake".into()),
                message: "boom".into(),
                related_information: None,
                tags: None,
                data: None,
            }],
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].file, file);
        assert_eq!(diagnostics[0].line, 1);
        assert_eq!(diagnostics[0].column, 1);
        assert_eq!(diagnostics[0].end_line, 2);
        assert_eq!(diagnostics[0].end_column, 5);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[0].code.as_deref(), Some("E1"));
    }

    #[test]
    fn publish_replaces_existing_file_diagnostics() {
        let file = PathBuf::from("/tmp/demo.rs");
        let mut store = DiagnosticsStore::new();
        let key = server_key(ServerKind::Rust);

        store.publish(
            key.clone(),
            file.clone(),
            vec![diag(
                "/tmp/demo.rs",
                1,
                "first",
                DiagnosticSeverity::Warning,
            )],
        );
        store.publish(
            key.clone(),
            file.clone(),
            vec![diag("/tmp/demo.rs", 2, "second", DiagnosticSeverity::Error)],
        );

        let stored = store.for_file(&file);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].message, "second");
    }

    #[test]
    fn empty_publish_is_preserved_as_checked_clean() {
        // The whole point of the v0.16 audit fix: empty publish ≠ deletion.
        // Agents need to be able to ask "has this file been checked yet?"
        // and get a truthful answer.
        let file = PathBuf::from("/tmp/clean.rs");
        let mut store = DiagnosticsStore::new();
        let key = server_key(ServerKind::Rust);

        // First publish has an issue.
        store.publish(
            key.clone(),
            file.clone(),
            vec![diag(
                "/tmp/clean.rs",
                5,
                "fix me",
                DiagnosticSeverity::Warning,
            )],
        );
        assert!(store.has_any_report_for_file(&file));
        assert_eq!(store.for_file(&file).len(), 1);

        // Second publish is empty (the fix worked). Entry is preserved as
        // "checked clean" rather than deleted.
        store.publish(key.clone(), file.clone(), Vec::new());
        assert!(
            store.has_any_report_for_file(&file),
            "checked-clean must be distinguishable from never-checked"
        );
        assert_eq!(store.for_file(&file).len(), 0);

        let entries = store.entries_for_file(&file);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].1.epoch > 0);
    }

    #[test]
    fn never_checked_returns_no_report() {
        let store = DiagnosticsStore::new();
        let file = PathBuf::from("/tmp/never.rs");
        assert!(!store.has_any_report_for_file(&file));
        assert!(store.for_file(&file).is_empty());
    }

    #[test]
    fn stale_entries_are_hidden_but_preserved_for_refresh() {
        let file = PathBuf::from("/tmp/stale.rs");
        let mut store = DiagnosticsStore::new();
        let key = server_key(ServerKind::Rust);
        store.publish(
            key.clone(),
            file.clone(),
            vec![diag("/tmp/stale.rs", 1, "old", DiagnosticSeverity::Error)],
        );

        let (had_entries, changed) = store.mark_stale_for_file(&file);

        assert!(had_entries);
        assert!(changed);
        assert!(store.has_any_report_for_file(&file));
        assert!(!store.has_any_fresh_report_for_file(&file));
        assert!(store.for_file(&file).is_empty());
        assert!(store.all().is_empty());
        assert_eq!(store.error_warning_counts(), (0, 0));
        assert_eq!(store.entries_for_file(&file).len(), 1);

        assert!(store.mark_fresh_for_server_file(&key, &file));
        assert!(store.has_any_fresh_report_for_file(&file));
        assert_eq!(store.for_file(&file).len(), 1);
        assert_eq!(store.error_warning_counts(), (1, 0));
    }

    #[test]
    fn per_server_state_is_tracked_independently() {
        let file = PathBuf::from("/tmp/multi.py");
        let mut store = DiagnosticsStore::new();
        let pyright_key = server_key(ServerKind::Python);
        let ty_key = server_key(ServerKind::Ty);

        store.publish(
            pyright_key,
            file.clone(),
            vec![diag(
                "/tmp/multi.py",
                1,
                "pyright says X",
                DiagnosticSeverity::Error,
            )],
        );
        store.publish(
            ty_key,
            file.clone(),
            vec![diag(
                "/tmp/multi.py",
                2,
                "ty says Y",
                DiagnosticSeverity::Warning,
            )],
        );

        let messages: Vec<&str> = store
            .for_file(&file)
            .into_iter()
            .map(|d| d.message.as_str())
            .collect();

        assert_eq!(messages.len(), 2, "both servers' reports preserved");
        assert!(messages.iter().any(|m| m == &"pyright says X"));
        assert!(messages.iter().any(|m| m == &"ty says Y"));
    }

    #[test]
    fn clear_for_server_file_removes_only_exact_entry() {
        let file_a = PathBuf::from("/tmp/a.rs");
        let file_b = PathBuf::from("/tmp/b.rs");
        let mut store = DiagnosticsStore::new();
        let rust_key = server_key(ServerKind::Rust);
        let py_key = server_key(ServerKind::Python);

        store.publish(
            rust_key.clone(),
            file_a.clone(),
            vec![diag("/tmp/a.rs", 1, "rust a", DiagnosticSeverity::Error)],
        );
        store.publish(
            rust_key.clone(),
            file_b.clone(),
            vec![diag("/tmp/b.rs", 1, "rust b", DiagnosticSeverity::Warning)],
        );
        store.publish(
            py_key.clone(),
            file_a.clone(),
            vec![diag("/tmp/a.rs", 2, "py a", DiagnosticSeverity::Warning)],
        );

        store.clear_for_server_file(&rust_key, &file_a);

        assert!(!store.has_report_for_server_file(&rust_key, &file_a));
        assert!(store.has_report_for_server_file(&rust_key, &file_b));
        assert!(store.has_report_for_server_file(&py_key, &file_a));
    }

    #[test]
    fn lru_evicts_oldest_when_capacity_exceeded() {
        let mut store = DiagnosticsStore::with_capacity(2);
        let key = server_key(ServerKind::Rust);

        store.publish(
            key.clone(),
            PathBuf::from("/a.rs"),
            vec![diag("/a.rs", 1, "a", DiagnosticSeverity::Warning)],
        );
        store.publish(
            key.clone(),
            PathBuf::from("/b.rs"),
            vec![diag("/b.rs", 1, "b", DiagnosticSeverity::Warning)],
        );
        assert_eq!(store.len(), 2);

        // Inserting a third entry should evict /a.rs (oldest).
        store.publish(
            key.clone(),
            PathBuf::from("/c.rs"),
            vec![diag("/c.rs", 1, "c", DiagnosticSeverity::Warning)],
        );
        assert_eq!(store.len(), 2);
        assert!(!store.has_any_report_for_file(Path::new("/a.rs")));
        assert!(!store.by_file.contains_key(Path::new("/a.rs")));
        assert!(store.has_any_report_for_file(Path::new("/b.rs")));
        assert!(store.has_any_report_for_file(Path::new("/c.rs")));
        store.debug_assert_index_consistent();
    }

    #[test]
    fn secondary_index_stays_consistent_through_seeded_mutation_storm() {
        fn next_random(seed: &mut u64) -> u64 {
            *seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *seed
        }

        let servers = [
            server_key(ServerKind::Rust),
            server_key(ServerKind::TypeScript),
            server_key(ServerKind::Python),
            server_key(ServerKind::Biome),
        ];
        let files = (0..11)
            .map(|index| PathBuf::from(format!("/tmp/index-{index}.rs")))
            .collect::<Vec<_>>();
        let mut store = DiagnosticsStore::with_capacity(7);
        let mut seed = 0x05ee_dd1a_6005_71c5_u64;
        let mut operation_counts = [0usize; 7];
        let mut stale_hits = 0usize;

        for step in 0..1_000 {
            let operation = (next_random(&mut seed) % operation_counts.len() as u64) as usize;
            operation_counts[operation] += 1;
            let server = servers[(next_random(&mut seed) % servers.len() as u64) as usize].clone();
            let file = files[(next_random(&mut seed) % files.len() as u64) as usize].clone();

            match operation {
                0 | 1 => store.publish(
                    server,
                    file.clone(),
                    vec![diag(
                        file.to_str().unwrap(),
                        step + 1,
                        "seeded diagnostic",
                        DiagnosticSeverity::Warning,
                    )],
                ),
                2 => {
                    let expected = store.mark_stale_for_file_linear_reference(&file);
                    let actual = store.mark_stale_for_file(&file);
                    assert_eq!(actual, expected);
                    stale_hits += usize::from(actual.0);
                }
                3 => {
                    store.clear_for_server_file(&server, &file);
                }
                4 => {
                    store.clear_for_file(&file);
                }
                5 => {
                    store.clear_for_server(&server);
                }
                6 => {
                    store.clear_server(server.kind);
                }
                _ => unreachable!(),
            }

            store.debug_assert_index_consistent();
            assert!(store.len() <= 7);
        }

        assert!(operation_counts.into_iter().all(|count| count > 0));
        assert!(
            stale_hits > 0,
            "seeded sequence must stale existing entries"
        );
    }

    #[test]
    fn touching_existing_entry_moves_it_to_end_of_lru() {
        let mut store = DiagnosticsStore::with_capacity(2);
        let key = server_key(ServerKind::Rust);

        store.publish(
            key.clone(),
            PathBuf::from("/a.rs"),
            vec![diag("/a.rs", 1, "a", DiagnosticSeverity::Warning)],
        );
        store.publish(
            key.clone(),
            PathBuf::from("/b.rs"),
            vec![diag("/b.rs", 1, "b", DiagnosticSeverity::Warning)],
        );

        // Re-publish /a.rs — this should refresh its LRU position so it's
        // newer than /b.rs. Inserting /c.rs should now evict /b.rs.
        store.publish(
            key.clone(),
            PathBuf::from("/a.rs"),
            vec![diag("/a.rs", 1, "a2", DiagnosticSeverity::Error)],
        );
        store.publish(
            key.clone(),
            PathBuf::from("/c.rs"),
            vec![diag("/c.rs", 1, "c", DiagnosticSeverity::Warning)],
        );

        assert!(store.has_any_report_for_file(Path::new("/a.rs")));
        assert!(!store.has_any_report_for_file(Path::new("/b.rs")));
        assert!(store.has_any_report_for_file(Path::new("/c.rs")));
    }

    #[test]
    fn capacity_zero_disables_eviction() {
        let mut store = DiagnosticsStore::with_capacity(0);
        let key = server_key(ServerKind::Rust);

        for i in 0..50 {
            store.publish(
                key.clone(),
                PathBuf::from(format!("/f{i}.rs")),
                vec![diag(
                    &format!("/f{i}.rs"),
                    1,
                    "x",
                    DiagnosticSeverity::Warning,
                )],
            );
        }
        assert_eq!(store.len(), 50);
    }

    #[test]
    fn set_capacity_evicts_on_shrink() {
        let mut store = DiagnosticsStore::with_capacity(0);
        let key = server_key(ServerKind::Rust);
        for i in 0..10 {
            store.publish(
                key.clone(),
                PathBuf::from(format!("/f{i}.rs")),
                vec![diag(
                    &format!("/f{i}.rs"),
                    1,
                    "x",
                    DiagnosticSeverity::Warning,
                )],
            );
        }
        assert_eq!(store.len(), 10);

        store.set_capacity(3);
        assert_eq!(store.len(), 3);
        // Most recent 3 should remain (/f7.rs, /f8.rs, /f9.rs).
        assert!(store.has_any_report_for_file(Path::new("/f9.rs")));
        assert!(!store.has_any_report_for_file(Path::new("/f0.rs")));
    }

    #[test]
    fn epoch_increments_monotonically() {
        let mut store = DiagnosticsStore::new();
        let key = server_key(ServerKind::Rust);
        let file = PathBuf::from("/e.rs");

        store.publish(key.clone(), file.clone(), Vec::new());
        let e1 = store.entries_for_file(&file)[0].1.epoch;

        store.publish(key.clone(), file.clone(), Vec::new());
        let e2 = store.entries_for_file(&file)[0].1.epoch;

        assert!(e2 > e1, "epoch must increase on republish");
    }

    #[test]
    fn result_id_is_round_tripped() {
        let mut store = DiagnosticsStore::new();
        let key = server_key(ServerKind::Rust);
        let file = PathBuf::from("/r.rs");

        store.publish_with_result_id(
            key.clone(),
            file.clone(),
            Vec::new(),
            Some("rev-42".to_string()),
        );

        let entries = store.entries_for_file(&file);
        assert_eq!(entries[0].1.result_id.as_deref(), Some("rev-42"));
    }

    #[test]
    fn clear_server_drops_all_entries_for_kind() {
        let mut store = DiagnosticsStore::new();
        let py_key = server_key(ServerKind::Python);
        let rust_key = server_key(ServerKind::Rust);

        store.publish(
            py_key.clone(),
            PathBuf::from("/a.py"),
            vec![diag("/a.py", 1, "x", DiagnosticSeverity::Error)],
        );
        store.publish(
            rust_key.clone(),
            PathBuf::from("/b.rs"),
            vec![diag("/b.rs", 1, "y", DiagnosticSeverity::Error)],
        );

        store.clear_server(ServerKind::Python);
        assert!(!store.has_any_report_for_file(Path::new("/a.py")));
        assert!(store.has_any_report_for_file(Path::new("/b.rs")));
    }

    #[test]
    fn clear_for_file_drops_every_server_entry_and_updates_counts() {
        let mut store = DiagnosticsStore::new();
        let py_key = server_key(ServerKind::Python);
        let biome_key = server_key(ServerKind::Biome);

        // Two servers both report for the SAME deleted file, plus an unrelated
        // file that must survive.
        store.publish(
            py_key,
            PathBuf::from("/gone.ts"),
            vec![diag("/gone.ts", 4, "type error", DiagnosticSeverity::Error)],
        );
        store.publish(
            biome_key,
            PathBuf::from("/gone.ts"),
            vec![diag(
                "/gone.ts",
                7,
                "lint warning",
                DiagnosticSeverity::Warning,
            )],
        );
        store.publish(
            server_key(ServerKind::Rust),
            PathBuf::from("/keep.rs"),
            vec![diag("/keep.rs", 1, "live error", DiagnosticSeverity::Error)],
        );

        assert_eq!(store.error_warning_counts(), (2, 1));

        // Clearing the deleted file drops both server entries for it.
        let removed = store.clear_for_file(Path::new("/gone.ts"));
        assert!(removed);
        assert!(!store.has_any_report_for_file(Path::new("/gone.ts")));
        // The unrelated file's diagnostic is untouched.
        assert!(store.has_any_report_for_file(Path::new("/keep.rs")));
        assert_eq!(store.error_warning_counts(), (1, 0));

        // Clearing again is a no-op (nothing left for that file).
        assert!(!store.clear_for_file(Path::new("/gone.ts")));
    }

    #[test]
    fn filtered_counts_apply_keep_predicate() {
        let mut store = DiagnosticsStore::new();
        store.publish(
            server_key(ServerKind::TypeScript),
            PathBuf::from("/repo/src/app.ts"),
            vec![diag(
                "/repo/src/app.ts",
                1,
                "in build",
                DiagnosticSeverity::Error,
            )],
        );
        store.publish(
            server_key(ServerKind::TypeScript),
            PathBuf::from("/repo/src/app.test.ts"),
            vec![diag(
                "/repo/src/app.test.ts",
                1,
                "excluded",
                DiagnosticSeverity::Error,
            )],
        );

        // Raw count sees both files.
        assert_eq!(store.error_warning_counts(), (2, 0));
        // Filtered count drops the build-excluded test file.
        let counts = store.filtered_error_warning_counts(|file| !file.ends_with("app.test.ts"));
        assert_eq!(counts, (1, 0));
    }

    #[test]
    fn filtered_counts_dedup_across_servers() {
        let mut store = DiagnosticsStore::new();
        let file = "/repo/src/app.ts";
        // Two different servers report the SAME diagnostic (same file/range/
        // severity/message/source) for one file — e.g. tsserver + a linter that
        // both surface an identical issue. Raw counting double-counts; the
        // status-bar count must collapse to one (matching inspect sort_and_dedup).
        store.publish(
            server_key(ServerKind::TypeScript),
            PathBuf::from(file),
            vec![diag(file, 7, "dup", DiagnosticSeverity::Error)],
        );
        store.publish(
            server_key(ServerKind::Biome),
            PathBuf::from(file),
            vec![diag(file, 7, "dup", DiagnosticSeverity::Error)],
        );

        assert_eq!(store.error_warning_counts(), (2, 0));
        assert_eq!(store.filtered_error_warning_counts(|_| true), (1, 0));
    }

    #[test]
    fn filtered_counts_keep_distinct_diagnostics_same_file() {
        let mut store = DiagnosticsStore::new();
        let file = "/repo/src/app.ts";
        // Two servers, genuinely different diagnostics on the same file — both
        // must be counted (dedup keys on location+message+source, not file).
        store.publish(
            server_key(ServerKind::TypeScript),
            PathBuf::from(file),
            vec![diag(file, 7, "type error", DiagnosticSeverity::Error)],
        );
        store.publish(
            server_key(ServerKind::Biome),
            PathBuf::from(file),
            vec![diag(file, 12, "lint warn", DiagnosticSeverity::Warning)],
        );
        assert_eq!(store.filtered_error_warning_counts(|_| true), (1, 1));
    }

    #[test]
    fn filtered_counts_exclude_environmental_diagnostics() {
        let mut store = DiagnosticsStore::new();
        let file = "/repo/src/app.ts";
        store.publish(
            server_key(ServerKind::TypeScript),
            PathBuf::from(file),
            vec![
                diag(
                    file,
                    1,
                    "Cannot find name 'foo'.",
                    DiagnosticSeverity::Error,
                ),
                diag(
                    file,
                    2,
                    "Failed to load schema from https://cdn.example/pkg/schema.json",
                    DiagnosticSeverity::Error,
                ),
            ],
        );
        assert_eq!(store.error_warning_counts(), (2, 0));
        assert_eq!(
            store.filtered_error_warning_counts(|_| true),
            (1, 0),
            "environmental schema-fetch must not inflate E count"
        );
    }

    #[test]
    fn environmental_flap_does_not_change_filtered_counts() {
        let mut store = DiagnosticsStore::new();
        let file = "/repo/package.json";
        let key = server_key(ServerKind::TypeScript);
        let env_msg =
            "Failed to fetch schema from https://json.schemastore.org/package.json: network";

        assert_eq!(store.filtered_error_warning_counts(|_| true), (0, 0));

        store.publish(
            key.clone(),
            PathBuf::from(file),
            vec![diag(file, 1, env_msg, DiagnosticSeverity::Error)],
        );
        assert_eq!(
            store.filtered_error_warning_counts(|_| true),
            (0, 0),
            "publish environmental diagnostic must not change filtered E/W"
        );

        store.publish(key, PathBuf::from(file), vec![]);
        assert_eq!(
            store.filtered_error_warning_counts(|_| true),
            (0, 0),
            "removing environmental diagnostic must not change filtered E/W"
        );
    }

    #[test]
    fn mixed_syntax_and_schema_fetch_counts_one_error() {
        let mut store = DiagnosticsStore::new();
        let file = "/repo/src/mixed.ts";
        store.publish(
            server_key(ServerKind::TypeScript),
            PathBuf::from(file),
            vec![
                diag(
                    file,
                    3,
                    "Cannot find name 'bar'.",
                    DiagnosticSeverity::Error,
                ),
                diag(
                    file,
                    1,
                    "Failed to resolve schema https://example.com/x.json",
                    DiagnosticSeverity::Error,
                ),
            ],
        );
        assert_eq!(
            store.filtered_error_warning_counts(|_| true),
            (1, 0),
            "classifier is per-diagnostic: one real syntax error => E1"
        );
    }

    #[test]
    fn authoritative_report_requires_settled_non_stale_entry() {
        let mut store = DiagnosticsStore::new();
        let key = server_key(ServerKind::Rust);
        let file = PathBuf::from("/tmp/auth.rs");

        // Nothing published yet: no authority.
        assert!(!store.has_authoritative_report_for_file(&file));

        // A warming (provisional) report is evidence, but not authority.
        store.publish_full_with_provisional(
            key.clone(),
            file.clone(),
            vec![diag(
                "/tmp/auth.rs",
                1,
                "warming",
                DiagnosticSeverity::Error,
            )],
            None,
            None,
            true,
        );
        assert!(store.has_any_fresh_report_for_file(&file));
        assert!(!store.has_authoritative_report_for_file(&file));

        // Settling the same server promotes its latest report to authority.
        store.promote_provisional_for_server(&key);
        assert!(store.has_authoritative_report_for_file(&file));

        // A watcher-observed edit revokes authority until the next report.
        assert!(store.mark_stale_for_file(&file).0);
        assert!(!store.has_authoritative_report_for_file(&file));
    }

    #[test]
    fn authoritative_report_includes_empty_checked_clean() {
        let mut store = DiagnosticsStore::new();
        let file = PathBuf::from("/tmp/clean.rs");
        store.publish(server_key(ServerKind::Rust), file.clone(), Vec::new());
        assert!(
            store.has_authoritative_report_for_file(&file),
            "an empty checked-clean report still proves the file was analyzed"
        );
    }
}
