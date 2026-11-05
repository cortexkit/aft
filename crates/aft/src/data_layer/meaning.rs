//! Meaning search inside a running AFT: a checkout's vector plane
//! ([`super::vectors`]), filled in the background with the configured
//! embedding model.
//!
//! It runs only with the data layer on (`AFT_DATA_LAYER=1`) and semantic
//! search switched on and admitted for the root. A fill embeds what the
//! checkout's current files need and no checkout of the repository has
//! embedded, one round of files at a time; the runtime runs each round as
//! one job of the root's maintenance, so other work on the root runs between
//! rounds. A fill starts when the checkout is built, when it goes quiet
//! after changes, and when a search finds files still waiting for vectors.
//!
//! Meaning search is ready only once every file it applies to within
//! `semantic.max_files` has its vectors or a recorded reason it has none.
//! Before that it serves the files filled so far, and the search says its
//! answer is partial; while nothing is filled it reports itself building.

use std::sync::{Arc, Mutex, MutexGuard};

use super::checkout::{Checkout, CheckoutView, DataError};
use super::vectors::{
    Coverage, FillBudget, FillProgress, FillReport, MeaningSnapshot, Producer, VectorPlane,
};
use crate::config::SemanticBackendConfig;
use crate::semantic_index::SemanticEmbeddingModel;

/// Files whose vectors one fill job publishes together: a round of the
/// fill, and the step in which searches see its progress.
const FILES_PER_ROUND: usize = 256;

/// The stage a meaning plane reports before its first fill round has
/// learned the model's identity, which needs the model loaded (and, for a
/// remote backend, asked for its vector length).
pub(crate) const STARTING_STAGE: &str = "loading_embedding_model";

/// The stage a meaning plane reports while it fills and has nothing to
/// serve yet.
pub(crate) const FILLING_STAGE: &str = "filling";

/// The stage a meaning plane reports while its fill waits for another fill
/// of the same repository, in this process or another, to publish what it
/// is embedding; it then embeds only what that one did not.
pub(crate) const WAITING_STAGE: &str = "waiting_for_another_fill";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// How a checkout's meaning search stands for one view.
#[derive(Debug, Clone)]
pub(crate) enum MeaningState {
    /// Semantic search is off or not admitted for the root.
    Off,
    /// No fill has learned the model's identity yet, so nothing can be
    /// searched: the first round is queued or loading the model.
    Starting,
    /// The last fill failed before any plane could be read, with why.
    Failed(String),
    /// The plane at the generation it read last, paired with the view.
    Serving {
        snapshot: MeaningSnapshot,
        /// The last fill's failure, kept until a fill succeeds: the files it
        /// did not reach stay pending.
        failure: Option<String>,
    },
}

/// What one fill round left to do.
#[derive(Debug)]
pub(crate) enum Round {
    /// Files remain for the next round.
    More,
    /// Every file the round set out to fill is filled.
    Done,
    /// Semantic search is off, or the checkout has nothing published yet.
    Idle,
    /// The round failed; the fill is tried again later.
    Failed(String),
}

/// The configuration a plane was made for, and the plane.
struct Configured {
    config: SemanticBackendConfig,
    plane: Arc<VectorPlane>,
}

/// One checkout's meaning search: its configuration, its plane once a fill
/// has made it, and the fill's last outcome.
#[derive(Default)]
pub(crate) struct Meaning {
    /// The root's semantic configuration as the last configure (or, before
    /// any, the first search) gave it: `Some(None)` while semantic search
    /// is off or not admitted for the root, `None` before anything gave one.
    config: Mutex<Option<Option<SemanticBackendConfig>>>,
    /// The plane for `config`, made by the first fill round under it.
    plane: Mutex<Option<Configured>>,
    /// The embedding model a fill uses, with the configuration it was made
    /// for, kept between the fill's rounds and dropped once the fill
    /// finishes, so an idle checkout holds no model. Only a fill round
    /// takes it, for the whole round.
    model: Mutex<Option<(SemanticBackendConfig, SemanticEmbeddingModel)>>,
    /// The last fill failure, until a fill round succeeds.
    failure: Mutex<Option<String>>,
    /// The last round's progress through its fill.
    progress: Mutex<Option<FillProgress>>,
}

impl Meaning {
    /// Takes `config` as the root's semantic configuration (`None` when
    /// semantic search is off for it). A different configuration drops the
    /// plane, so the next fill round loads the new model and embeds with it.
    /// Returns whether it changed.
    pub(crate) fn configure(&self, config: Option<&SemanticBackendConfig>) -> bool {
        let mut current = lock(&self.config);
        if current
            .as_ref()
            .is_some_and(|current| current.as_ref() == config)
        {
            return false;
        }
        *current = Some(config.cloned());
        drop(current);
        // A round in progress keeps its model; the next round sees the
        // configuration changed and loads the new one.
        *lock(&self.plane) = None;
        *lock(&self.failure) = None;
        *lock(&self.progress) = None;
        true
    }

    /// Takes `config()` as the root's semantic configuration unless one was
    /// given already: working it out reads the filesystem, so searches ask
    /// once and each configure gives it again ([`Self::configure`]).
    pub(crate) fn configure_once(&self, config: impl FnOnce() -> Option<SemanticBackendConfig>) {
        if lock(&self.config).is_none() {
            self.configure(config().as_ref());
        }
    }

    /// Whether semantic search is on for the root.
    pub(crate) fn enabled(&self) -> bool {
        matches!(&*lock(&self.config), Some(Some(_)))
    }

    fn config(&self) -> Option<SemanticBackendConfig> {
        lock(&self.config).clone().flatten()
    }

    /// How meaning search stands for `view`.
    pub(crate) fn state(&self, view: &CheckoutView) -> MeaningState {
        if !self.enabled() {
            return MeaningState::Off;
        }
        let plane = lock(&self.plane)
            .as_ref()
            .map(|configured| Arc::clone(&configured.plane));
        let failure = lock(&self.failure).clone();
        let Some(plane) = plane else {
            return match failure {
                Some(failure) => MeaningState::Failed(failure),
                None => MeaningState::Starting,
            };
        };
        match plane.snapshot(view) {
            Ok(snapshot) => MeaningState::Serving { snapshot, failure },
            Err(error) => MeaningState::Failed(error.to_string()),
        }
    }

    /// The last fill round's progress, if a fill ran.
    pub(crate) fn progress(&self) -> Option<FillProgress> {
        *lock(&self.progress)
    }

    /// Runs one round of the fill for `checkout`: loads the model if no
    /// round of this fill has, makes the plane if no round under this
    /// configuration has, and fills one round of files.
    pub(crate) fn round(&self, checkout: &Arc<Checkout>) -> Round {
        let Some(config) = self.config() else {
            return Round::Idle;
        };
        if checkout.view().is_none() {
            // The build that publishes the checkout starts the next fill.
            return Round::Idle;
        }
        let mut model = lock(&self.model);
        let outcome = self.fill_round(checkout, &config, &mut model);
        match &outcome {
            Round::More => {}
            Round::Done | Round::Idle => {
                *model = None;
                *lock(&self.failure) = None;
            }
            Round::Failed(error) => {
                // A failed embedder may recover; load it again next time.
                *model = None;
                *lock(&self.failure) = Some(error.clone());
            }
        }
        outcome
    }

    fn fill_round(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
        model: &mut Option<(SemanticBackendConfig, SemanticEmbeddingModel)>,
    ) -> Round {
        if model
            .as_ref()
            .is_none_or(|(made_for, _)| made_for != config)
        {
            *model = None;
            match SemanticEmbeddingModel::from_config(config) {
                Ok(loaded) => *model = Some((config.clone(), loaded)),
                Err(error) => return Round::Failed(format!("embedding model: {error}")),
            }
        }
        let (_, model) = model.as_mut().expect("loaded above");
        let plane = match self.plane_for(checkout, config, model) {
            Ok(plane) => plane,
            Err(error) => return Round::Failed(error),
        };
        let budget = FillBudget {
            max_batch: model.max_batch_size(),
            files_per_round: FILES_PER_ROUND,
        };
        let filled = plane.fill_rounds(&mut |texts| model.embed(texts), budget, 1, &mut |step| {
            *lock(&self.progress) = Some(step);
        });
        match filled {
            Err(DataError::Cancelled) => Round::Idle,
            Err(error) => Round::Failed(error.to_string()),
            Ok(report) => outcome(&report),
        }
    }

    /// The plane for `config`, made now if no round under it has: its
    /// producer is the model's fingerprint, as today's index records it.
    fn plane_for(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
        model: &mut SemanticEmbeddingModel,
    ) -> Result<Arc<VectorPlane>, String> {
        let mut slot = lock(&self.plane);
        if let Some(configured) = slot
            .as_ref()
            .filter(|configured| &configured.config == config)
        {
            return Ok(Arc::clone(&configured.plane));
        }
        let fingerprint = model
            .fingerprint(config)
            .map_err(|error| format!("embedding model: {error}"))?;
        let plane = Arc::new(VectorPlane::new(
            Arc::clone(checkout),
            Producer::from_fingerprint(&fingerprint),
            config.max_files,
        ));
        *slot = Some(Configured {
            config: config.clone(),
            plane: Arc::clone(&plane),
        });
        Ok(plane)
    }
}

/// What a round's report leaves to do.
fn outcome(report: &FillReport) -> Round {
    if let Some(error) = &report.error {
        Round::Failed(format!("embedding: {error}"))
    } else if report.files_left > 0 {
        Round::More
    } else {
        Round::Done
    }
}

/// Whether `coverage` leaves files waiting for vectors that a fill would
/// embed.
#[must_use]
pub(crate) fn waiting(coverage: &Coverage) -> bool {
    !coverage.pending.is_empty()
}
