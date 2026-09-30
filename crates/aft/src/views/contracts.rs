//! Interfaces between the per-checkout view core and the planes built on it.
//!
//! The core (registry, family stores, manifests, snapshots, readiness, GC)
//! lives in the sibling modules. The three query planes and the loader are
//! built in parallel against the traits here:
//!
//! | owner | implements | module |
//! |---|---|---|
//! | trigram plane | [`PlaneAdapter`] for `FamilyPlane::Trigram` | `views::trigram`, `views::live_delta`, `views::intent` |
//! | semantic plane | [`PlaneAdapter`] for `FamilyPlane::Semantic` | `views::semantic`, `views::semantic_arena` |
//! | callgraph plane | [`PlaneAdapter`] for `FamilyPlane::Callgraph` | `views::callgraph` |
//! | loader and runtime | [`PlaneLoader`], [`QueryWait`] | `views::first_load`, `views::query_wait` |
//!
//! Plane owners register adapters with [`plane_hooks`]; the loader is the only
//! code that calls them from the runtime (`context.rs`, `runtime_drain.rs`,
//! `commands/configure.rs`). Nothing is registered until the planes land, and
//! with nothing registered the runtime keeps its current behaviour.
//!
//! Two kinds of caller use a view: its owner (a registered checkout) and a
//! registry reader that holds no view, such as a multi-repo parent-folder
//! session. [`ViewAccess`] names both, so every plane answers read-only
//! requests from a reader without writing, building or repairing anything.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use crate::blob_store::v2::FamilyPlane;

use super::readiness::PlaneReadiness;
use super::registry::{ReaderRegistration, ViewRegistration};
use super::snapshot::{OpenGeneration, Snapshot};
use super::RelPath;

/// Durability boundaries of view construction, in the order a publication
/// crosses them. Tests park a child process at one of them and kill it; every
/// boundary must leave protected bytes readable and the pointer either on the
/// old generation or on a complete new one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DurabilityStep {
    /// A family-store put or touch committed.
    BlobCommitted,
    /// A segment row (state `building`) committed before its file exists.
    SegmentRowRecorded,
    /// The segment file was renamed into place and synced.
    SegmentFileSynced,
    /// The segment row was marked durable.
    SegmentDurable,
    /// The manifest file has its permanent name.
    ManifestWritten,
    /// The view directory holding the manifest was synced.
    ManifestParentSynced,
    /// The pointer compare-and-swap committed.
    PointerCas,
    /// The pointer database and directory were synced.
    PointerSynced,
}

impl DurabilityStep {
    pub const ALL: [Self; 8] = [
        Self::BlobCommitted,
        Self::SegmentRowRecorded,
        Self::SegmentFileSynced,
        Self::SegmentDurable,
        Self::ManifestWritten,
        Self::ManifestParentSynced,
        Self::PointerCas,
        Self::PointerSynced,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlobCommitted => "blob-committed",
            Self::SegmentRowRecorded => "segment-row-recorded",
            Self::SegmentFileSynced => "segment-file-synced",
            Self::SegmentDurable => "segment-durable",
            Self::ManifestWritten => "manifest-written",
            Self::ManifestParentSynced => "manifest-parent-synced",
            Self::PointerCas => "pointer-cas",
            Self::PointerSynced => "pointer-synced",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.as_str() == value)
    }
}

/// Observes durability boundaries. Production passes `None`.
pub trait DurabilityObserver {
    fn reached(&self, step: DurabilityStep);
}

pub(crate) fn observe(observer: Option<&dyn DurabilityObserver>, step: DurabilityStep) {
    if let Some(observer) = observer {
        observer.reached(step);
    }
}

/// Who is using a view.
#[derive(Clone, Debug)]
pub enum ViewAccess {
    /// The checkout's own registered view: may put blobs and publish.
    Owner(ViewRegistration),
    /// A registry reader holding no view of its own, reading member `scope`.
    /// It may pin and read, and never writes family or view state.
    Reader {
        registration: Arc<ReaderRegistration>,
        scope: String,
    },
}

impl ViewAccess {
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::Reader { .. })
    }

    pub fn family(&self) -> &str {
        match self {
            Self::Owner(registration) => registration.family(),
            Self::Reader { registration, .. } => registration.family(),
        }
    }

    pub fn scope(&self) -> &str {
        match self {
            Self::Owner(registration) => registration.scope(),
            Self::Reader { scope, .. } => scope,
        }
    }
}

/// An error a plane reports to the loader. It is carried into the existing
/// incomplete/named-gap reporting, never turned into an empty success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaneError {
    pub plane: FamilyPlane,
    pub reason: String,
}

impl std::fmt::Display for PlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} plane: {}", self.plane.as_str(), self.reason)
    }
}

impl std::error::Error for PlaneError {}

/// One query plane's hooks into the view core.
pub trait PlaneAdapter: Send + Sync {
    fn plane(&self) -> FamilyPlane;

    /// The producer fingerprint of the current configuration (trigram policy,
    /// embedding model and chunker, or extractor version). Completions and
    /// manifests from any other producer are rejected.
    fn producer(&self) -> String;

    /// Whether a manifest entry at `rel_path` carries a state for this plane.
    fn applies_to(&self, rel_path: &RelPath) -> bool;

    /// Makes a newly current generation resident for `access`. Called by the
    /// loader after the generation is pinned and verified.
    fn open_generation(
        &self,
        access: &ViewAccess,
        generation: &Arc<OpenGeneration>,
    ) -> Result<(), PlaneError>;

    /// Drops resident state for a generation that no snapshot holds.
    fn release_generation(&self, access: &ViewAccess, generation: &str);

    /// This plane's readiness for the view `access` names, as of `snapshot`.
    ///
    /// The view must be named because two views can hold the same generation
    /// (a sibling seed is shared by name) while their fills and live edits
    /// differ; each must report its own pending and failed counts.
    fn readiness(&self, access: &ViewAccess, snapshot: &Snapshot) -> PlaneReadiness;
}

/// What a load produced for one view.
#[derive(Clone, Debug)]
pub struct LoadOutcome {
    pub snapshot: Snapshot,
    /// Planes that are not ready yet, with the reason, for gap reporting.
    pub pending_planes: Vec<PlaneError>,
}

/// Brings a view to a servable snapshot: selects and pins a seed or the
/// view's own generation, reconciles the checkout strictly, and installs the
/// view's own generation when it is built. A reader access never builds.
pub trait PlaneLoader: Send + Sync {
    fn load(&self, access: &ViewAccess) -> Result<LoadOutcome, PlaneError>;
}

/// The result of waiting for relevant work before answering.
#[derive(Clone, Debug)]
pub enum WaitOutcome {
    /// All relevant work is installed; answer from this snapshot.
    Installed(Snapshot),
    /// The budget ran out. Answer from this snapshot, taken after the wait,
    /// and name these files as not yet reflected (`complete: false`).
    TimedOut {
        snapshot: Snapshot,
        unreflected: Vec<PathBuf>,
    },
}

/// The bounded wait that callgraph-consuming queries (including inspect and
/// dead code) take before answering: about three seconds in total, not reset
/// by edits that arrive during the wait.
pub trait QueryWait: Send + Sync {
    fn wait_for(&self, access: &ViewAccess, plane: FamilyPlane, budget: Duration) -> WaitOutcome;
}

/// The callgraph wait budget the runtime uses.
pub const CALLGRAPH_QUERY_WAIT: Duration = Duration::from_secs(3);

/// Registered plane adapters and the loader. Empty until the planes land.
#[derive(Default)]
pub struct PlaneHooks {
    adapters: RwLock<Vec<Arc<dyn PlaneAdapter>>>,
    loader: RwLock<Option<Arc<dyn PlaneLoader>>>,
    wait: RwLock<Option<Arc<dyn QueryWait>>>,
}

impl PlaneHooks {
    /// Registers `adapter`, replacing any adapter already registered for the
    /// same plane.
    pub fn register_adapter(&self, adapter: Arc<dyn PlaneAdapter>) {
        let mut adapters = self
            .adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        adapters.retain(|existing| existing.plane() != adapter.plane());
        adapters.push(adapter);
    }

    pub fn adapter(&self, plane: FamilyPlane) -> Option<Arc<dyn PlaneAdapter>> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|adapter| adapter.plane() == plane)
            .cloned()
    }

    pub fn adapters(&self) -> Vec<Arc<dyn PlaneAdapter>> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_loader(&self, loader: Arc<dyn PlaneLoader>) {
        *self
            .loader
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(loader);
    }

    pub fn loader(&self) -> Option<Arc<dyn PlaneLoader>> {
        self.loader
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_query_wait(&self, wait: Arc<dyn QueryWait>) {
        *self
            .wait
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wait);
    }

    pub fn query_wait(&self) -> Option<Arc<dyn QueryWait>> {
        self.wait
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// True when no plane and no loader is registered, which is the case until
    /// the per-checkout runtime is wired. Runtime hooks return early on it.
    pub fn is_unwired(&self) -> bool {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && self.loader().is_none()
    }
}

/// The process-wide hook table.
pub fn plane_hooks() -> &'static PlaneHooks {
    static HOOKS: OnceLock<PlaneHooks> = OnceLock::new();
    HOOKS.get_or_init(PlaneHooks::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durability_steps_round_trip_their_names() {
        for step in DurabilityStep::ALL {
            assert_eq!(DurabilityStep::parse(step.as_str()), Some(step));
        }
    }

    #[test]
    fn the_process_hooks_start_unwired() {
        assert!(PlaneHooks::default().is_unwired());
    }
}
