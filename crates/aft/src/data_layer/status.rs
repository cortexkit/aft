use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

// `SemanticIndexStatus::Ready` exposes a unique `refreshing` path list. Keep
// per-path queue accounting separately so repeated edits to the same file do not
// let an older refresh completion remove the path while newer work is pending.
#[derive(Debug, Default, Clone)]
#[doc(hidden)]
pub struct SemanticRefreshAccounting {
    #[doc(hidden)]
    pub pending: usize,
    #[doc(hidden)]
    pub in_flight: usize,
}

fn ensure_refreshing_path(refreshing: &mut Vec<PathBuf>, path: PathBuf) {
    if !refreshing.iter().any(|existing| existing == &path) {
        refreshing.push(path);
        refreshing.sort();
    }
}

fn remove_refreshing_path(refreshing: &mut Vec<PathBuf>, path: &Path) {
    refreshing.retain(|existing| existing != path);
}

#[derive(Debug, Clone)]
pub enum SemanticIndexStatus {
    Disabled,
    Building {
        /// Cold-build only — index is not queryable.
        stage: String,
        files: Option<usize>,
        entries_done: Option<usize>,
        entries_total: Option<usize>,
    },
    Ready {
        /// Files currently being re-embedded after recent edits. The index is
        /// still queryable; results for these files may be temporarily missing.
        refreshing: Vec<PathBuf>,
        /// Per-root queue accounting for repeated refreshes of the same path.
        /// Kept on the status value so two AppContexts in one process cannot
        /// share refresh-completion state.
        #[doc(hidden)]
        accounting: BTreeMap<PathBuf, SemanticRefreshAccounting>,
    },
    Failed(String),
}

impl SemanticIndexStatus {
    pub fn ready() -> Self {
        Self::Ready {
            refreshing: Vec::new(),
            accounting: BTreeMap::new(),
        }
    }

    pub fn add_refreshing_file(&mut self, path: PathBuf) {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            let state = accounting.entry(path.clone()).or_default();
            state.pending = state.pending.saturating_add(1);
            ensure_refreshing_path(refreshing, path);
        }
    }

    pub fn start_refreshing_file(&mut self, path: PathBuf) {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            let state = accounting.entry(path.clone()).or_default();
            if state.pending == 0 {
                state.pending = 1;
            }
            if state.in_flight == 0 {
                state.in_flight = state.pending;
            }
            ensure_refreshing_path(refreshing, path);
        }
    }

    pub fn cancel_refreshing_file(&mut self, path: &Path) {
        self.finish_refreshing_file(path, false);
    }

    /// Take every file currently tracked as refreshing, clearing the
    /// accounting. Used when the refresh worker is cancelled outright: the
    /// caller re-queues the paths for a replacement worker.
    pub fn take_refreshing_files(&mut self) -> Vec<PathBuf> {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            accounting.clear();
            std::mem::take(refreshing)
        } else {
            Vec::new()
        }
    }

    /// True while a corpus-wide (not per-file) refresh is running.
    pub fn corpus_refresh_in_flight(&self) -> bool {
        matches!(self, Self::Building { stage, .. } if stage == "refreshing_corpus")
    }

    pub fn complete_refreshing_file(&mut self, path: &Path) {
        self.finish_refreshing_file(path, true);
    }

    pub fn remove_refreshing_file(&mut self, path: &Path) {
        self.complete_refreshing_file(path);
    }

    fn finish_refreshing_file(&mut self, path: &Path, complete_in_flight: bool) {
        if let Self::Ready {
            refreshing,
            accounting,
        } = self
        {
            let mut keep_refreshing = false;
            if let Some(state) = accounting.get_mut(path) {
                let finished = if complete_in_flight {
                    state.in_flight.max(1)
                } else {
                    1
                };
                state.pending = state.pending.saturating_sub(finished);
                if complete_in_flight {
                    state.in_flight = 0;
                } else {
                    state.in_flight = state.in_flight.min(state.pending);
                }
                keep_refreshing = state.pending > 0;
                if !keep_refreshing {
                    accounting.remove(path);
                }
            }

            if !keep_refreshing {
                remove_refreshing_path(refreshing, path);
            }
        }
    }

    pub fn refreshing_count(&self) -> usize {
        match self {
            Self::Ready { refreshing, .. } => refreshing.len(),
            _ => 0,
        }
    }

    /// Number of files waiting to be re-embedded when that number is large
    /// enough that the semantic lane is materially partial, or `None` when the
    /// index is serving (nearly) everything it holds.
    ///
    /// Every file in `refreshing` has had its vectors masked, so a query cannot
    /// match it until the refresh lands. For a handful of edited files that gap
    /// is small and short, and flipping every status surface away from `ready`
    /// on each save would make the status flicker for no useful signal. A burst
    /// of hundreds of files (a branch switch, a copied tree, a restore) is
    /// different: the index keeps answering, but a large slice of the project
    /// is missing from it, and reporting `ready` then is not honest.
    pub fn mass_refresh_pending(&self) -> Option<usize> {
        let pending = self.refreshing_count();
        (pending >= SEMANTIC_MASS_REFRESH_MIN_FILES).then_some(pending)
    }
}

/// Masked-file count at which a ready semantic index reports `refreshing`
/// instead of `ready`, and search replies call the semantic lane partial.
///
/// Every masked file stays missing for at least the refresh worker's quiet
/// window (15 s by default) plus its re-embed, so the question is not how long
/// the gap lasts but how much of the project it hides. An ordinary edit batch
/// (one save, an agent's multi-file edit, a small patch) touches single-digit
/// files; reporting those would flip the status on nearly every edit. The
/// bursts that hide a real part of the index are an order of magnitude larger:
/// the watcher already treats a batch above 256 paths as oversized, and a
/// branch switch or copied tree reaches hundreds to thousands of files. 32 sits
/// well clear of the first group and well below the second.
pub const SEMANTIC_MASS_REFRESH_MIN_FILES: usize = 32;

/// Degraded reason recorded while a project's checkout view fails to load.
/// Cleared by the next successful load; the history counts are kept.
/// Rendered as-is by status surfaces, like the other degraded reasons.
pub(crate) const VIEW_LOAD_DEGRADED_REASON: &str = "view_load_failed";

/// Add reporting guidance only when a view-load failure becomes an agent reply.
/// Busy loads and measured disk-reserve refusals are expected conditions,
/// not internal errors that need a bug report.
pub(crate) fn view_load_failure_message(error: impl std::fmt::Display) -> String {
    let error = error.to_string();
    if crate::view_storage_reserve::is_message(&error) {
        return error;
    }
    format!("{error}\nThis is an AFT bug: tell the user to report it with `npx @cortexkit/aft doctor --issue`; don't run it yourself.")
}

/// Render an index read failure as the call's error. No other index or
/// filesystem walk answers in its place.
pub(crate) fn view_load_error_response(id: &str, message: String) -> crate::protocol::Response {
    crate::protocol::Response::error(id, "internal_error", message)
}

/// Error-level log throttle: one line per project and plane per minute.
/// Repeats inside the window are counted and reported on the next emission.
pub(crate) const VIEW_LOAD_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// The most recent checkout view load failure of a project. Planes are the
/// query planes a failed load serves: `trigram` for the lexical view behind
/// grep, glob and aft_search, `callgraph` for the graph behind navigation,
/// zoom and inspect, and `semantic` when a built semantic view cannot serve
/// or score a query. The semantic lane still answers `unavailable` with its
/// cause and the query continues lexically; health counts the failure too.
/// Pending fill coverage is normal progress and is not counted.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ViewLoadFailure {
    pub plane: String,
    /// Bucketed cause (`busy`, `io`, `invalid`, `unavailable` or `other`);
    /// the full error stays in `error`.
    pub cause: String,
    pub error: String,
    /// Failures with this plane and cause since this process started.
    pub count: u64,
    /// All view load failures since this process started.
    pub total: u64,
    pub last_at_ms: u64,
}

/// Bucket a view load error into a stable short cause. I/O failures already
/// name their path and operation; verification and manifest problems name
/// what mismatches or is missing; the full text stays in the recorded error.
pub(crate) fn view_load_cause(error: &str) -> &'static str {
    let error = error.to_lowercase();
    if error.contains("busy") {
        "busy"
    } else if error.contains("view i/o failed") {
        "io"
    } else if error.contains("invalid")
        || error.contains("mismatch")
        || error.contains("missing")
        || error.contains("unreadable")
        || error.contains("unverified")
        || error.contains("verification")
    {
        "invalid"
    } else if error.contains("unavailable")
        || error.contains("not ready")
        || error.contains("not built")
        || error.contains("disabled")
    {
        "unavailable"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_load_cause_buckets_errors_by_kind() {
        // Transient contention, whatever database reports it.
        assert_eq!(
            view_load_cause("callgraph derived store is busy: database is locked"),
            "busy"
        );
        assert_eq!(
            view_load_cause("view loader [driver]: v3 store busy: members.sqlite: checkpoint WAL"),
            "busy"
        );
        // I/O errors name their path and operation.
        assert_eq!(
            view_load_cause(
                "view loader [driver]: view I/O failed reading /root/a.ts: No such file"
            ),
            "io"
        );
        // Verification and manifest problems.
        assert_eq!(
            view_load_cause("invalid view manifest: duplicate member"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("view producer mismatch: trigram policy changed"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("view generation abc manifest unreadable: gone"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("manifest references missing trigram blob deadbeef"),
            "invalid"
        );
        assert_eq!(
            view_load_cause("lexical view has unverified writes"),
            "invalid"
        );
        // Loads that cannot serve yet or anymore.
        assert_eq!(
            view_load_cause("lexical payload not ready: /root/a.ts"),
            "unavailable"
        );
        assert_eq!(
            view_load_cause(
                "callgraph view unavailable: view loader [reconciliation]: walked away"
            ),
            "unavailable"
        );
        assert_eq!(
            view_load_cause(
                "call graph is disabled (indexes.callgraph=false): this view generation was published without call graph data"
            ),
            "unavailable"
        );
        assert_eq!(
            view_load_cause("lexical source deliberately refused"),
            "other"
        );
    }
}
