//! aft_search's semantic lane, read from the data layer.
//!
//! The lane asks the meaning index for the chunks closest to a query
//! vector among the files a request includes, and for the vector length the
//! query must have. [`MeaningIndex`] answers from either source: today's
//! [`SemanticIndex`], or a data-layer meaning plane pinned at one generation
//! ([`DataMeaning`]). The plane returns today's rows over the same vectors
//! (see [`crate::data_layer::vectors`]), so the lane ranks the same chunks
//! the same way from either source.
//!
//! With the data layer on and semantic search switched on for the root,
//! [`DataSemantic`] says how the plane stands, from its own fill rather than
//! today's index status: ready only once every file it applies to is
//! filled, partial (it serves what is filled and the reply says what is
//! missing) while a fill is under way, and building while nothing is filled.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::context::{AppContext, SemanticIndexStatus};
use crate::data_layer::checkout::CheckoutView;
use crate::data_layer::meaning::{self, MeaningState};
use crate::data_layer::vectors::{Coverage, MeaningSnapshot};
use crate::semantic_index::{SemanticIndex, SemanticResult, SemanticSkipReason};

/// The stage a meaning plane reports while its checkout has published
/// nothing yet, so there is no view to fill.
pub(crate) const DATA_LAYER_BUILD_STAGE: &str = "data_layer_build";

/// A data-layer meaning plane pinned at one generation of the family's
/// vectors for one view, with what the last fill reported.
#[derive(Debug)]
pub(crate) struct DataMeaning {
    snapshot: MeaningSnapshot,
    /// The last fill's failure, kept until a fill succeeds.
    failure: Option<String>,
}

impl DataMeaning {
    /// Names what the lane's answers come from: the view's text generation
    /// and the generation of the vectors. A recorded change and a fill that
    /// lands each change it, so a list cached under it is never served over
    /// other vectors.
    pub(crate) fn generation(&self) -> String {
        format!(
            "{}:vectors:{}",
            self.snapshot.view_generation(),
            self.snapshot.generation()
        )
    }

    /// Logs, at debug level, that a ranking read this plane. The data-layer
    /// parity harness (`benchmarks/aft-search/compare_data_layer.py`) counts
    /// these lines to show which source answered each request.
    pub(crate) fn note_read(&self, request_id: &str) {
        crate::slog_debug!(
            "aft_search {request_id} read the data layer meaning plane at {}",
            self.generation()
        );
    }

    /// Whether every file meaning search applies to within
    /// `semantic.max_files` has its vectors, or a recorded reason it has
    /// none (too large, or it could not be cut into chunks).
    pub(crate) fn complete(&self) -> bool {
        !meaning::waiting(self.snapshot.coverage())
    }

    pub(crate) fn coverage(&self) -> &Coverage {
        self.snapshot.coverage()
    }

    /// Why the last fill stopped, while files still wait for vectors.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The vector length a query needs, once the plane holds any vector;
    /// `None` before, as today's index gives none while it is empty.
    pub(crate) fn dimension(&self) -> Option<usize> {
        (self.snapshot.chunks() > 0).then(|| self.snapshot.dimensions())
    }

    /// The summary today's index gives for chunks the embedder refused, in
    /// the same words, with how many there are and why; `None` when the
    /// embedder refused none.
    pub(crate) fn skipped_chunks(&self) -> Option<(String, usize, SemanticSkipReason)> {
        let root = self.snapshot.root();
        let failed = &self.snapshot.coverage().chunks_failed;
        if failed.is_empty() {
            return None;
        }
        let mut refused = std::collections::BTreeMap::<PathBuf, usize>::new();
        let mut exceeded = std::collections::BTreeMap::<PathBuf, usize>::new();
        for (path, _, reason) in failed {
            let counts = if reason == crate::semantic_index::PROVIDER_REFUSAL_REASON {
                &mut refused
            } else {
                &mut exceeded
            };
            *counts.entry(path.clone()).or_default() += 1;
        }
        let summary = [
            crate::semantic_index::format_skipped_chunks_summary(
                root,
                &refused,
                "the embedding provider refused them",
            ),
            crate::semantic_index::format_skipped_chunks_summary(
                root,
                &exceeded,
                "they exceed the model's input limit",
            ),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
        let reason = if refused.is_empty() {
            SemanticSkipReason::InputExceededModelContext
        } else {
            SemanticSkipReason::ProviderRefusedInput
        };
        Some((summary, failed.len(), reason))
    }
}

/// How this root's meaning search stands on the data layer.
#[derive(Debug, Clone)]
pub(crate) enum DataSemantic {
    /// The plane serves: ready, or partial while a fill is under way (see
    /// [`DataMeaning::complete`]).
    Serving(Arc<DataMeaning>),
    /// The plane cannot serve yet, with the status to report.
    NotServing(SemanticIndexStatus),
}

impl DataSemantic {
    /// This root's meaning search, with the data layer on and semantic
    /// search switched on and admitted for the root: binds the checkout if
    /// nothing has, and starts a fill when files wait for vectors. `None`
    /// otherwise (the root has no checkout in the data layer, or semantic
    /// search is off), and the caller keeps reading today's index.
    pub(crate) fn current(ctx: &AppContext) -> Option<Self> {
        if !crate::data_layer::runtime::enabled() || !ctx.config().indexes.semantic {
            return None;
        }
        let checkout = ctx.data_checkout()?;
        Self::from_state(checkout.meaning(|| ctx.data_meaning_config()))
    }

    /// As [`Self::current`] for status reads: never binds a checkout and
    /// never starts a fill.
    pub(crate) fn probe(ctx: &AppContext) -> Option<Self> {
        use crate::data_layer::surfaces::DataCheckoutProbe;
        if !crate::data_layer::runtime::enabled() || !ctx.config().indexes.semantic {
            return None;
        }
        match ctx.probe_data_checkout()? {
            DataCheckoutProbe::Bound(checkout) => Self::of_checkout(&checkout),
            _ => None,
        }
    }

    /// How `checkout`'s meaning search stands, as its last configuration
    /// left it; starts nothing. `None` while semantic search is off for it.
    pub(crate) fn of_checkout(checkout: &crate::data_layer::runtime::DataCheckout) -> Option<Self> {
        Self::from_state(checkout.meaning_status())
    }

    fn from_state(found: Option<(CheckoutView, MeaningState)>) -> Option<Self> {
        let building = |stage: &str, coverage: Option<&Coverage>| {
            Self::NotServing(SemanticIndexStatus::Building {
                stage: stage.to_string(),
                files: coverage.map(|coverage| coverage.files),
                entries_done: coverage.map(files_accounted),
                entries_total: coverage
                    .map(|coverage| coverage.files.saturating_sub(coverage.past_cap)),
            })
        };
        Some(match found {
            None => building(DATA_LAYER_BUILD_STAGE, None),
            Some((_, MeaningState::Off)) => return None,
            Some((_, MeaningState::Starting)) => building(meaning::STARTING_STAGE, None),
            Some((_, MeaningState::Failed(error))) => {
                Self::NotServing(SemanticIndexStatus::Failed(error))
            }
            Some((_, MeaningState::Serving { snapshot, failure })) => {
                let meaning = DataMeaning { snapshot, failure };
                if meaning.complete() || meaning.snapshot.chunks() > 0 {
                    Self::Serving(Arc::new(meaning))
                } else if let Some(error) = meaning.failure {
                    Self::NotServing(SemanticIndexStatus::Failed(error))
                } else {
                    building(meaning::FILLING_STAGE, Some(meaning.snapshot.coverage()))
                }
            }
        })
    }

    /// The status aft_search reports for the lane: ready whenever the plane
    /// serves (a partial answer says so itself), otherwise why it does not.
    pub(crate) fn status(&self) -> SemanticIndexStatus {
        match self {
            Self::Serving(_) => SemanticIndexStatus::ready(),
            Self::NotServing(status) => status.clone(),
        }
    }

    pub(crate) fn serving(&self) -> Option<&Arc<DataMeaning>> {
        match self {
            Self::Serving(meaning) => Some(meaning),
            Self::NotServing(_) => None,
        }
    }
}

/// Files the plane has accounted for: filled, too large, or failed with a
/// recorded reason.
fn files_accounted(coverage: &Coverage) -> usize {
    coverage.files_embedded + coverage.files_too_large + coverage.files_failed.len()
}

/// Where aft_search's semantic lane reads chunk vectors from.
#[derive(Clone, Copy)]
pub(crate) enum MeaningIndex<'a> {
    /// Today's semantic index.
    Snapshot(&'a SemanticIndex),
    /// The data layer's meaning plane.
    Data(&'a DataMeaning),
}

impl MeaningIndex<'_> {
    /// The `top_k` chunks closest to `query` among files whose absolute
    /// path `include` accepts, best first: `SemanticIndex::search_filtered`.
    ///
    /// # Errors
    ///
    /// When the data layer's store cannot be read; today's index never
    /// fails.
    pub(crate) fn search_filtered(
        &self,
        query: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> io::Result<Vec<SemanticResult>> {
        match self {
            Self::Snapshot(index) => Ok(index.search_filtered(query, top_k, include)),
            Self::Data(meaning) => meaning
                .snapshot
                .search(query, top_k, include)
                .map_err(|error| io::Error::other(error.to_string())),
        }
    }

    /// The vector length a query must have; `None` while the index holds no
    /// vector, when any length is let through.
    pub(crate) fn dimension(&self) -> Option<usize> {
        match self {
            Self::Snapshot(index) => (index.len() > 0).then(|| index.dimension()),
            Self::Data(meaning) => meaning.dimension(),
        }
    }
}
