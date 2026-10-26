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
use super::snapshot::{ComposedGeneration, Snapshot};
use super::RelPath;

/// Durability boundaries of view construction. V3-only boundaries are appended
/// after the older publication sequence. Tests park a child process and kill it; every
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
    /// Both v3 floor entries (registry and store) are durable before publication
    /// creates a registry or plane database.
    V3FloorRaised,
    /// All plane writes in a v3 publication are durable before its ledger commit.
    ClosureFlushed,
    /// A `base_builds` claim row names this process as the base's builder.
    BaseClaimed,
    /// The base's member row (`kind = base`, no pointer yet) is committed.
    BaseRegistered,
    /// The base pointer moved to a readiness successor with the same membership.
    BaseSuccessorPublished,
    /// A worker's per-acquisition marker and `intent` referrer row exist.
    BaseRefMarked,
    /// The worker's referrer row is `published` with its worker generation.
    ReferrerPublished,
    /// A sweep found the base unreferenced and set its counter to 1.
    BaseUnreferencedCounted,
    /// A sweep committed `evicting` for an unreferenced base before removing it.
    BaseReclaimBegun,
}

impl DurabilityStep {
    pub const V3_ONLY: [Self; 2] = [Self::V3FloorRaised, Self::ClosureFlushed];
    /// Boundaries of the base-reference protocol, in the order one base build,
    /// worker bind and reclaim reaches them.
    pub const BASE_PATH: [Self; 7] = [
        Self::BaseClaimed,
        Self::BaseRegistered,
        Self::BaseSuccessorPublished,
        Self::BaseRefMarked,
        Self::ReferrerPublished,
        Self::BaseUnreferencedCounted,
        Self::BaseReclaimBegun,
    ];
    pub const ALL: [Self; 17] = [
        Self::BlobCommitted,
        Self::SegmentRowRecorded,
        Self::SegmentFileSynced,
        Self::SegmentDurable,
        Self::ManifestWritten,
        Self::ManifestParentSynced,
        Self::PointerCas,
        Self::PointerSynced,
        Self::V3FloorRaised,
        Self::ClosureFlushed,
        Self::BaseClaimed,
        Self::BaseRegistered,
        Self::BaseSuccessorPublished,
        Self::BaseRefMarked,
        Self::ReferrerPublished,
        Self::BaseUnreferencedCounted,
        Self::BaseReclaimBegun,
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
            Self::V3FloorRaised => "v3-floor-raised",
            Self::ClosureFlushed => "closure-flushed",
            Self::BaseClaimed => "base-claimed",
            Self::BaseRegistered => "base-registered",
            Self::BaseSuccessorPublished => "base-successor-published",
            Self::BaseRefMarked => "base-ref-marked",
            Self::ReferrerPublished => "referrer-published",
            Self::BaseUnreferencedCounted => "base-unreferenced-counted",
            Self::BaseReclaimBegun => "base-reclaim-begun",
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

/// A failure a test injects at one durability boundary. The boundary's
/// durable action has already happened; the caller sees the error a failed
/// `fsync`, a failed `rename` or a full disk would give it right after.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InjectedFault {
    Fsync,
    Rename,
    NoSpace,
}

#[cfg(test)]
impl InjectedFault {
    pub(crate) const ALL: [Self; 3] = [Self::Fsync, Self::Rename, Self::NoSpace];
    fn error(self) -> std::io::Error {
        match self {
            // EIO, EXDEV and ENOSPC on unix. Windows maps these codes to other
            // errors; callers handle every boundary failure the same way.
            Self::Fsync => std::io::Error::from_raw_os_error(5),
            Self::Rename => std::io::Error::from_raw_os_error(18),
            Self::NoSpace => std::io::Error::from_raw_os_error(28),
        }
    }
}

#[cfg(test)]
thread_local! {
    static INJECTED: std::cell::Cell<Option<(DurabilityStep, InjectedFault)>> =
        const { std::cell::Cell::new(None) };
}

/// Makes the next [`boundary`] at `step` on this thread fail with `fault`.
#[cfg(test)]
pub(crate) fn inject_fault_at(step: DurabilityStep, fault: InjectedFault) {
    INJECTED.with(|slot| slot.set(Some((step, fault))));
}

/// [`observe`], for boundaries whose callers handle a failure there. Tests can
/// make one boundary fail with [`inject_fault_at`]; production never fails here.
pub(crate) fn boundary(
    observer: Option<&dyn DurabilityObserver>,
    step: DurabilityStep,
) -> std::io::Result<()> {
    observe(observer, step);
    #[cfg(test)]
    if let Some((at, fault)) = INJECTED.with(|slot| slot.get()) {
        if at == step {
            INJECTED.with(|slot| slot.set(None));
            return Err(fault.error());
        }
    }
    Ok(())
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

/// An error from an index plane or loader orchestration, carried into existing
/// incomplete/named-gap reporting, never turned into an empty success.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaneError {
    /// Loader failures have no persisted artifact plane.
    pub plane: Option<FamilyPlane>,
    pub reason: String,
}

impl std::fmt::Display for PlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.plane {
            Some(plane) => write!(f, "{} plane: {}", plane.as_str(), self.reason),
            None => write!(f, "view loader {}", self.reason),
        }
    }
}

impl std::error::Error for PlaneError {}

/// Loader work is not an artifact plane; name its stage instead of blaming an index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderStage {
    SeedSelection,
    Reconciliation,
    Installation,
    ProducerValidation,
    Driver,
}

impl LoaderStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::SeedSelection => "seed selection",
            Self::Reconciliation => "reconciliation",
            Self::Installation => "installation",
            Self::ProducerValidation => "producer validation",
            Self::Driver => "driver",
        }
    }
}

impl PlaneError {
    pub fn loader(stage: LoaderStage, reason: impl Into<String>) -> Self {
        Self {
            plane: None,
            reason: format!("[{}]: {}", stage.as_str(), reason.into()),
        }
    }
}

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
        generation: &Arc<ComposedGeneration>,
    ) -> Result<(), PlaneError>;

    /// Open the resident state and optionally retain its actual read handle.
    /// Adapters whose scope caches may be cleared while queries outlive them
    /// return an owned handle; simpler adapters use the release-generation lease.
    fn open_reader(
        &self,
        access: &ViewAccess,
        generation: &Arc<ComposedGeneration>,
    ) -> Result<Option<super::snapshot::PlaneAttachment>, PlaneError> {
        self.open_generation(access, generation)?;
        Ok(None)
    }

    /// Bind a shared immutable reader to this bundle's query state. This runs
    /// for every bundle, including cache hits, so a new binding cannot inherit
    /// mutable fill state from a retired binding of the same generation.
    fn bind_reader(
        &self,
        _access: &ViewAccess,
        _generation: &Arc<ComposedGeneration>,
        reader: Option<super::snapshot::PlaneAttachment>,
    ) -> Result<Option<super::snapshot::PlaneAttachment>, PlaneError> {
        Ok(reader)
    }

    /// Drops resident state for a generation that no snapshot holds.
    fn release_generation(&self, access: &ViewAccess, generation: &str);

    /// Called only after durable pointer publication for this generation succeeded.
    /// Opening draft readers must not drop live protection for unpublished fills.
    /// `current` returns installed disk facts without I/O or building; sample it
    /// after capturing a fill-state version so a concurrent completion is not lost.
    fn generation_published(
        &self,
        _access: &ViewAccess,
        _generation: &Arc<ComposedGeneration>,
        _current: &dyn Fn() -> Snapshot,
    ) -> Result<(), PlaneError> {
        Ok(())
    }

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
    fn adapter_error_keeps_its_plane_tag_and_text() {
        let error = PlaneError {
            plane: Some(FamilyPlane::Semantic),
            reason: "store unavailable".into(),
        };
        assert_eq!(error.plane, Some(FamilyPlane::Semantic));
        assert_eq!(error.to_string(), "semantic plane: store unavailable");
    }

    #[test]
    fn the_process_hooks_start_unwired() {
        assert!(PlaneHooks::default().is_unwired());
    }
}
