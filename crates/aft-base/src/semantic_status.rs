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
