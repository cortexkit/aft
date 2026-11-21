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

use std::path::PathBuf;
use std::sync::Arc;

use crate::data_layer::host::IndexHost;
use crate::data_layer::meaning::{self, FillFailure, MeaningState};
use crate::data_layer::status::SemanticIndexStatus;
use crate::data_layer::vectors::{Coverage, MeaningSnapshot};
use crate::search_state::waits::{self, SearchWait};
use crate::semantic_index::SemanticSkipReason;

/// The stage a meaning plane reports while its checkout has published
/// nothing yet, so there is no view to fill.
pub(crate) const DATA_LAYER_BUILD_STAGE: &str = "data_layer_build";

/// A data-layer meaning plane pinned at one generation of the family's
/// vectors for one view, with what the last fill reported.
#[derive(Debug)]
#[doc(hidden)]
pub struct DataMeaning {
    pub snapshot: MeaningSnapshot,
    /// The last fill's failure, kept until a fill succeeds.
    failure: Option<FillFailure>,
}

impl DataMeaning {
    /// Names what the lane's answers come from: the view's text generation
    /// and the generation of the vectors. A recorded change and a fill that
    /// lands each change it, so a list cached under it is never served over
    /// other vectors.
    #[doc(hidden)]
    pub fn generation(&self) -> String {
        format!(
            "{}:vectors:{}",
            self.snapshot.view_generation(),
            self.snapshot.generation()
        )
    }

    /// Logs, at debug level, that a ranking read this plane. The data-layer
    /// parity harness (`benchmarks/aft-search/compare_data_layer.py`) counts
    /// these lines to show which source answered each request.
    #[doc(hidden)]
    pub fn note_read(&self, request_id: &str) {
        crate::slog_debug!(
            "aft_search {request_id} read the data layer meaning plane at {}",
            self.generation()
        );
    }

    /// Whether every file meaning search applies to within
    /// `semantic.max_files` is accounted for, apart from Luau outline failures.
    #[doc(hidden)]
    pub fn complete(&self) -> bool {
        !meaning::waiting(self.snapshot.coverage())
            && self
                .snapshot
                .coverage()
                .luau_outline_failures()
                .next()
                .is_none()
    }

    #[doc(hidden)]
    pub fn coverage(&self) -> &Coverage {
        self.snapshot.coverage()
    }

    /// Why the last fill stopped, while files still wait for vectors.
    #[doc(hidden)]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_ref().map(FillFailure::message)
    }

    /// The vector length a query needs, once the plane holds any vector;
    /// `None` before, as today's index gives none while it is empty.
    #[doc(hidden)]
    pub fn dimension(&self) -> Option<usize> {
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
#[doc(hidden)]
pub enum DataSemantic {
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
    #[doc(hidden)]
    pub fn current(ctx: &(impl crate::data_layer::host::IndexHost + ?Sized)) -> Option<Self> {
        if !ctx.config().indexes.semantic {
            return None;
        }
        // The context's checkout slot is held while a checkout binds (a bind
        // scans the folder); the meaning plane's snapshot can wait for a
        // generation reload or a member-cache build.
        let checkout = waits::timed(SearchWait::CheckoutSlot, || ctx.data_checkout())?;
        let _wait = waits::time(SearchWait::MeaningView);
        Self::from_state(checkout.meaning(|| ctx.data_meaning_config()))
    }

    /// As [`Self::current`] for status reads: never binds a checkout and
    /// never starts a fill.
    #[doc(hidden)]
    pub fn probe(ctx: &(impl IndexHost + ?Sized)) -> Option<Self> {
        use crate::data_layer::surfaces::DataCheckoutProbe;
        if !ctx.config().indexes.semantic {
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

    pub(crate) fn from_state(state: MeaningState) -> Option<Self> {
        Some(match state {
            MeaningState::Off => return None,
            MeaningState::Unpublished => Self::NotServing(building(DATA_LAYER_BUILD_STAGE, None)),
            MeaningState::Starting => Self::NotServing(building(meaning::STARTING_STAGE, None)),
            MeaningState::Blocked(failure) => Self::NotServing(blocked(&failure, None)),
            MeaningState::Serving { snapshot, failure } => {
                let meaning = DataMeaning { snapshot, failure };
                // Files whose outlines could not be extracted have a terminal
                // failure instead of chunks to embed. Serve a settled, possibly
                // empty snapshot so its gaps are disclosed instead of waiting
                // for embedding work that will never be queued.
                if !meaning::waiting(meaning.snapshot.coverage()) || meaning.snapshot.chunks() > 0 {
                    Self::Serving(Arc::new(meaning))
                } else {
                    let coverage = Some(meaning.snapshot.coverage());
                    Self::NotServing(match &meaning.failure {
                        Some(failure) => blocked(failure, coverage),
                        None => building(meaning::FILLING_STAGE, coverage),
                    })
                }
            }
        })
    }

    /// The status aft_search reports for the lane: ready whenever the plane
    /// serves (a partial answer says so itself), otherwise why it does not.
    #[doc(hidden)]
    pub fn status(&self) -> SemanticIndexStatus {
        match self {
            Self::Serving(_) => SemanticIndexStatus::ready(),
            Self::NotServing(status) => status.clone(),
        }
    }

    #[doc(hidden)]
    pub fn serving(&self) -> Option<&Arc<DataMeaning>> {
        match self {
            Self::Serving(meaning) => Some(meaning),
            Self::NotServing(_) => None,
        }
    }
}

/// A plane that is building at `stage`, with how far it has come when known.
fn building(stage: &str, coverage: Option<&Coverage>) -> SemanticIndexStatus {
    SemanticIndexStatus::Building {
        stage: stage.to_string(),
        files: coverage.map(|coverage| coverage.files),
        entries_done: coverage.map(files_accounted),
        entries_total: coverage.map(|coverage| coverage.files.saturating_sub(coverage.past_cap)),
    }
}

/// A plane with nothing to serve after `failure`, in the status today's
/// semantic build reports for the same failure: building while it waits for
/// the backend or for ONNX Runtime (it is retried), failed otherwise.
fn blocked(failure: &FillFailure, coverage: Option<&Coverage>) -> SemanticIndexStatus {
    match failure {
        FillFailure::Unreachable(reason) => building(
            &format!("waiting_for_embedding_backend: {reason}"),
            coverage,
        ),
        FillFailure::WaitingForRuntime => building(
            crate::semantic_index::WAITING_FOR_ONNX_RUNTIME_DOWNLOAD_STAGE,
            coverage,
        ),
        FillFailure::Failed(message) => SemanticIndexStatus::Failed(message.clone()),
    }
}

/// Files the plane has accounted for: filled, too large, or failed with a
/// recorded reason.
fn files_accounted(coverage: &Coverage) -> usize {
    coverage.files_embedded + coverage.files_too_large + coverage.files_failed.len()
}

#[cfg(test)]
#[path = "meaning_query_test_support.rs"]
pub(crate) mod test_support;
