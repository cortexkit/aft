use std::collections::HashMap;
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
    /// Document version this publish/pull was tagged against, when the
    /// server provided one. Servers that participate in versioned text
    /// document sync echo `version` on `publishDiagnostics`; we store it
    /// so post-edit waiters can reject stale publishes deterministically
    /// (`version == target_version`) instead of relying on epoch ordering
    /// alone, which has a race when an old-version publish arrives after
    /// the pre-edit drain. `None` = server didn't tag the publish.
    pub version: Option<i32>,
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
    /// Insertion/access order for LRU eviction. Most-recently-touched
    /// entries are at the END of the vector.
    order: Vec<(ServerKey, PathBuf)>,
    /// Maximum number of entries before LRU eviction kicks in. 0 = no cap.
    capacity: usize,
    /// Monotonic epoch counter. Incremented on every publish.
    next_epoch: u64,
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
            order: Vec::new(),
            capacity,
            next_epoch: 0,
            last_publish_at_for_file: HashMap::new(),
        }
    }

    /// Set or change the LRU cap. If the new cap is smaller than the
    /// current entry count, the oldest entries are evicted immediately
    /// to fit.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        if capacity > 0 {
            while self.entries.len() > capacity {
                self.evict_lru();
            }
        }
    }

    /// Number of currently-tracked entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
    /// `version` should be the LSP `version` field from `publishDiagnostics`
    /// when the server provided one, or `None` otherwise.
    pub fn publish_full(
        &mut self,
        server: ServerKey,
        file: PathBuf,
        diagnostics: Vec<StoredDiagnostic>,
        result_id: Option<String>,
        version: Option<i32>,
    ) {
        let key = (server, file);
        self.next_epoch = self.next_epoch.saturating_add(1);
        let entry = DiagnosticEntry {
            diagnostics,
            epoch: self.next_epoch,
            result_id,
            version,
        };

        self.last_publish_at_for_file
            .insert(key.clone(), Instant::now());

        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), entry);
            self.touch_existing(&key);
        } else {
            // New entry — apply LRU cap before inserting.
            if self.capacity > 0 && self.entries.len() >= self.capacity {
                self.evict_lru();
            }
            self.entries.insert(key.clone(), entry);
            self.order.push(key);
        }
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

    /// Get all diagnostics for a specific file (across all servers).
    /// Updates LRU position for each touched entry.
    pub fn for_file(&self, file: &Path) -> Vec<&StoredDiagnostic> {
        self.entries
            .iter()
            .filter(|((_, stored_file), _)| stored_file == file)
            .flat_map(|(_, entry)| entry.diagnostics.iter())
            .collect()
    }

    /// Get the full per-server entry for a file. Useful when callers need
    /// to know epoch/resultId, not just the diagnostics array.
    pub fn entries_for_file(&self, file: &Path) -> Vec<(&ServerKey, &DiagnosticEntry)> {
        self.entries
            .iter()
            .filter(|((_, stored_file), _)| stored_file == file)
            .map(|((key, _), entry)| (key, entry))
            .collect()
    }

    /// True if any server has reported (even an empty result) for this file.
    pub fn has_any_report_for_file(&self, file: &Path) -> bool {
        self.entries.keys().any(|(_, f)| f == file)
    }

    /// True if this exact server instance has reported (even an empty result)
    /// for this exact file.
    pub fn has_report_for_server_file(&self, server: &ServerKey, file: &Path) -> bool {
        self.entries
            .contains_key(&(server.clone(), file.to_path_buf()))
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
            .is_some_and(|published_at| *published_at >= since)
    }

    /// Get all diagnostics for files under a directory.
    pub fn for_directory(&self, dir: &Path) -> Vec<&StoredDiagnostic> {
        self.entries
            .iter()
            .filter(|((_, stored_file), _)| stored_file.starts_with(dir))
            .flat_map(|(_, entry)| entry.diagnostics.iter())
            .collect()
    }

    /// All stored diagnostics, flattened.
    pub fn all(&self) -> Vec<&StoredDiagnostic> {
        self.entries
            .values()
            .flat_map(|entry| entry.diagnostics.iter())
            .collect()
    }

    /// Drop all entries for a server kind (e.g., on server crash/restart).
    /// Prefer `clear_for_server` for real manager cleanup so peer roots of the
    /// same kind are not wiped.
    pub fn clear_server(&mut self, server: ServerKind) {
        self.entries
            .retain(|(stored_key, _), _| stored_key.kind != server);
        self.order
            .retain(|(stored_key, _)| stored_key.kind != server);
        self.last_publish_at_for_file
            .retain(|(stored_key, _), _| stored_key.kind != server);
    }

    /// Drop all entries for a specific server instance.
    pub fn clear_for_server(&mut self, key: &ServerKey) {
        self.entries.retain(|(k, _), _| k != key);
        self.order.retain(|(k, _)| k != key);
        self.last_publish_at_for_file.retain(|(k, _), _| k != key);
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
        self.last_publish_at_for_file.remove(&evicted);
        Some(evicted)
    }

    fn touch_existing(&mut self, key: &(ServerKey, PathBuf)) {
        if let Some(idx) = self.order.iter().position(|k| k == key) {
            let removed = self.order.remove(idx);
            self.order.push(removed);
        }
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
        assert!(store.has_any_report_for_file(Path::new("/b.rs")));
        assert!(store.has_any_report_for_file(Path::new("/c.rs")));
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
}
