//! Sibling-seeded loading contracts. These hooks are opt-in until all planes
//! can build checkout generations; registering a driver does not change legacy routing.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::blob_store::v2::FamilyPlane;
pub use crate::watcher_backend::cookie::{CookieObservation, CookieTicket};

use super::contracts::{LoaderStage, PlaneError, ViewAccess};
use super::manifest_v2::Producers;
use super::snapshot::{
    ComposedGeneration, InstallCut, LiveDelta, LiveEntry, PlaneReaders, Snapshot,
};
use super::RelPath;

#[cfg(test)]
#[path = "first_load/overflow_recovery_tests.rs"]
mod overflow_recovery_tests;

#[cfg(test)]
#[path = "first_load/publication_fence_tests.rs"]
mod publication_fence_tests;

/// A complete strict content walk under the checkout's current ignore rules.
/// The revision must change for every delivered edit, write intent, watcher gap,
/// or membership change; a partial walk must return an error, not this value.
#[derive(Clone, Debug)]
pub struct ReconciledCheckout {
    pub revision: u64,
    pub entries: BTreeMap<RelPath, LiveEntry>,
    pub cut: Option<InstallCut>,
    /// Content-bound readiness already known at the strict cut.
    pub readiness: BTreeMap<RelPath, super::snapshot::PlanePatch>,
    /// Outer None delegates CAS to an external driver; Some(None) is an empty pointer.
    pub publication_base: Option<Option<String>>,
}

/// The runtime owner supplies source reads and generation construction. Plane
/// implementations attach their data here rather than writing shared runtime code.
pub trait FirstLoadDriver: Send + Sync {
    fn validate_cut(
        &self,
        access: &ViewAccess,
        _snapshot: &Snapshot,
        revision: u64,
    ) -> Result<(), PlaneError> {
        if self.revision(access) == revision {
            Ok(())
        } else {
            Err(PlaneError::loader(
                LoaderStage::Installation,
                CHECKOUT_CHANGED_BEFORE_INSTALL,
            ))
        }
    }
    fn installed_after(&self, fallback: &Snapshot) -> Snapshot {
        fallback.clone()
    }
    /// The loader calls this before choosing a seed. It must not write or block
    /// on model work; every selected manifest must match these producer identities.
    fn producers(&self, access: &ViewAccess) -> Producers;

    /// The loader calls this for seed preference only. No HEAD is valid for a
    /// copied or non-git tree; it must not replace the strict membership walk.
    fn head_tree(&self, access: &ViewAccess) -> Option<String>;

    /// The loader calls this on owners only. It may block while walking and
    /// hashing every current member, attaching plane data from the same bytes.
    /// It must not publish or write another checkout's artifacts.
    fn reconcile(&self, access: &ViewAccess) -> Result<ReconciledCheckout, PlaneError>;

    /// Called after the walk, and again before installation, to detect edits
    /// made during loading. This must not block or write any filesystem state.
    fn revision(&self, access: &ViewAccess) -> u64;

    /// Called on owners after strict reconciliation. It may block to build and
    /// publish the owner's generation under the core protection and compare-and-swap protocol.
    /// It must return a pinned, verified generation, never merely a pointer.
    /// A foreign derived graph may be copied only when `seed_derived` is true.
    fn build_own_generation(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        seed_derived: bool,
    ) -> Result<Arc<ComposedGeneration>, PlaneError>;

    /// Called with a reconciled seed snapshot, then with the rebased own snapshot.
    /// It atomically installs resident data and snapshot under the root lock,
    /// replaying delivered edits and refusing a changed epoch or bundle identity.
    /// It may block on that lock, never build, publish or repair foreign state.
    fn install(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        revision: u64,
    ) -> Result<(), PlaneError>;
}

/// The query router supplies an atomic installed snapshot and its actual gaps.
/// Pointer publication alone must not satisfy this interface.
pub trait QueryState: Send + Sync {
    /// Called repeatedly during a bounded wait, including once after timeout.
    /// It may briefly block on the root lock, but must never build, repair, or
    /// write any artifacts. Gaps include pending, failed, dirty and intent paths
    /// for this plane even when another plane is ready.
    fn installed_state(
        &self,
        access: &ViewAccess,
        plane: FamilyPlane,
    ) -> (Snapshot, Vec<std::path::PathBuf>);
}

/// Load failures caused by the checkout changing while it was loaded. They
/// clear by themselves on the next attempt, so callers retry them.
pub const CHECKOUT_CHANGED_DURING_RECONCILE: &str =
    "checkout changed during strict reconciliation; retry load";
pub const CHECKOUT_WRITE_ACTIVE: &str = "checkout write still active during reconciliation";
pub const CHECKOUT_CHANGED_BEFORE_INSTALL: &str = "checkout changed before snapshot installation";

/// The reason a [`PublishFence`] gives when it refuses a publication.
pub const SUPERSEDED_PUBLISH: &str = "checkout view owner superseded; publication refused";

/// Operations supplied by the durable base/referrer owner. The defaults refuse
/// every operation: a legacy view registration does not protect a shared base.
/// The registry implementation must supply markers that pin the exact base,
/// intent rows that protect the pending worker-to-base reference, and flushes
/// that make every referenced artifact durable before publishing the pointer.
pub trait PublicationBinding {
    fn cut(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn acquire(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn build_and_flush(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn open_readers(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn verify_held(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn publish(&mut self) -> Result<super::manifest_v2::PublishV2, PlaneError> {
        Err(binding_unavailable())
    }
    /// Hold the ownership PublishFence across final marker verification and
    /// pointer CAS, and return only after the pointer is synced. Keeping both
    /// operations in one callback prevents a validation-to-CAS ownership race.
    /// Mark `MayHaveLanded` immediately before CAS, before any error can hide a
    /// committed transaction. Leave it `NotAttempted` if verification refuses.
    fn publish_verified(
        &mut self,
        _progress: &mut PublicationProgress,
    ) -> Result<super::manifest_v2::PublishV2, PlaneError> {
        Err(binding_unavailable())
    }
    fn referrer_published(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn install_replayed(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    fn schedule_revalidated(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    /// Preserve intent and temporary protection for a publication that landed or
    /// whose CAS outcome is uncertain. Recovery must reconcile the pointer and
    /// finish its referrer/install; it must not abort the published reference.
    fn recover_published(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
    /// Remove the temporary insertion intent and acquisition marker after a
    /// known unpublished attempt, or after InstalledBundle.readers retains
    /// independent ComposedGeneration residency markers. Never called to abort
    /// a landed or uncertain CAS after a later step failed.
    fn terminal_cleanup(&mut self) -> Result<(), PlaneError> {
        Err(binding_unavailable())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PublicationProgress {
    #[default]
    NotAttempted,
    /// Set before invoking CAS: a storage error may hide a committed transaction.
    MayHaveLanded,
    Published,
}

fn binding_unavailable() -> PlaneError {
    PlaneError::loader(
        LoaderStage::Driver,
        "durable base publication binding is not wired",
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingOutcome {
    Installed,
    Revalidate,
}

/// The sole shared-base publication/install ordering seam. `publish_verified`
/// holds the ownership fence across verification and the synced pointer CAS; `install_replayed` must hold the root write lock through journal
/// replay, epoch/base checks and the exact installed-bundle identity check.
pub fn publish_and_install(
    binding: &mut dyn PublicationBinding,
) -> Result<BindingOutcome, PlaneError> {
    let mut progress = PublicationProgress::NotAttempted;
    let result: Result<BindingOutcome, PlaneError> = (|| {
        binding.cut()?;
        binding.acquire()?;
        binding.build_and_flush()?;
        binding.open_readers()?;
        match binding.publish_verified(&mut progress)? {
            super::manifest_v2::PublishV2::Published => {
                progress = PublicationProgress::Published;
            }
            super::manifest_v2::PublishV2::Conflict { .. }
            | super::manifest_v2::PublishV2::EquivalentWinner { .. } => {
                progress = PublicationProgress::NotAttempted;
                return Ok(BindingOutcome::Revalidate);
            }
        }
        binding.referrer_published()?;
        binding.install_replayed()?;
        Ok(BindingOutcome::Installed)
    })();
    let recovering = result.is_err() && progress != PublicationProgress::NotAttempted;
    let cleanup = if recovering {
        binding.recover_published()
    } else {
        binding.terminal_cleanup()
    };
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(SiblingLoader::error(format!(
            "publication failed: {}; {} failed: {}",
            error.reason,
            if recovering {
                "published recovery"
            } else {
                "terminal cleanup"
            },
            cleanup.reason
        ))),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(outcome), Ok(())) => {
            if outcome == BindingOutcome::Revalidate {
                binding.schedule_revalidated()?;
            }
            Ok(outcome)
        }
    }
}

#[cfg(test)]
mod publication_binding_tests {
    use super::super::manifest_v2::PublishV2;
    use super::*;

    struct DefaultBinding;
    impl PublicationBinding for DefaultBinding {}

    #[test]
    fn unwired_publication_binding_refuses_every_step() {
        let mut binding = DefaultBinding;
        assert!(binding.cut().is_err());
        assert!(binding.acquire().is_err());
        assert!(binding.build_and_flush().is_err());
        assert!(binding.open_readers().is_err());
        assert!(binding.verify_held().is_err());
        assert!(binding.publish().is_err());
        let mut progress = PublicationProgress::NotAttempted;
        assert!(binding.publish_verified(&mut progress).is_err());
        assert_eq!(progress, PublicationProgress::NotAttempted);
        assert!(binding.referrer_published().is_err());
        assert!(binding.install_replayed().is_err());
        assert!(binding.schedule_revalidated().is_err());
        assert!(binding.recover_published().is_err());
        assert!(binding.terminal_cleanup().is_err());
        let error = publish_and_install(&mut binding).unwrap_err();
        assert!(error.reason.contains("terminal cleanup failed"));
    }

    struct Binding {
        steps: Vec<&'static str>,
        fail: Option<&'static str>,
        outcome: PublishV2,
        intent: Option<std::path::PathBuf>,
    }
    impl Binding {
        fn step(&mut self, name: &'static str) -> Result<(), PlaneError> {
            self.steps.push(name);
            if self.fail == Some(name) {
                Err(SiblingLoader::error(name))
            } else {
                Ok(())
            }
        }
    }
    impl PublicationBinding for Binding {
        fn cut(&mut self) -> Result<(), PlaneError> {
            self.step("cut")
        }
        fn acquire(&mut self) -> Result<(), PlaneError> {
            self.step("acquire")?;
            if let Some(path) = &self.intent {
                std::fs::write(path, b"pending reference").unwrap();
            }
            Ok(())
        }
        fn build_and_flush(&mut self) -> Result<(), PlaneError> {
            self.step("closure-flushed")
        }
        fn open_readers(&mut self) -> Result<(), PlaneError> {
            self.step("readers-opened")
        }
        fn verify_held(&mut self) -> Result<(), PlaneError> {
            self.step("verify-held")
        }
        fn publish(&mut self) -> Result<PublishV2, PlaneError> {
            self.step("pointer-synced")?;
            Ok(self.outcome.clone())
        }
        fn publish_verified(
            &mut self,
            progress: &mut PublicationProgress,
        ) -> Result<PublishV2, PlaneError> {
            self.verify_held()?;
            *progress = PublicationProgress::MayHaveLanded;
            self.publish()
        }
        fn referrer_published(&mut self) -> Result<(), PlaneError> {
            self.step("referrer-published")
        }
        fn install_replayed(&mut self) -> Result<(), PlaneError> {
            self.step("installed")
        }
        fn schedule_revalidated(&mut self) -> Result<(), PlaneError> {
            self.step("revalidated-job")
        }
        fn terminal_cleanup(&mut self) -> Result<(), PlaneError> {
            self.step("terminal-cleanup")?;
            if let Some(path) = &self.intent {
                if path.exists() {
                    std::fs::remove_file(path).unwrap();
                }
            }
            Ok(())
        }
        fn recover_published(&mut self) -> Result<(), PlaneError> {
            self.step("published-recovery")
        }
    }

    #[test]
    fn publication_binding_nine_steps_and_phase_appropriate_failure_recovery() {
        let expected = [
            "cut",
            "acquire",
            "closure-flushed",
            "readers-opened",
            "verify-held",
            "pointer-synced",
            "referrer-published",
            "installed",
            "terminal-cleanup",
        ];
        let mut binding = Binding {
            steps: Vec::new(),
            fail: None,
            outcome: PublishV2::Published,
            intent: None,
        };
        assert_eq!(
            publish_and_install(&mut binding).unwrap(),
            BindingOutcome::Installed
        );
        assert_eq!(binding.steps, expected);
        for (index, step) in expected.iter().enumerate() {
            let mut binding = Binding {
                steps: Vec::new(),
                fail: Some(step),
                outcome: PublishV2::Published,
                intent: None,
            };
            assert!(publish_and_install(&mut binding).is_err(), "{step}");
            let mut prefix = expected[..=index].to_vec();
            if *step != "terminal-cleanup" {
                prefix.push(if index >= 5 {
                    "published-recovery"
                } else {
                    "terminal-cleanup"
                });
            }
            assert_eq!(binding.steps, prefix, "{step}");
        }
    }

    #[test]
    fn losing_and_equivalent_jobs_cleanup_then_revalidate_never_install() {
        for outcome in [
            PublishV2::Conflict {
                current: Some("foreign".into()),
            },
            PublishV2::EquivalentWinner {
                current: "equivalent".into(),
            },
        ] {
            let mut binding = Binding {
                steps: Vec::new(),
                fail: None,
                outcome,
                intent: None,
            };
            assert_eq!(
                publish_and_install(&mut binding).unwrap(),
                BindingOutcome::Revalidate
            );
            assert_eq!(
                binding.steps,
                [
                    "cut",
                    "acquire",
                    "closure-flushed",
                    "readers-opened",
                    "verify-held",
                    "pointer-synced",
                    "terminal-cleanup",
                    "revalidated-job"
                ]
            );
        }
    }

    #[test]
    fn landed_or_ambiguous_cas_failure_preserves_insertion_protection() {
        for failed in ["pointer-synced", "referrer-published", "installed"] {
            let directory = tempfile::tempdir().unwrap();
            let intent = directory.path().join("insertion-intent");
            let mut binding = Binding {
                steps: Vec::new(),
                fail: Some(failed),
                outcome: PublishV2::Published,
                intent: Some(intent.clone()),
            };
            assert!(publish_and_install(&mut binding).is_err());
            assert!(
                intent.is_file(),
                "{failed}: recovery discarded the published reference's protection"
            );
            assert!(!binding.steps.contains(&"terminal-cleanup"));
            assert_eq!(binding.steps.last(), Some(&"published-recovery"));
        }
        let directory = tempfile::tempdir().unwrap();
        let intent = directory.path().join("insertion-intent");
        let mut binding = Binding {
            steps: Vec::new(),
            fail: Some("verify-held"),
            outcome: PublishV2::Published,
            intent: Some(intent.clone()),
        };
        assert!(publish_and_install(&mut binding).is_err());
        assert!(
            !intent.exists(),
            "a known pre-CAS refusal should release its temporary protection"
        );
    }
}

/// Decides, atomically with the pointer publication itself, whether the
/// driver may still publish its checkout's view. A runtime that can be
/// replaced by another owner of the same view (a lane restarted with other
/// settings) installs one, so the replaced owner can never overwrite the
/// replacement's publication.
pub trait PublishFence: Send + Sync {
    /// Runs `commit` while holding whatever makes the ownership check and
    /// the publication one atomic step, or returns an error with reason
    /// [`SUPERSEDED_PUBLISH`] without running it.
    fn publish(&self, commit: &mut dyn FnMut() -> Result<(), PlaneError>)
        -> Result<(), PlaneError>;
}

/// Derived graphs cannot be copied between checkouts until tests prove their
/// rows contain no source-root-dependent data. Per-file blobs remain reusable.
pub const DERIVED_CALLGRAPH_SEEDING: bool = false;

/// Opt-in orchestration. Construction does not register process-global hooks.
pub struct SiblingLoader {
    driver: Arc<dyn FirstLoadDriver>,
    adapters: Vec<Arc<dyn super::contracts::PlaneAdapter>>,
}

impl SiblingLoader {
    pub fn new(
        driver: Arc<dyn FirstLoadDriver>,
        adapters: Vec<Arc<dyn super::contracts::PlaneAdapter>>,
    ) -> Self {
        Self { driver, adapters }
    }

    fn error(reason: impl Into<String>) -> PlaneError {
        PlaneError::loader(LoaderStage::Driver, reason)
    }

    fn stage_error(stage: LoaderStage, reason: impl Into<String>) -> PlaneError {
        PlaneError::loader(stage, reason)
    }

    fn seed(&self, access: &ViewAccess) -> Result<Arc<ComposedGeneration>, PlaneError> {
        let producers = self.driver.producers(access);
        if let ViewAccess::Reader {
            registration,
            scope,
        } = access
        {
            return super::read::open_foreign_generation(registration, scope, &producers)
                .map_err(|error| Self::stage_error(LoaderStage::SeedSelection, error.to_string()))?
                .ok_or_else(|| {
                    Self::stage_error(
                        LoaderStage::SeedSelection,
                        format!("foreign view {scope} unavailable: no generation"),
                    )
                });
        }
        let ViewAccess::Owner(owner) = access else {
            unreachable!()
        };
        let reader = owner
            .registry()
            .register_reader("first-load")
            .map_err(|error| Self::stage_error(LoaderStage::SeedSelection, error.to_string()))?;
        let mut candidates = Vec::new();
        let head = self.driver.head_tree(access);
        for member in reader
            .members()
            .map_err(|error| Self::stage_error(LoaderStage::SeedSelection, error.to_string()))?
        {
            // A broken sibling is not repaired and cannot prevent a full walk.
            if let Ok(Some(generation)) =
                super::read::open_foreign_generation(&reader, &member.scope, &producers)
            {
                let exact_head = head.is_some() && generation.header().head_tree == head;
                candidates.push((
                    member.scope == owner.scope(),
                    exact_head,
                    member.last_publish_ms,
                    member.scope,
                    generation,
                ));
            }
        }
        candidates.sort_by(|a, b| (&b.0, &b.1, &b.2, &a.3).cmp(&(&a.0, &a.1, &a.2, &b.3)));
        if let Some((_, _, _, _, generation)) = candidates.into_iter().next() {
            return Ok(generation);
        }
        Ok(Arc::new(ComposedGeneration::new(
            "empty",
            super::manifest_v2::ManifestV2::new(super::manifest_v2::ManifestHeader {
                producers,
                head_tree: head,
                ignore_fingerprint: None,
                segment: None,
            }),
            None,
        )))
    }

    fn reconciled(
        &self,
        access: &ViewAccess,
        base: Arc<ComposedGeneration>,
    ) -> Result<(Snapshot, u64), PlaneError> {
        Self::reconciled_snapshot(self.driver.as_ref(), access, base)
    }

    fn reconciled_snapshot(
        driver: &dyn FirstLoadDriver,
        access: &ViewAccess,
        base: Arc<ComposedGeneration>,
    ) -> Result<(Snapshot, u64), PlaneError> {
        for _ in 0..3 {
            // Read before the walk, so an ignore edit racing the walk shows up
            // as a difference from this record rather than being recorded as
            // the rules the walk applied.
            let ignore_files = match access {
                ViewAccess::Owner(owner) => {
                    Some(super::live_delta::root_ignore_files(owner.root()))
                }
                _ => None,
            };
            let checkout = driver.reconcile(access)?;
            if checkout.cut.is_none() && driver.revision(access) != checkout.revision {
                continue;
            }
            let mut delta = super::snapshot::LiveDelta::new(Arc::clone(&base));
            delta.bump_epoch();
            delta.record_root_ignore_files(ignore_files);
            // Full walker membership replaces seed membership, including ignored
            // sibling paths and files deleted in this checkout.
            for (path, _) in base.entries() {
                if !checkout.entries.contains_key(path) {
                    delta.apply(
                        path.clone(),
                        LiveEntry::new(super::snapshot::DiskState::Absent, checkout.revision),
                    );
                }
            }
            for (path, entry) in checkout.entries {
                if entry.reconciliation == super::readiness::ReconciledDelta::Pending {
                    delta.record_intent(path);
                } else {
                    delta.apply(path, entry);
                }
            }
            for (path, patch) in checkout.readiness {
                delta.set_readiness(path, patch);
            }
            let strict_epoch = checkout.cut.as_ref().map_or(delta.epoch(), |cut| cut.epoch);
            let mut snapshot = delta.snapshot().with_reconciled_epoch(strict_epoch);
            if let Some(base) = checkout.publication_base {
                snapshot = snapshot.with_publication_base(base);
            }
            if let Some(cut) = checkout.cut {
                snapshot = snapshot.with_install_cut(cut);
            }
            return Ok((snapshot, checkout.revision));
        }
        Err(Self::stage_error(
            LoaderStage::Reconciliation,
            CHECKOUT_CHANGED_DURING_RECONCILE,
        ))
    }

    fn open_planes(&self, access: &ViewAccess, snapshot: &Snapshot) -> Vec<PlaneError> {
        let mut pending = Vec::new();
        for adapter in &self.adapters {
            if let Err(error) = if snapshot.readers().is_none() {
                adapter.open_generation(access, snapshot.generation())
            } else {
                Ok(())
            } {
                pending.push(error);
                continue;
            }
            if adapter.readiness(access, snapshot)
                != (super::readiness::PlaneReadiness::Ready {
                    pending: 0,
                    failed: 0,
                })
            {
                pending.push(PlaneError {
                    plane: Some(adapter.plane()),
                    reason: "plane has unreflected checkout work".into(),
                });
            }
        }
        pending
    }

    /// Refresh an already installed owner without reopening sibling manifests.
    /// Membership is still strictly reconciled, including unreported edits.
    pub(crate) fn refresh(
        &self,
        access: &ViewAccess,
        base: Arc<ComposedGeneration>,
    ) -> Result<Snapshot, PlaneError> {
        let (snapshot, revision) = self.reconciled(access, base)?;
        self.driver.install(access, &snapshot, revision)?;
        let snapshot = self.driver.installed_after(&snapshot);
        self.fold(access, &snapshot, revision)
    }

    /// Fold plane completions from an installed cut. Ordinary delivered edits
    /// replay during installation; watcher or membership gaps invalidate its epoch.
    pub(crate) fn fold(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        revision: u64,
    ) -> Result<Snapshot, PlaneError> {
        self.driver.validate_cut(access, snapshot, revision)?;
        let own = self
            .driver
            .build_own_generation(access, snapshot, DERIVED_CALLGRAPH_SEEDING)?;
        own.ensure_producers(&self.driver.producers(access))
            .map_err(|error| {
                Self::stage_error(LoaderStage::ProducerValidation, error.to_string())
            })?;
        if snapshot.install_cut().is_none() {
            // Legacy drivers do not supply a replayable cut. They must obtain
            // new disk facts instead of treating a builder's claims as a walk.
            let (fresh, revision) = self.reconciled(access, own)?;
            self.driver.install(access, &fresh, revision)?;
            return Ok(self.driver.installed_after(&fresh));
        }
        let snapshot = {
            // Rebase the reconciled disk facts, not the new manifest's claims.
            // A builder can leave pending data without changing source bytes.
            let mut delta = LiveDelta::from_snapshot(snapshot);
            let draft = super::snapshot::derive_successor(
                &delta.cut(),
                own,
                &super::snapshot::carry_disk_state_only,
            );
            delta
                .replay_and_swap(draft, &super::snapshot::carry_disk_state_only)
                .map_err(|error| Self::error(format!("{error:?}")))?;
            let next = delta.snapshot();
            match snapshot.install_cut() {
                Some(cut) => next.with_install_cut(cut.clone()),
                None => next,
            }
        };
        self.driver.install(access, &snapshot, revision)?;
        Ok(self.driver.installed_after(&snapshot))
    }
}

impl super::contracts::PlaneLoader for SiblingLoader {
    fn load(&self, access: &ViewAccess) -> Result<super::contracts::LoadOutcome, PlaneError> {
        let seed = self.seed(access)?;
        if access.is_read_only() {
            let snapshot = super::snapshot::LiveDelta::new(seed).snapshot();
            let (readers, mut pending_planes) =
                PlaneReaders::open_available(access, snapshot.generation().clone(), &self.adapters);
            let snapshot = snapshot.with_readers(readers);
            for gap in self.open_planes(access, &snapshot) {
                if !pending_planes.iter().any(|error| error.plane == gap.plane) {
                    pending_planes.push(gap);
                }
            }
            return Ok(super::contracts::LoadOutcome {
                snapshot,
                pending_planes,
            });
        }
        let (snapshot, revision) = self.reconciled(access, seed)?;
        self.driver.install(access, &snapshot, revision)?;
        let snapshot = self.driver.installed_after(&snapshot);
        let snapshot = self.fold(access, &snapshot, revision)?;
        let pending_planes = self.open_planes(access, &snapshot);
        Ok(super::contracts::LoadOutcome {
            snapshot,
            pending_planes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::contracts::{PlaneLoader, QueryWait, WaitOutcome};
    use super::super::manifest_v2::{
        EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, PublishV2,
    };
    use super::super::registry::{FamilyRegistry, ViewRegistration};
    use super::super::snapshot::{DiskState, LiveDelta};
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    fn producers() -> Producers {
        Producers {
            trigram: "trigram".into(),
            semantic: None,
            callgraph: "extractor".into(),
        }
    }
    fn path(s: &str) -> RelPath {
        RelPath::new(s.as_bytes().to_vec()).unwrap()
    }
    fn manifest(files: &[(&str, &[u8])]) -> ManifestV2 {
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: producers(),
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        });
        for (name, bytes) in files {
            manifest
                .insert(
                    path(name),
                    EntryV2::regular(
                        crate::blob_store::v2::ContentHash::of(bytes),
                        bytes.len() as u64,
                        EntryPlanes::default(),
                    ),
                )
                .unwrap();
        }
        manifest
    }
    fn publish(owner: &ViewRegistration, manifest: &ManifestV2) {
        let store = owner.view_store().unwrap();
        let name = GenerationName::for_manifest(manifest).unwrap();
        let base = store.current_generation().unwrap();
        let prepared = store
            .prepare_v2(&name, base.as_deref(), manifest, None)
            .unwrap();
        assert_eq!(
            store.commit_v2(prepared, None).unwrap(),
            PublishV2::Published
        );
    }
    struct Driver {
        owner: ViewRegistration,
        revision: AtomicU64,
        walks: AtomicU64,
        installed: Mutex<Vec<Snapshot>>,
    }
    impl FirstLoadDriver for Driver {
        fn producers(&self, _: &ViewAccess) -> Producers {
            producers()
        }
        fn head_tree(&self, _: &ViewAccess) -> Option<String> {
            None
        }
        fn revision(&self, _: &ViewAccess) -> u64 {
            self.revision.load(Ordering::SeqCst)
        }
        fn reconcile(&self, _: &ViewAccess) -> Result<ReconciledCheckout, PlaneError> {
            let revision = self.revision.load(Ordering::SeqCst);
            let mut entries = BTreeMap::new();
            entries.insert(
                path("local.rs"),
                LiveEntry::new(
                    DiskState::of_bytes(if revision == 0 { b"before" } else { b"edited" }),
                    revision,
                ),
            );
            if self.walks.fetch_add(1, Ordering::SeqCst) == 0 {
                self.revision.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ReconciledCheckout {
                revision,
                entries,
                cut: None,
                readiness: Default::default(),
                publication_base: None,
            })
        }
        fn build_own_generation(
            &self,
            _: &ViewAccess,
            _: &Snapshot,
            seed_derived: bool,
        ) -> Result<Arc<ComposedGeneration>, PlaneError> {
            assert!(!seed_derived, "derived callgraph reuse must stay disabled");
            if std::env::var("AFT_FIRST_LOAD_PAUSE").as_deref() == Ok("before") {
                park_child(self.owner.registry().storage());
            }
            publish(&self.owner, &manifest(&[("local.rs", b"edited")]));
            self.revision.fetch_add(1, Ordering::SeqCst);
            let reader = self
                .owner
                .registry()
                .register_reader("test-builder")
                .unwrap();
            Ok(super::super::read::open_foreign_generation(
                &reader,
                self.owner.scope(),
                &producers(),
            )
            .unwrap()
            .unwrap())
        }
        fn install(
            &self,
            access: &ViewAccess,
            snapshot: &Snapshot,
            revision: u64,
        ) -> Result<(), PlaneError> {
            assert_eq!(revision, self.revision(access));
            self.installed.lock().unwrap().push(snapshot.clone());
            Ok(())
        }
    }

    #[test]
    fn seed_failure_names_the_loader_not_the_trigram_plane() {
        let temp = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(temp.path(), "owner", temp.path())
            .unwrap();
        let driver = Arc::new(Driver {
            owner,
            revision: AtomicU64::new(0),
            walks: AtomicU64::new(0),
            installed: Mutex::new(Vec::new()),
        });
        let reader = registry.register_reader("missing-seed").unwrap();
        let error = SiblingLoader::new(driver, vec![])
            .load(&ViewAccess::Reader {
                registration: Arc::new(reader),
                scope: "missing".into(),
            })
            .unwrap_err();
        assert!(error
            .to_string()
            .starts_with("view loader [seed selection]:"));
        assert!(!error.to_string().contains("trigram plane:"));
        assert_eq!(error.plane, None);
    }

    #[test]
    fn loader_retries_edits_and_removes_sibling_members_before_serving() {
        let temp = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
        let sibling = registry
            .register_view_with_storage(registry.storage(), "sibling", temp.path())
            .unwrap();
        publish(&sibling, &manifest(&[("excluded.rs", b"foreign")]));
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", temp.path())
            .unwrap();
        let driver = Arc::new(Driver {
            owner: owner.clone(),
            revision: AtomicU64::new(0),
            walks: AtomicU64::new(0),
            installed: Mutex::new(Vec::new()),
        });
        let outcome = SiblingLoader::new(driver.clone(), vec![])
            .load(&ViewAccess::Owner(owner))
            .unwrap();
        assert!(driver.walks.load(Ordering::SeqCst) >= 3);
        let installed = driver.installed.lock().unwrap();
        assert_eq!(installed.len(), 2);
        for snapshot in installed.iter() {
            assert_eq!(snapshot.disk_state(&path("excluded.rs")), DiskState::Absent);
            assert_eq!(
                snapshot.disk_state(&path("local.rs")),
                DiskState::of_bytes(b"edited")
            );
        }
        assert_eq!(outcome.snapshot.membership(), installed[0].membership());
        assert!(outcome.snapshot.generation().name().starts_with("g2"));
    }

    #[test]
    fn foreign_pointer_reads_do_not_change_database_bytes_or_journal_mode() {
        let temp = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "foreign", temp.path())
            .unwrap();
        publish(&owner, &manifest(&[("file.rs", b"hello")]));
        let store = owner.view_store().unwrap();
        // Publication maintenance is not part of the read-only operation audited below.
        crate::views::generation::wait_for_generation_sweep_for_test(store.view_dir());
        // Close each SQLite handle before reading its file: no live WAL file
        // is opened by a second descriptor in this audit.
        {
            let connection = crate::db::file_identity::IdentityConnection::open(
                store.pointer_path(),
                "first-load audit fixture",
            )
            .unwrap();
            connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                .unwrap();
        }
        let before = std::fs::read(store.pointer_path()).unwrap();
        let inventory_before = inventory(temp.path());
        let epoch_before = registry.gc_epoch().unwrap();
        let members_before = format!("{:?}", registry.members().unwrap());
        let reader = registry.register_reader("foreign-audit").unwrap();
        let generation =
            super::super::read::open_foreign_generation(&reader, "foreign", &producers())
                .unwrap()
                .unwrap();
        assert_eq!(generation.len(), 1);
        assert_eq!(std::fs::read(store.pointer_path()).unwrap(), before);
        assert_eq!(inventory(temp.path()), inventory_before);
        assert_eq!(registry.gc_epoch().unwrap(), epoch_before);
        assert_eq!(format!("{:?}", registry.members().unwrap()), members_before);
        assert!(!store.pointer_path().with_extension("sqlite-wal").exists());
        drop(generation);
    }

    fn inventory(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(
            root: &std::path::Path,
            dir: &std::path::Path,
            out: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Reader rows and their SQLite journals are markers; all other
                // durable bytes, including pointer sidecars, are audited.
                if name == "readers" || name.starts_with("members.sqlite") {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &entry.path(), out);
                } else {
                    out.insert(
                        entry.path().strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(entry.path()).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        visit(root, root, &mut out);
        out
    }

    #[test]
    fn seed_pin_survives_sibling_switch_and_family_sweep() {
        let temp = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
        let sibling = registry
            .register_view_with_storage(registry.storage(), "sibling", temp.path())
            .unwrap();
        publish(&sibling, &manifest(&[("old.rs", b"old")]));
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", temp.path())
            .unwrap();
        let driver = Arc::new(Driver {
            owner: owner.clone(),
            revision: AtomicU64::new(0),
            walks: AtomicU64::new(0),
            installed: Mutex::new(Vec::new()),
        });
        let loader = SiblingLoader::new(driver, vec![]);
        let seed = loader.seed(&ViewAccess::Owner(owner)).unwrap();
        let marker_dir = crate::root_cache::read_marker_dir(sibling.view_dir(), seed.name());
        assert!(
            std::fs::read_dir(&marker_dir).unwrap().next().is_some(),
            "seed lost its residency marker"
        );
        let old_path = sibling
            .view_store()
            .unwrap()
            .manifest_path(seed.name())
            .unwrap();
        publish(&sibling, &manifest(&[("new.rs", b"new")]));
        for _ in 0..2 {
            crate::gc::family::sweep_family(
                &registry,
                None,
                crate::gc::family::FamilySweepPolicy { byte_budget: 0 },
                None,
            )
            .unwrap();
        }
        assert!(old_path.is_file());
        assert_eq!(
            seed.disk_state(&path("old.rs")),
            DiskState::of_bytes(b"old")
        );
    }

    fn park_child(storage: &std::path::Path) {
        std::fs::write(storage.join("child-ready"), b"ready").unwrap();
        while !storage.join("child-go").exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore]
    fn first_load_child() {
        let storage = std::path::PathBuf::from(std::env::var_os("AFT_FIRST_LOAD_STORAGE").unwrap());
        let registry = FamilyRegistry::open(&storage, "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", &storage)
            .unwrap();
        let driver = Arc::new(Driver {
            owner: owner.clone(),
            revision: AtomicU64::new(0),
            walks: AtomicU64::new(0),
            installed: Mutex::new(Vec::new()),
        });
        let outcome = SiblingLoader::new(driver, vec![])
            .load(&ViewAccess::Owner(owner))
            .unwrap();
        assert_eq!(
            outcome.snapshot.disk_state(&path("local.rs")),
            DiskState::of_bytes(b"edited")
        );
        assert_eq!(
            outcome.snapshot.disk_state(&path("excluded.rs")),
            DiskState::Absent
        );
        if std::env::var("AFT_FIRST_LOAD_PAUSE").as_deref() == Ok("after") {
            park_child(&storage);
        }
    }

    #[test]
    fn restart_and_kill_before_and_after_own_install_reconcile_safely() {
        for phase in ["before", "after"] {
            for kill in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
                let sibling = registry
                    .register_view_with_storage(registry.storage(), "sibling", temp.path())
                    .unwrap();
                publish(&sibling, &manifest(&[("excluded.rs", b"foreign")]));
                let executable = std::env::current_exe().unwrap();
                let spawn = |pause: &str| {
                    std::process::Command::new(&executable)
                        .args([
                            "--ignored",
                            "--exact",
                            "views::first_load::tests::first_load_child",
                        ])
                        .env("AFT_FIRST_LOAD_STORAGE", temp.path())
                        .env("AFT_FIRST_LOAD_PAUSE", pause)
                        .stdout(std::process::Stdio::null())
                        .spawn()
                        .unwrap()
                };
                let mut child = spawn(phase);
                let started = std::time::Instant::now();
                while !temp.path().join("child-ready").exists() {
                    assert!(
                        started.elapsed() < Duration::from_secs(15),
                        "child did not reach {phase}"
                    );
                    assert!(
                        child.try_wait().unwrap().is_none(),
                        "child exited before pause"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                if kill {
                    child.kill().unwrap();
                    assert!(!child.wait().unwrap().success());
                } else {
                    std::fs::write(temp.path().join("child-go"), b"go").unwrap();
                    assert!(child.wait().unwrap().success());
                }
                let mut reopened = spawn("none");
                assert!(
                    reopened.wait().unwrap().success(),
                    "reopen failed after {phase}, kill={kill}"
                );
                let reader = registry.register_reader("restart-verifier").unwrap();
                let own =
                    super::super::read::open_foreign_generation(&reader, "local", &producers())
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    own.disk_state(&path("local.rs")),
                    DiskState::of_bytes(b"edited")
                );
                assert_eq!(own.disk_state(&path("excluded.rs")), DiskState::Absent);
            }
        }
    }

    struct State {
        polls: AtomicU64,
        snapshot: Snapshot,
        ready_at: u64,
    }
    impl QueryState for State {
        fn installed_state(
            &self,
            _: &ViewAccess,
            _: FamilyPlane,
        ) -> (Snapshot, Vec<std::path::PathBuf>) {
            let poll = self.polls.fetch_add(1, Ordering::SeqCst);
            let mut delta = LiveDelta::new(Arc::clone(self.snapshot.generation()));
            delta.apply(
                path("changing.rs"),
                LiveEntry::new(DiskState::of_bytes(&poll.to_le_bytes()), poll),
            );
            (
                delta.snapshot(),
                if poll >= self.ready_at {
                    vec![]
                } else {
                    vec![format!("unreflected-{poll}.rs").into()]
                },
            )
        }
    }
    #[test]
    fn query_wait_uses_installed_data_and_one_deadline_despite_edits() {
        let temp = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", temp.path())
            .unwrap();
        let access = ViewAccess::Owner(owner);
        let snapshot = LiveDelta::new(Arc::new(ComposedGeneration::new(
            "test",
            manifest(&[]),
            None,
        )))
        .snapshot();
        let state = Arc::new(State {
            polls: AtomicU64::new(0),
            snapshot: snapshot.clone(),
            ready_at: 2,
        });
        let wait = super::super::query_wait::BoundedQueryWait::new(state.clone());
        match wait.wait_for(&access, FamilyPlane::Callgraph, Duration::from_secs(1)) {
            WaitOutcome::Installed(snapshot) => assert_eq!(
                snapshot.disk_state(&path("changing.rs")),
                DiskState::of_bytes(&2u64.to_le_bytes())
            ),
            _ => panic!("installed data did not become ready"),
        }
        let state = Arc::new(State {
            polls: AtomicU64::new(0),
            snapshot,
            ready_at: u64::MAX,
        });
        let wait = super::super::query_wait::BoundedQueryWait::new(state.clone());
        let started = std::time::Instant::now();
        match wait.wait_for(&access, FamilyPlane::Callgraph, Duration::from_millis(25)) {
            WaitOutcome::TimedOut {
                snapshot,
                unreflected,
            } => {
                let poll = state.polls.load(Ordering::SeqCst) - 1;
                assert_eq!(
                    unreflected,
                    vec![std::path::PathBuf::from(format!("unreflected-{poll}.rs"))]
                );
                assert_eq!(
                    snapshot.disk_state(&path("changing.rs")),
                    DiskState::of_bytes(&poll.to_le_bytes())
                );
            }
            _ => panic!("unreflected data incorrectly reported installed"),
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

/// Runtime bridge around a plane's byte extractor and v2 materializer. The
/// runtime calls these only for its own registered checkout, never a foreign reader.
pub trait CompositePlane: Send + Sync {
    fn plane(&self) -> FamilyPlane;
    fn applies_to(&self, path: &RelPath) -> bool;
    fn bootstrap_policy(&self) -> Option<crate::blob_store::v2::TrigramPolicy> {
        None
    }

    /// Whether this plane can complete its entries of a freshly built v3
    /// base itself (see [`Self::complete_v3_base`]). Fresh bootstrap refuses a
    /// nonlexical plane that cannot, rather than strand its base entries.
    fn completes_fresh_base(&self) -> bool {
        false
    }

    /// Completes this plane's pending entries of the claimed base from the
    /// attachments the bootstrap's strict reconciliation computed for the
    /// same bytes, and publishes the result as a readiness successor.
    fn complete_v3_base(
        &self,
        claim: &super::base_builder::BuildClaim,
        observed: &BTreeMap<RelPath, LiveEntry>,
    ) -> Result<(), PlaneError> {
        let _ = (claim, observed);
        Ok(())
    }

    /// Derive a payload key without extraction. Planes with deferred extraction
    /// may leave this absent and keep their own cheap attachment format.
    fn key(
        &self,
        _path: &RelPath,
        _content: crate::blob_store::v2::ContentHash,
    ) -> Option<crate::blob_store::v2::FamilyKey> {
        None
    }
    fn cached_attachment(
        &self,
        _content: crate::blob_store::v2::ContentHash,
        key: crate::blob_store::v2::FamilyKey,
    ) -> super::snapshot::PlaneAttachment {
        Arc::new(CachedAttachment(key))
    }
    fn attachment_key(
        &self,
        _attachment: &super::snapshot::PlaneAttachment,
    ) -> Option<crate::blob_store::v2::FamilyKey> {
        None
    }

    /// Called during strict reconciliation with the very bytes that were hashed.
    /// May compute extraction data, but must not write artifacts or read source.
    fn attachment(
        &self,
        path: &RelPath,
        bytes: &[u8],
    ) -> Result<super::snapshot::PlaneAttachment, PlaneError>;

    /// Called before owner publication, while an assembly protects `generation`.
    /// May block and write owner artifacts. Protect every new key/segment in
    /// `live` before putting or touching it; update manifest readiness honestly.
    /// Reuse compatible immutable seed keys, but never read checkout source here.
    fn materialize(
        &self,
        owner: &super::registry::ViewRegistration,
        generation: &str,
        snapshot: &Snapshot,
        observed: &BTreeMap<RelPath, LiveEntry>,
        manifest: &mut super::manifest_v2::ManifestV2,
        live: &mut crate::pins::LivePin,
        seed_derived: bool,
    ) -> Result<(), PlaneError>;

    /// Called with the final immutable manifest name before pointer publication.
    /// Rename/sync any privately built derived artifacts to that name. Must not
    /// publish pointers, read source, or repair/write another checkout's state.
    fn finish_generation(
        &self,
        owner: &super::registry::ViewRegistration,
        staging: &str,
        published: &str,
    ) -> Result<(), PlaneError>;

    /// Materializes this plane for a v3 worker generation whose `delta`
    /// composes over `base`: rows go to the family's v3 plane stores and
    /// derived files to `route.member_dir()` under `staging`. Every new key
    /// is listed in `live`, the owner's v3 live pin, before it is put or
    /// touched. Only the delta's entries are this checkout's work; inherited
    /// base entries belong to the base's builder.
    #[allow(clippy::too_many_arguments)]
    fn materialize_v3(
        &self,
        owner: &super::registry::ViewRegistration,
        route: &super::snapshot::V3Route,
        staging: &str,
        base: &Arc<ComposedGeneration>,
        observed: &BTreeMap<RelPath, LiveEntry>,
        delta: &mut super::manifest_v2::ManifestV2,
        live: &mut crate::pins::LivePin,
    ) -> Result<(), PlaneError> {
        // A plane without v3 storage leaves its delta entries as they are
        // (pending), which keeps them queued and honestly incomplete.
        let _ = (owner, route, staging, base, observed, delta, live);
        Ok(())
    }

    /// [`Self::finish_generation`] for a v3 member directory.
    fn finish_generation_v3(
        &self,
        route: &super::snapshot::V3Route,
        staging: &str,
        published: &str,
    ) -> Result<(), PlaneError> {
        let _ = (route, staging, published);
        Ok(())
    }
}

/// Supplies the same full membership as the configured lexical walker. Runtime
/// adapters must return walker errors rather than silently truncating membership.
pub trait MembershipWalker: Send + Sync {
    fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError>;
    fn changed_files(
        &self,
        _root: &std::path::Path,
        _candidates: &std::collections::BTreeSet<std::path::PathBuf>,
    ) -> Result<Option<Vec<std::path::PathBuf>>, PlaneError> {
        Ok(None)
    }
}

#[derive(Clone)]
/// One installed checkout authority. Cloned query handles keep the composed
/// generation, journal cut, coverage and resident readers from the same install.
pub struct InstalledBundle {
    pub seq: u64,
    pub composed: Arc<ComposedGeneration>,
    pub coverage: BTreeMap<FamilyPlane, super::readiness::PlaneReadiness>,
    pub readers: Arc<PlaneReaders>,
    pub published: super::manifest_v2::PublishedFrontier,
    revision: u64,
    snapshot: Snapshot,
    uncertain_paths: std::collections::BTreeSet<std::path::PathBuf>,
    own_installed: bool,
}

/// Evidence that a previous source read still describes this file. V3 stores
/// stable observations with their published generation. Missing change-time
/// or file-identity support means reading bytes instead of trusting mtime alone.
#[derive(Clone)]
struct ObservedStat {
    stamp: crate::watcher::FileStamp,
    #[cfg(unix)]
    device: u64,
    recorded_ns: u128,
    delivery_seq: u64,
    native: Option<super::verification::NativeStat>,
}

/// Publish stat evidence together with the extracted content it validates.
/// Concurrent strict loads must not pair one walk's stamps with another
/// walk's entries, even when an obsolete walk loses the installation fence.
#[derive(Clone, Default)]
struct ObservedCheckout {
    entries: BTreeMap<RelPath, LiveEntry>,
    stats: BTreeMap<RelPath, ObservedStat>,
    evidence: BTreeMap<RelPath, super::verification::Evidence>,
    guard: Option<Arc<super::verification::BindGuard>>,
    subscription: Option<crate::watcher_backend::subscription::Continuity>,
}

impl ObservedStat {
    fn new(metadata: &std::fs::Metadata, recorded_ns: u128) -> Self {
        Self {
            stamp: crate::watcher::FileStamp::from_metadata(metadata),
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            recorded_ns,
            delivery_seq: 0,
            native: None,
        }
    }

    fn reusable(&self, current: &Self) -> bool {
        if let (Some(old), Some(current)) = (&self.native, &current.native) {
            return old.reusable(current, self.recorded_ns);
        }
        #[cfg(unix)]
        {
            self.stamp == current.stamp && self.device == current.device
            // Equal timestamps can hide a write made within the filesystem's
            // timestamp granularity. Do not move the observation time on hits.
            && self.stamp.modified_ns.is_some_and(|mtime| mtime < self.recorded_ns)
        }
        #[cfg(not(unix))]
        {
            let _ = (current, self.recorded_ns);
            false
        }
    }
}

#[cfg(all(test, unix))]
mod stat_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn driver(root: &std::path::Path, storage: &std::path::Path) -> (CheckoutDriver, ViewAccess) {
        let registry = super::super::registry::FamilyRegistry::open(storage, "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "stat", root)
            .unwrap();
        let driver = CheckoutDriver::new(
            owner.clone(),
            Producers {
                trigram: "unused".into(),
                semantic: None,
                callgraph: "unused".into(),
            },
            None,
            Arc::new(ConfiguredMembershipWalker),
            vec![],
        );
        (driver, ViewAccess::Owner(owner))
    }

    fn old_mtime(path: &std::path::Path) {
        std::fs::File::open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(60)))
            .unwrap();
    }

    #[test]
    fn strict_reconcile_without_a_frontier_verifies_membership() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), b"fn fresh() {}\n").unwrap();
        let (driver, access) = driver(root.path(), storage.path());
        let base = driver.installed_snapshot().generation().clone();
        let (snapshot, _) = SiblingLoader::reconciled_snapshot(&driver, &access, base).unwrap();
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        assert_eq!(
            snapshot.disk_state(&path),
            super::super::snapshot::DiskState::of_bytes(b"fn fresh() {}\n")
        );
        assert_eq!(driver.take_reconcile_work().reads, 1);
        assert!(snapshot.install_cut().is_some());
    }

    #[test]
    fn unstable_strict_reads_retry_three_times_and_name_pending_without_bytes() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), b"initial").unwrap();
        let (driver, access) = driver(root.path(), storage.path());
        *driver.read_hook.lock().unwrap() = Some(Box::new(|file, attempt| {
            std::fs::write(file, vec![b'x'; 20 + attempt]).unwrap();
        }));
        let base = driver.installed_snapshot().generation().clone();
        let (snapshot, _) = SiblingLoader::reconciled_snapshot(&driver, &access, base).unwrap();
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        assert_eq!(snapshot.pending_intent().collect::<Vec<_>>(), vec![&path]);
        assert_eq!(
            snapshot.disk_state(&path),
            super::super::snapshot::DiskState::Absent
        );
        assert_eq!(driver.take_reconcile_work().reads, 3);
        assert!(driver.observed.lock().unwrap().stats.is_empty());
    }

    #[test]
    fn event_during_exception_read_invalidates_even_unchanged_stats() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), b"unchanged").unwrap();
        let (driver, access) = driver(root.path(), storage.path());
        let driver = Arc::new(driver);
        let weak = Arc::downgrade(&driver);
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        let changed = path.clone();
        *driver.read_hook.lock().unwrap() = Some(Box::new(move |_, attempt| {
            if attempt == 0 {
                weak.upgrade().unwrap().record_change(changed.clone());
            }
        }));
        driver.reconcile(&access).unwrap();
        assert_eq!(driver.take_reconcile_work().reads, 2);
        assert_eq!(driver.observed.lock().unwrap().stats[&path].delivery_seq, 1);
    }

    #[test]
    fn overflow_updates_the_canonical_epoch_and_preserves_bundle_readers() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, access) = driver(root.path(), storage.path());
        let before = driver.installed_snapshot();
        driver.record_watcher_overflow();
        let after = driver.installed_snapshot();
        assert_eq!(
            after.watcher(),
            super::super::snapshot::WatcherState::Overflowed
        );
        assert!(after.install_cut().unwrap().epoch > before.install_cut().unwrap().epoch);
        assert!(Arc::ptr_eq(
            after.readers().unwrap(),
            before.readers().unwrap()
        ));
        assert!(driver.validate_cut(&access, &before, 0).is_err());
    }

    #[test]
    fn stat_reconcile_detects_unreported_same_size_backdated_edit() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = root.path().join("file.rs");
        std::fs::write(&file, b"before").unwrap();
        old_mtime(&file);
        let (driver, access) = driver(root.path(), storage.path());
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        driver.reconcile(&access).unwrap();
        driver.take_reconcile_work();
        driver.reconcile(&access).unwrap();
        assert_eq!(driver.take_reconcile_work().reads, 0);
        let before = driver.observed.lock().unwrap().stats[&path].clone();
        std::fs::write(&file, b"edited").unwrap();
        old_mtime(&file);
        let current = ObservedStat::new(&std::fs::metadata(&file).unwrap(), before.recorded_ns);
        assert_eq!(before.stamp.size, current.stamp.size);
        assert_eq!(before.stamp.modified_ns, current.stamp.modified_ns);
        assert_eq!(before.stamp.inode, current.stamp.inode);
        assert_ne!(
            before.stamp.ctime_ns, current.stamp.ctime_ns,
            "fixture must move ctime"
        );
        // No event or intent is delivered: the changed ctime alone must force
        // a source read despite an identical size, inode and backdated mtime.
        let checkout = driver.reconcile(&access).unwrap();
        let work = driver.take_reconcile_work();
        eprintln!("unreported backdated edit: {work:?}");
        assert_eq!(
            checkout.entries[&path].disk,
            super::super::snapshot::DiskState::of_bytes(b"edited")
        );
        assert_eq!(work.reads, 1);
        assert_eq!(work.content_hash_bytes, 6);
    }

    #[test]
    fn stat_reconcile_reads_racily_clean_file_even_when_stamp_matches() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = root.path().join("file.rs");
        std::fs::write(&file, b"before").unwrap();
        old_mtime(&file);
        let (driver, access) = driver(root.path(), storage.path());
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        driver.reconcile(&access).unwrap();
        driver.take_reconcile_work();
        // Inject the timestamp boundary instead of waiting for the host clock
        // or depending on this filesystem's timestamp resolution.
        {
            let mut observed = driver.observed.lock().unwrap();
            let observed = observed.stats.get_mut(&path).unwrap();
            observed.recorded_ns = observed.stamp.modified_ns.unwrap();
        }
        let checkout = driver.reconcile(&access).unwrap();
        let work = driver.take_reconcile_work();
        eprintln!("racily clean: {work:?}");
        assert_eq!(
            checkout.entries[&path].disk,
            super::super::snapshot::DiskState::of_bytes(b"before")
        );
        assert_eq!(
            work.reads, 1,
            "an equal stat in the observation's timestamp window is not proof of unchanged bytes"
        );
        assert_eq!(work.content_hash_bytes, 6);
    }
}

/// Composite checkout driver shared by the loader and query-wait router.
/// Watchers and first-party mutation paths call `record_change` before acknowledging
/// edits. Real planes supply extraction/materialization bridges at construction.
pub struct CheckoutDriver {
    owner: super::registry::ViewRegistration,
    producers: Producers,
    head_tree: Option<String>,
    walker: Arc<dyn MembershipWalker>,
    planes: Vec<Arc<dyn CompositePlane>>,
    adapters: Vec<Arc<dyn super::contracts::PlaneAdapter>>,
    publish_fence: std::sync::RwLock<Option<Arc<dyn PublishFence>>>,
    observed: std::sync::Mutex<ObservedCheckout>,
    installed: std::sync::Mutex<Arc<InstalledBundle>>,
    live: std::sync::Mutex<LiveDelta>,
    prepared_readers: std::sync::Mutex<BTreeMap<String, PreparedReaders>>,
    // Serialize the final identity check/CAS with the installed sequence swap,
    // without holding query locks during I/O or plane-reader retirement.
    publication_transition: std::sync::Mutex<()>,
    retry: std::sync::atomic::AtomicBool,
    watcher_cookies: crate::watcher_backend::cookie::CookieBarrier,
    delivery: std::sync::Mutex<DeliveryState>,
    v3_reconcile: std::sync::atomic::AtomicBool,
    advisory_token: std::sync::Mutex<Option<String>>,
    base_claim: std::sync::Mutex<Option<super::base_builder::BuildClaim>>,
    #[cfg(test)]
    read_hook: std::sync::Mutex<Option<ReadHook>>,
    #[cfg(test)]
    work: std::sync::Mutex<ReconcileWork>,
}

#[derive(Default)]
struct DeliveryState {
    seq: u64,
    paths: BTreeMap<RelPath, u64>,
}

#[cfg(test)]
type ReadHook = Box<dyn Fn(&std::path::Path, usize) + Send>;

enum StableRead {
    Absent,
    Bytes(Vec<u8>, ObservedStat),
    Pending,
}

#[derive(Clone)]
struct CachedAttachment(crate::blob_store::v2::FamilyKey);

fn v3_contains(route: &super::snapshot::V3Route, key: &crate::blob_store::v2::FamilyKey) -> bool {
    crate::blob_store::v3::PlaneReader::open_existing(&route.storage, &route.family, key.plane())
        .and_then(|reader| reader.contains(key))
        .unwrap_or(false)
}

struct PreparedReaders {
    seq: u64,
    readers: Arc<PlaneReaders>,
}

/// Source work only; publication and model counters belong to their stores.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ReconcileWork {
    pub walks: usize,
    pub stats: usize,
    pub reads: usize,
    pub content_hash_bytes: usize,
    pub attachments: usize,
}

/// Why this driver cannot own and complete a fresh base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FreshBootstrapRefusal {
    NonLexicalPlane(FamilyPlane),
    MissingLexicalPlane,
    LexicalProducerMismatch,
}

impl std::fmt::Display for FreshBootstrapRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonLexicalPlane(plane) => write!(
                f,
                "fresh_bootstrap_nonlexical_plane: {} requires its base completion coordinator",
                plane.as_str()
            ),
            Self::MissingLexicalPlane => f.write_str("fresh_bootstrap_missing_lexical_plane"),
            Self::LexicalProducerMismatch => {
                f.write_str("fresh_bootstrap_lexical_producer_mismatch")
            }
        }
    }
}
impl std::error::Error for FreshBootstrapRefusal {}

impl CheckoutDriver {
    /// Pure eligibility check for callers choosing between new and existing
    /// runtime paths. The bind hook reports the same refusal through PlaneError.
    pub fn fresh_bootstrap_policy(
        &self,
    ) -> Result<crate::blob_store::v2::TrigramPolicy, FreshBootstrapRefusal> {
        if let Some(plane) = self
            .planes
            .iter()
            .find(|plane| plane.plane() != FamilyPlane::Trigram && !plane.completes_fresh_base())
            .map(|plane| plane.plane())
        {
            return Err(FreshBootstrapRefusal::NonLexicalPlane(plane));
        }
        if self.producers.semantic.is_some() {
            return Err(FreshBootstrapRefusal::NonLexicalPlane(
                FamilyPlane::Semantic,
            ));
        }
        // A registered call-graph producer needs a registered plane that
        // completes the base's graph entries.
        if self.producers.callgraph != super::semantic_runtime::UNREGISTERED_PRODUCER
            && !self.planes.iter().any(|plane| {
                plane.plane() == FamilyPlane::Callgraph && plane.completes_fresh_base()
            })
        {
            return Err(FreshBootstrapRefusal::NonLexicalPlane(
                FamilyPlane::Callgraph,
            ));
        }
        let policy = self
            .planes
            .iter()
            .find_map(|plane| plane.bootstrap_policy())
            .ok_or(FreshBootstrapRefusal::MissingLexicalPlane)?;
        if self.producers.trigram != policy.fingerprint_hex() {
            return Err(FreshBootstrapRefusal::LexicalProducerMismatch);
        }
        Ok(policy)
    }
    fn stable_read(
        &self,
        path: &RelPath,
        absolute: &std::path::Path,
        recorded_ns: u128,
        first_seq: u64,
        first_stat: Option<ObservedStat>,
    ) -> Result<StableRead, PlaneError> {
        for attempt in 0..3 {
            let seq = if attempt == 0 {
                first_seq
            } else {
                self.delivery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .seq
            };
            let before = if attempt == 0 {
                first_stat.clone()
            } else {
                self.source_stat(absolute, recorded_ns)?
            };
            let bytes = super::assembly::read_working_tree_file(absolute)
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
            #[cfg(test)]
            {
                if let Some(bytes) = &bytes {
                    let mut work = self.work.lock().unwrap();
                    work.reads += 1;
                    let _ = bytes;
                }
                if let Some(hook) = &*self.read_hook.lock().unwrap() {
                    hook(absolute, attempt);
                }
            }
            let after = self.source_stat(absolute, recorded_ns)?;
            let delivery = self
                .delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let unchanged = delivery.paths.get(path).copied().unwrap_or(0) <= seq;
            drop(delivery);
            match (before, after, bytes) {
                (None, None, None) if unchanged => return Ok(StableRead::Absent),
                (Some(mut before), Some(after), Some(bytes)) if unchanged => {
                    let stable = before.stamp == after.stamp && before.native == after.native;
                    #[cfg(unix)]
                    let stable = stable && before.device == after.device;
                    if stable {
                        before.delivery_seq = seq;
                        return Ok(StableRead::Bytes(bytes, before));
                    }
                }
                _ => {}
            }
            // A changing file must not contribute bytes to a ready generation.
            // Back off between bounded attempts; tests inject changes directly.
            #[cfg(not(test))]
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(1 << attempt));
            }
        }
        Ok(StableRead::Pending)
    }

    /// Verifies this checkout against `base` and returns a reconciliation cut.
    /// Unsupported freshness proofs use strict reconciliation instead.
    /// A fresh checkout bootstraps a lexical-ready v3 base and publishes a v3
    /// worker over it; it never falls back to legacy storage. Installation is
    /// left to the caller and still validates the cut's epoch and bundle identity.
    /// Fresh bases are lexical-only until nonlexical completion can be scheduled;
    /// already-built bases can be verified for any registered plane.
    pub fn verified_bind(
        &self,
        access: &ViewAccess,
        base: Arc<ComposedGeneration>,
    ) -> Result<(Snapshot, u64), PlaneError> {
        self.check_owner(access)?;
        if !self.owner.registry().is_v3_only() {
            return Err(SiblingLoader::error(
                "verified_bind requires a v3-only registration",
            ));
        }
        let bootstrap = base.base_header().is_none();
        if bootstrap {
            self.fresh_bootstrap_policy()
                .map_err(|reason| SiblingLoader::error(reason.to_string()))?;
        } else {
            base.ensure_producers(&self.producers).map_err(|error| {
                SiblingLoader::stage_error(LoaderStage::ProducerValidation, error.to_string())
            })?;
        }
        if let Some(token) = self
            .advisory_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let route = self.owner.v3_route();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            // Token rejection is advisory; every outcome follows the same
            // verification path. Tree-base warming is not enabled yet.
            let _ = super::registry::consume_view_token(
                &route.storage,
                &route.family,
                &token,
                self.owner.root(),
                now,
            );
        }
        self.v3_reconcile
            .store(true, std::sync::atomic::Ordering::Release);
        let base = if bootstrap {
            self.bootstrap_v3_base(access, base)?
        } else {
            base
        };
        let (snapshot, revision) = SiblingLoader::reconciled_snapshot(self, access, base)?;
        if !bootstrap {
            return Ok((snapshot, revision));
        }
        let own = self.build_own_generation(access, &snapshot, DERIVED_CALLGRAPH_SEEDING);
        // Every plane of a fresh bootstrap has completed its base entries by
        // now (lexical in the build, others in `bootstrap_v3_base`).
        if self
            .planes
            .iter()
            .all(|plane| plane.plane() == FamilyPlane::Trigram || plane.completes_fresh_base())
        {
            if let Some(claim) = self
                .base_claim
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                claim
                    .release()
                    .map_err(|e| SiblingLoader::error(e.to_string()))?;
            }
        }
        let own = own?;
        let mut delta = LiveDelta::from_snapshot(&snapshot);
        let draft = super::snapshot::derive_successor(
            &delta.cut(),
            own,
            &super::snapshot::carry_disk_state_only,
        );
        delta
            .replay_and_swap(draft, &super::snapshot::carry_disk_state_only)
            .map_err(|error| SiblingLoader::error(format!("{error:?}")))?;
        Ok((
            delta
                .snapshot()
                .with_install_cut(snapshot.install_cut().unwrap().clone())
                .with_reconciled_epoch(
                    snapshot
                        .reconciled_epoch()
                        .unwrap_or(snapshot.install_cut().unwrap().epoch),
                ),
            revision,
        ))
    }

    fn bootstrap_v3_base(
        &self,
        access: &ViewAccess,
        seed: Arc<ComposedGeneration>,
    ) -> Result<Arc<ComposedGeneration>, PlaneError> {
        let policy = self
            .fresh_bootstrap_policy()
            .map_err(|reason| SiblingLoader::error(reason.to_string()))?;
        let (checkout, _) = SiblingLoader::reconciled_snapshot(self, access, seed)?;
        if checkout.pending_intent().next().is_some() {
            return Err(SiblingLoader::error(
                "v3 bootstrap has pending unstable paths",
            ));
        }
        let route = self.owner.v3_route();
        let ignore_fingerprint = checkout
            .install_cut()
            .and_then(|cut| cut.verification.as_ref())
            .map(|g| g.policy.digest.clone());
        let header = super::manifest_v2::ManifestHeader {
            producers: self.producers.clone(),
            head_tree: None,
            ignore_fingerprint: ignore_fingerprint.clone(),
            segment: None,
        };
        let mut manifest = super::manifest_v2::ManifestV2::new_v3(header, None, Default::default());
        for (path, state) in checkout.membership() {
            if let super::snapshot::DiskState::Present { content, size } = state {
                manifest
                    .insert(
                        path.clone(),
                        super::base_builder::base_entry(&path, content, size, Default::default()),
                    )
                    .map_err(|e| SiblingLoader::error(e.to_string()))?;
            }
        }
        let expected = super::base_refs::BaseIdentity::of(&route.family, &manifest)
            .map_err(|e| SiblingLoader::error(e.to_string()))?;
        let resolved =
            match super::base_refs::resolve_base(&route.storage, &route.family, &expected.scope) {
                Ok(base) => base,
                Err(_) => {
                    let paths: Vec<_> = manifest.entries().map(|(path, _)| path.clone()).collect();
                    match super::base_builder::build_base(
                        super::base_builder::BaseBuildInput {
                            storage: &route.storage,
                            family: &route.family,
                            root: self.owner.root(),
                            paths: &paths,
                            policy,
                            semantic_producer: self.producers.semantic.clone(),
                            callgraph_producer: self.producers.callgraph.clone(),
                            ignore_fingerprint,
                            history: None,
                            budget: Default::default(),
                        },
                        None,
                    )
                    .map_err(|e| SiblingLoader::error(e.to_string()))?
                    {
                        super::base_builder::BaseBuild::Published {
                            identity, claim, ..
                        } => {
                            *self
                                .base_claim
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(claim);
                            super::base_refs::resolve_base(
                                &route.storage,
                                &route.family,
                                &identity.scope,
                            )
                            .map_err(|e| SiblingLoader::error(e.to_string()))?
                        }
                        super::base_builder::BaseBuild::Busy => super::base_refs::resolve_base(
                            &route.storage,
                            &route.family,
                            &expected.scope,
                        )
                        .map_err(|_| {
                            SiblingLoader::error(
                                "v3 base build busy; retain direct-scan coverage and retry",
                            )
                        })?,
                        super::base_builder::BaseBuild::Strict(reason) => {
                            return Err(SiblingLoader::error(format!(
                                "v3 base admission refused: {reason}"
                            )))
                        }
                    }
                }
            };
        let resolved = self.complete_fresh_base(&route, resolved)?;
        let header = resolved.header();
        let generation = resolved.into_generation(&route.storage, &route.family);
        let manifest = super::manifest_v2::ManifestV2::new_v3(
            generation.header().clone(),
            Some(header),
            Default::default(),
        );
        let seed_route = generation.v3_route().unwrap().clone();
        let composed =
            ComposedGeneration::compose(generation.name(), manifest, generation.clone(), None)
                .map_err(|e| SiblingLoader::error(e.to_string()))?;
        Ok(Arc::new(composed.with_v3_route(seed_route)))
    }

    /// Completes the nonlexical planes of a fresh base before a worker
    /// publishes over it, so the worker's graph composes a ready base. The
    /// base's build claim is the completion lock: this driver uses the claim
    /// it took building the base, or takes over a dead builder's claim. A live
    /// foreign builder still owns the completion, so the bind refuses.
    fn complete_fresh_base(
        &self,
        route: &super::snapshot::V3Route,
        base: super::base_refs::AuthenticatedBase,
    ) -> Result<super::base_refs::AuthenticatedBase, PlaneError> {
        let completing: Vec<_> = self
            .planes
            .iter()
            .filter(|plane| plane.completes_fresh_base())
            .filter(|plane| {
                base.manifest().entries().any(|(_, entry)| {
                    entry
                        .plane_state(plane.plane())
                        .is_some_and(super::readiness::PlaneState::is_pending)
                })
            })
            .cloned()
            .collect();
        if completing.is_empty() {
            return Ok(base);
        }
        let scope = base.scope().to_owned();
        drop(base);
        let mut held = self
            .base_claim
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.as_ref().is_none_or(|claim| claim.scope() != scope) {
            match super::base_builder::claim_base(&route.storage, &route.family, &scope, None)
                .map_err(|e| SiblingLoader::error(e.to_string()))?
            {
                super::base_builder::ClaimOutcome::Claimed(claim) => {
                    if let Some(previous) = held.replace(claim) {
                        let _ = previous.release();
                    }
                }
                outcome => {
                    return Err(SiblingLoader::error(format!(
                        "v3 base plane completion is owned elsewhere: {outcome:?}"
                    )))
                }
            }
        }
        let claim = held.as_ref().expect("claim held above");
        let observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .clone();
        for plane in completing {
            plane.complete_v3_base(claim, &observed)?;
        }
        super::base_refs::resolve_base(&route.storage, &route.family, &scope)
            .map_err(|e| SiblingLoader::error(e.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_v3(
        storage: &std::path::Path,
        family: &str,
        scope: &str,
        root: &std::path::Path,
        producers: Producers,
        walker: Arc<dyn MembershipWalker>,
        planes: Vec<Arc<dyn CompositePlane>>,
    ) -> Result<Self, PlaneError> {
        let owner = super::registry::ViewRegistration::register_v3(storage, family, scope, root)
            .map_err(|e| SiblingLoader::error(e.to_string()))?;
        let driver = Self::new(owner, producers, None, walker, planes);
        driver
            .v3_reconcile
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(driver)
    }

    pub fn set_view_token(&self, token: String) {
        *self
            .advisory_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(token);
    }

    pub fn access(&self) -> ViewAccess {
        ViewAccess::Owner(self.owner.clone())
    }

    pub fn take_revalidated_job(&self) -> bool {
        self.retry.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    pub fn installed_bundle(&self) -> Arc<InstalledBundle> {
        self.check_subscription();
        self.installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn new(
        owner: super::registry::ViewRegistration,
        producers: Producers,
        head_tree: Option<String>,
        walker: Arc<dyn MembershipWalker>,
        planes: Vec<Arc<dyn CompositePlane>>,
    ) -> Self {
        let empty = super::manifest_v2::ManifestV2::new(super::manifest_v2::ManifestHeader {
            producers: producers.clone(),
            head_tree: head_tree.clone(),
            ignore_fingerprint: None,
            segment: None,
        });
        let mut delta = super::snapshot::LiveDelta::new(Arc::new(ComposedGeneration::new(
            "empty", empty, None,
        )));
        delta.set_watcher(super::snapshot::WatcherState::Reconciling);
        let composed = delta.base().clone();
        let readers = PlaneReaders::open(&ViewAccess::Owner(owner.clone()), composed.clone(), &[])
            .expect("empty reader lease");
        let snapshot = delta
            .snapshot()
            .with_readers(readers.clone())
            .with_install_cut(InstallCut {
                seq: 0,
                resident: composed.clone(),
                epoch: delta.epoch(),
                version: delta.version(),
                intent_version: delta.snapshot().intent_version(),
                verification: None,
            });
        Self {
            owner,
            producers,
            head_tree,
            walker,
            planes,
            adapters: Vec::new(),
            publish_fence: std::sync::RwLock::new(None),
            observed: Default::default(),
            watcher_cookies: Default::default(),
            delivery: Default::default(),
            v3_reconcile: Default::default(),
            advisory_token: Default::default(),
            base_claim: Default::default(),
            #[cfg(test)]
            read_hook: Default::default(),
            installed: std::sync::Mutex::new(Arc::new(InstalledBundle {
                seq: 0,
                composed,
                readers,
                coverage: BTreeMap::new(),
                published: Default::default(),
                revision: 0,
                snapshot,
                uncertain_paths: std::collections::BTreeSet::new(),
                own_installed: false,
            })),
            live: std::sync::Mutex::new(delta),
            prepared_readers: Default::default(),
            publication_transition: Default::default(),
            retry: Default::default(),
            #[cfg(test)]
            work: Default::default(),
        }
    }

    /// Registers resident plane readers for atomic installation with snapshots.
    /// The driver remains opt-in; this does not change process-global routing.
    /// Routes every pointer publication of this driver through `fence`, so
    /// a publisher that no longer owns the checkout's view cannot publish.
    pub fn set_publish_fence(&self, fence: Arc<dyn PublishFence>) {
        *self
            .publish_fence
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fence);
    }

    pub fn with_adapters(mut self, adapters: Vec<Arc<dyn super::contracts::PlaneAdapter>>) -> Self {
        self.adapters = adapters;
        self
    }

    /// Journals an unreflected write under the same root lock as installation.
    /// Ordinary edits replay onto a draft instead of invalidating its whole build.
    /// This method performs no filesystem writes.
    pub fn record_change(&self, path: RelPath) {
        let mut delta = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let installed = Arc::make_mut(&mut installed);
        {
            let mut delivery = self
                .delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            delivery.seq += 1;
            let seq = delivery.seq;
            delivery.paths.insert(path.clone(), seq);
        }
        installed.revision += 1;
        // Keep prior membership until reconciliation, but mark the edited path
        // as unreflected so queries cannot report stale data as installed.
        for plane in &self.planes {
            if plane.applies_to(&path) {
                installed
                    .coverage
                    .insert(plane.plane(), super::readiness::PlaneReadiness::Building);
            }
        }
        delta.record_intent(path);
        // Membership is unchanged, so are the ignore rules it was walked under.
        delta.record_root_ignore_files(installed.snapshot.root_ignore_files().cloned());
        // One known-path event cannot prove that delivery remained continuous
        // for the whole watcher stream.
        installed.snapshot = delta
            .snapshot()
            .with_readers(installed.readers.clone())
            .with_install_cut(InstallCut {
                seq: installed.seq,
                resident: installed.composed.clone(),
                epoch: delta.epoch(),
                version: delta.version(),
                intent_version: delta.snapshot().intent_version(),
                verification: installed
                    .snapshot
                    .install_cut()
                    .and_then(|cut| cut.verification.clone()),
            });
    }

    /// Invalidates stale builds and indexed membership until strict reconciliation.
    pub fn record_watcher_overflow(&self) {
        crate::watcher_backend::subscription::gap(self.owner.root());
        let mut delta = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let installed = Arc::make_mut(&mut installed);
        installed.revision += 1;
        delta.bump_epoch();
        delta.set_watcher(super::snapshot::WatcherState::Overflowed);
        installed.snapshot = delta
            .snapshot()
            .with_readers(installed.readers.clone())
            .with_install_cut(InstallCut {
                seq: installed.seq,
                resident: installed.composed.clone(),
                epoch: delta.epoch(),
                version: delta.version(),
                intent_version: delta.snapshot().intent_version(),
                verification: installed
                    .snapshot
                    .install_cut()
                    .and_then(|cut| cut.verification.clone()),
            });
    }

    /// Creates an exclusive root cookie. This does not enable verified bind;
    /// the caller must establish subscription coverage and reject delivery gaps.
    pub fn begin_watcher_cookie(&self) -> std::io::Result<CookieTicket> {
        self.watcher_cookies.begin(self.owner.root())
    }

    /// Called only after preceding dispatch paths have entered the driver.
    pub(crate) fn record_watcher_cookie(&self, path: &std::path::Path) {
        let installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.watcher_cookies.observe(
            path,
            CookieObservation {
                revision: installed.revision,
                watcher: installed.snapshot.watcher(),
            },
        );
    }

    fn check_owner(&self, access: &ViewAccess) -> Result<(), PlaneError> {
        match access {
            ViewAccess::Owner(owner)
                if owner.family() == self.owner.family() && owner.scope() == self.owner.scope() =>
            {
                Ok(())
            }
            _ => Err(SiblingLoader::error(
                "checkout driver refuses foreign writes",
            )),
        }
    }

    fn validate_install_identity(
        &self,
        snapshot: &Snapshot,
        cut: &InstallCut,
        live: &super::snapshot::LiveDelta,
        installed: &InstalledBundle,
    ) -> Result<(), PlaneError> {
        if cut.seq != installed.seq || !Arc::ptr_eq(&cut.resident, &installed.composed) {
            return Err(SiblingLoader::error(format!(
                "{CHECKOUT_CHANGED_BEFORE_INSTALL}: BaseChanged: installed bundle superseded"
            )));
        }
        // A prior ignore-file record is not proof that the current membership
        // epoch was walked. Only the completed strict walk carries this tag.
        let strict = snapshot.reconciled_epoch() == Some(live.epoch());
        if !installed.uncertain_paths.is_empty() && !strict {
            return Err(SiblingLoader::error(format!(
                "{CHECKOUT_CHANGED_BEFORE_INSTALL}: membership requires strict reconciliation"
            )));
        }
        match live.watcher() {
            super::snapshot::WatcherState::Healthy => Ok(()),
            super::snapshot::WatcherState::Reconciling
            | super::snapshot::WatcherState::Overflowed
                if strict =>
            {
                Ok(())
            }
            _ => Err(SiblingLoader::error(format!(
                "{CHECKOUT_CHANGED_BEFORE_INSTALL}: watcher is not healthy or strictly reconciled"
            ))),
        }
    }
}

impl FirstLoadDriver for CheckoutDriver {
    fn installed_after(&self, _: &Snapshot) -> Snapshot {
        self.installed_snapshot()
    }
    fn validate_cut(&self, _: &ViewAccess, snapshot: &Snapshot, _: u64) -> Result<(), PlaneError> {
        let cut = snapshot
            .install_cut()
            .ok_or_else(|| SiblingLoader::error("missing installed bundle identity"))?;
        let live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        live.replay_onto(snapshot, cut)
            .map(|_| ())
            .map_err(|error| {
                SiblingLoader::error(format!("{CHECKOUT_CHANGED_BEFORE_INSTALL}: {error:?}"))
            })?;
        let installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.validate_install_identity(snapshot, cut, &live, &installed)
    }
    fn producers(&self, _: &ViewAccess) -> Producers {
        self.producers.clone()
    }
    fn head_tree(&self, _: &ViewAccess) -> Option<String> {
        self.head_tree.clone()
    }
    fn revision(&self, _: &ViewAccess) -> u64 {
        self.installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision
    }
    fn reconcile(&self, access: &ViewAccess) -> Result<ReconciledCheckout, PlaneError> {
        self.check_owner(access)?;
        if super::intent::active(self.owner.root()) {
            return Err(SiblingLoader::error(CHECKOUT_WRITE_ACTIVE));
        }
        let publication_base =
            match super::ViewStore::existing_dir(self.owner.view_dir().to_path_buf()) {
                Some(store) => store
                    .current_generation_read_only()
                    .map_err(|error| SiblingLoader::error(error.to_string()))?,
                None => None,
            };
        let (installed, revision) = self.installed_cut();
        let pending: std::collections::BTreeSet<_> = installed.pending_intent().cloned().collect();
        let previous = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let v3 = self.v3_reconcile.load(std::sync::atomic::Ordering::Acquire)
            || installed.generation().v3_route().is_some();
        let guard = if v3 {
            Some(Arc::new(
                super::verification::BindGuard::capture(self.owner.root(), None)
                    .map_err(|e| SiblingLoader::error(e.to_string()))?,
            ))
        } else {
            None
        };
        let route = self.owner.v3_route();
        let durable = if v3 && previous.entries.is_empty() {
            super::base_refs::open_foreign_v3(
                &route.storage,
                &route.family,
                &route.scope,
                &self.producers,
            )
            .ok()
            .flatten()
        } else {
            None
        };
        let persisted = durable
            .as_ref()
            .and_then(|generation| {
                super::verification::read_evidence(
                    &route.member_dir(),
                    generation.name(),
                    guard.as_ref().unwrap(),
                )
            })
            .unwrap_or_default();
        let subscription = crate::watcher_backend::subscription::current(self.owner.root());
        let mut changed = pending.clone();
        changed.extend(
            installed
                .live_entries()
                .filter(|(_, entry)| {
                    entry.reconciliation != super::readiness::ReconciledDelta::Reconciled
                })
                .map(|(path, _)| path.clone()),
        );
        let mut candidates = std::collections::BTreeSet::new();
        let mut policy_or_directory = false;
        for path in &changed {
            let relative = super::segment_store::rel_path_to_os(path)
                .map_err(|e| SiblingLoader::error(e.to_string()))?;
            let absolute = self.owner.root().join(relative);
            let name = absolute.file_name().and_then(|n| n.to_str()).unwrap_or("");
            policy_or_directory |= matches!(
                name,
                ".gitattributes" | ".gitignore" | ".ignore" | ".aftignore"
            ) || path.as_bytes().starts_with(b".git/")
                || super::assembly::is_resolution_input(path.as_bytes())
                || std::fs::symlink_metadata(&absolute).is_ok_and(|m| m.is_dir());
            candidates.insert(absolute);
        }
        let continuous = v3
            && subscription.is_some()
            && subscription == previous.subscription
            && installed.watcher() == super::snapshot::WatcherState::Healthy
            && guard
                .as_ref()
                .zip(previous.guard.as_ref())
                .is_some_and(|(now, old)| now.root == old.root && now.policy == old.policy)
            && guard
                .as_ref()
                .is_some_and(|guard| guard.policy.reusable && guard.steady_supported)
            && !policy_or_directory;
        let changed_files = if continuous {
            self.walker.changed_files(self.owner.root(), &candidates)?
        } else {
            None
        };
        let incremental = changed_files.is_some();
        let files = match changed_files {
            Some(files) => files,
            None => self.walker.files(self.owner.root())?,
        };
        let mut entries = if incremental {
            previous.entries.clone()
        } else {
            BTreeMap::new()
        };
        let mut stats = if incremental {
            previous.stats.clone()
        } else {
            BTreeMap::new()
        };
        let mut evidence = if incremental {
            previous.evidence.clone()
        } else {
            BTreeMap::new()
        };
        if incremental {
            for path in &changed {
                entries.remove(path);
                stats.remove(path);
                evidence.remove(path);
                let mut prefix = path.as_bytes().to_vec();
                prefix.push(b'/');
                entries.retain(|p, _| !p.as_bytes().starts_with(&prefix));
                stats.retain(|p, _| !p.as_bytes().starts_with(&prefix));
                evidence.retain(|p, _| !p.as_bytes().starts_with(&prefix));
            }
        }
        #[cfg(test)]
        {
            if !incremental {
                self.work.lock().unwrap().walks += 1;
            }
        }
        // Restart and delivery gaps require full membership. A continuous
        // subscription checks only reported files under the same walker policy.
        for absolute in files {
            let relative = absolute
                .strip_prefix(self.owner.root())
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
            let path = RelPath::from_os_path(relative)
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
            // Round down to the second so coarse filesystem timestamps in the
            // observation's second are also treated as racily clean.
            let recorded_ns = super::verification::now_ns();
            let delivery_seq = self
                .delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .seq;
            let before = self.source_stat(&absolute, recorded_ns)?;
            if !pending.contains(&path) {
                if let (Some(old), Some(cached), Some(current)) = (
                    previous.entries.get(&path),
                    previous.stats.get(&path),
                    before.as_ref(),
                ) {
                    let delivered = self
                        .delivery
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .paths
                        .get(&path)
                        .copied()
                        .unwrap_or(0);
                    let policy_matches = !v3
                        || guard.as_ref().is_some_and(|guard| {
                            guard.policy.reusable
                                && previous.guard.as_ref().is_some_and(|old| {
                                    old.policy == guard.policy && old.root == guard.root
                                })
                        });
                    let keys_present = old.attachments.values().all(|a| {
                        a.downcast_ref::<CachedAttachment>()
                            .is_none_or(|cached| v3_contains(&route, &cached.0))
                    });
                    if cached.reusable(current)
                        && delivered <= cached.delivery_seq
                        && policy_matches
                        && keys_present
                    {
                        let mut entry = old.clone();
                        entry.seq = revision;
                        entries.insert(path.clone(), entry);
                        stats.insert(path.clone(), cached.clone());
                        if let Some(record) = previous.evidence.get(&path) {
                            evidence.insert(path.clone(), record.clone());
                        }
                        continue;
                    }
                }
            }
            if !pending.contains(&path) && guard.as_ref().is_some_and(|g| g.policy.reusable) {
                if let (Some(record), Some(current), Some(generation)) =
                    (persisted.get(&path), before.as_ref(), durable.as_ref())
                {
                    if record.stat.reusable(
                        current.native.as_ref().unwrap_or(&record.stat),
                        record.observed_ns,
                    ) && current.native.is_some()
                        && record.content().is_some_and(|content| {
                            generation.disk_state(&path)
                                == (super::snapshot::DiskState::Present {
                                    content,
                                    size: record.size,
                                })
                        })
                        && self.planes.iter().filter(|p| p.applies_to(&path)).all(|p| {
                            record.key(p.plane()).is_some_and(|key| {
                                matches!(generation.plane_readiness(&path, p.plane()), Some(super::readiness::PlaneState::Ready { key: ready }) if *ready == key.to_hex())
                                    && record.content().is_some_and(|content| p.key(&path, content).is_none_or(|expected| expected == key))
                                    && v3_contains(&route, &key)
                            })
                        })
                        && self
                            .delivery
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .paths
                            .get(&path)
                            .copied()
                            .unwrap_or(0)
                            == 0
                    {
                        let mut entry = LiveEntry::new(generation.disk_state(&path), revision);
                        entry.reconciliation = super::readiness::ReconciledDelta::Reconciled;
                        for plane in &self.planes {
                            if plane.applies_to(&path) {
                                let key = record.key(plane.plane()).unwrap();
                                entry.attachments.insert(
                                    plane.plane(),
                                    plane.cached_attachment(record.content().unwrap(), key),
                                );
                            }
                        }
                        let mut current = current.clone();
                        current.recorded_ns = record.observed_ns;
                        stats.insert(path.clone(), current);
                        entries.insert(path.clone(), entry);
                        evidence.insert(path, record.clone());
                        continue;
                    }
                }
            }
            let (bytes, stat_evidence) =
                match self.stable_read(&path, &absolute, recorded_ns, delivery_seq, before)? {
                    StableRead::Absent => continue,
                    StableRead::Bytes(bytes, evidence) => (bytes, evidence),
                    StableRead::Pending => {
                        let mut entry = LiveEntry::new(installed.disk_state(&path), revision);
                        entry.reconciliation = super::readiness::ReconciledDelta::Pending;
                        entries.insert(path, entry);
                        continue;
                    }
                };
            stats.insert(path.clone(), stat_evidence);
            let content = crate::blob_store::v2::ContentHash::of(&bytes);
            #[cfg(test)]
            {
                self.work.lock().unwrap().content_hash_bytes += bytes.len();
            }
            let mut entry = LiveEntry::new(
                super::snapshot::DiskState::Present {
                    content,
                    size: bytes.len() as u64,
                },
                revision,
            );
            entry.reconciliation = super::readiness::ReconciledDelta::Reconciled;
            let mut keys = BTreeMap::new();
            for plane in &self.planes {
                if plane.applies_to(&path) {
                    let key = plane.key(&path, content);
                    if v3 && key.is_some_and(|key| v3_contains(&route, &key)) {
                        let key = key.unwrap();
                        keys.insert(format!("{:?}", plane.plane()), key.to_hex());
                        entry
                            .attachments
                            .insert(plane.plane(), plane.cached_attachment(content, key));
                        continue;
                    }
                    #[cfg(test)]
                    {
                        self.work.lock().unwrap().attachments += 1;
                    }
                    let attachment = plane.attachment(&path, &bytes)?;
                    if let Some(key) = key.or_else(|| plane.attachment_key(&attachment)) {
                        keys.insert(format!("{:?}", plane.plane()), key.to_hex());
                    }
                    entry.attachments.insert(plane.plane(), attachment);
                }
            }
            if let (Some(guard), Some(stat)) =
                (&guard, stats.get(&path).and_then(|s| s.native.clone()))
            {
                evidence.insert(
                    path.clone(),
                    super::verification::Evidence {
                        version: 1,
                        stat,
                        observed_ns: recorded_ns,
                        content: content.to_hex(),
                        size: bytes.len() as u64,
                        git_oid: None,
                        policy: guard.policy.digest.clone(),
                        keys,
                    },
                );
            }
            entries.insert(path, entry);
        }
        *self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ObservedCheckout {
            entries: entries.clone(),
            stats,
            evidence,
            guard: guard.clone(),
            subscription,
        };
        let mut cut = installed.install_cut().cloned();
        if let Some(cut) = &mut cut {
            cut.verification = guard;
        }
        Ok(ReconciledCheckout {
            revision,
            entries,
            cut,
            readiness: installed.readiness_patches().clone(),
            publication_base: Some(publication_base),
        })
    }
    fn install(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        _revision: u64,
    ) -> Result<(), PlaneError> {
        self.check_owner(access)?;
        let cut = snapshot
            .install_cut()
            .ok_or_else(|| SiblingLoader::error("missing installed bundle identity"))?;
        let old_bundle = self.installed_bundle();
        let previously_owned = old_bundle.own_installed
            && old_bundle.composed.resident_key() == snapshot.generation().resident_key();
        drop(old_bundle);
        let prepared = self
            .prepared_readers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(snapshot.generation().name())
            .map(|prepared| prepared.readers.clone());
        let from_prepared = prepared.is_some();
        let own_installed = prepared.is_some() || previously_owned;
        let readers = match prepared {
            Some(readers) => readers,
            None => {
                let old = self
                    .installed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if Arc::ptr_eq(&old.composed, snapshot.generation()) {
                    old.readers.clone()
                } else {
                    drop(old);
                    let adapters: Vec<_> = self
                        .adapters
                        .iter()
                        .filter(|adapter| {
                            snapshot.generation().name() != "empty"
                                && (adapter.plane() != FamilyPlane::Callgraph || own_installed)
                        })
                        .cloned()
                        .collect();
                    PlaneReaders::open(access, snapshot.generation().clone(), &adapters)?
                }
            }
        };
        let publication_guard = self
            .publication_transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let install_error = |error: PlaneError| {
            if from_prepared {
                self.retry.store(true, std::sync::atomic::Ordering::Release);
                SiblingLoader::error(format!(
                    "{CHECKOUT_CHANGED_BEFORE_INSTALL}: published installation refused: {}",
                    error.reason
                ))
            } else {
                error
            }
        };
        let mut successor = live.replay_onto(snapshot, cut).map_err(|error| {
            install_error(SiblingLoader::error(format!(
                "{CHECKOUT_CHANGED_BEFORE_INSTALL}: {error:?}"
            )))
        })?;
        let mut installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.validate_install_identity(snapshot, cut, &live, &installed)
            .map_err(install_error)?;
        let seq = installed.seq + 1;
        successor.set_watcher(super::snapshot::WatcherState::Healthy);
        let published = snapshot
            .generation()
            .published()
            .cloned()
            .unwrap_or_default();
        let mut installed_snapshot = successor
            .snapshot()
            .with_readers(readers.clone())
            .with_install_cut(InstallCut {
                seq,
                resident: snapshot.generation().clone(),
                epoch: successor.epoch(),
                version: successor.version(),
                intent_version: successor.snapshot().intent_version(),
                verification: cut.verification.clone(),
            });
        if snapshot.reconciled_epoch().is_some() {
            if let Some(base) = snapshot.publication_base() {
                installed_snapshot = installed_snapshot.with_publication_base(base.clone());
            }
        }
        let coverage = FamilyPlane::ALL
            .into_iter()
            .map(|plane| {
                (
                    plane,
                    if installed_snapshot.pending_intent().any(|path| {
                        self.planes
                            .iter()
                            .any(|driver| driver.plane() == plane && driver.applies_to(path))
                    }) {
                        super::readiness::PlaneReadiness::Building
                    } else {
                        super::readiness::plane_readiness(
                            plane,
                            Some(snapshot.generation()),
                            &installed_snapshot,
                            &Default::default(),
                            &|path| {
                                self.planes.iter().any(|bridge| {
                                    bridge.plane() == plane && bridge.applies_to(path)
                                })
                            },
                        )
                    },
                )
            })
            .collect();
        let replacement = Arc::new(InstalledBundle {
            seq,
            composed: snapshot.generation().clone(),
            coverage,
            readers,
            published,
            revision: installed.revision,
            snapshot: installed_snapshot,
            uncertain_paths: Default::default(),
            own_installed,
        });
        let retired_bundle = std::mem::replace(&mut *installed, replacement);
        let retired_delta = std::mem::replace(&mut *live, successor);
        drop(installed);
        drop(live);
        drop(publication_guard);
        // Last-drop reader close and residency-pin cleanup can perform I/O.
        // Keep displaced owners alive until both root locks are released.
        drop(retired_bundle);
        drop(retired_delta);
        let mut retired_prepared = Vec::new();
        {
            let mut prepared = self
                .prepared_readers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            prepared.retain(|_, entry| {
                if entry.seq < seq {
                    retired_prepared.push(entry.readers.clone());
                    false
                } else {
                    true
                }
            });
        }
        drop(retired_prepared);
        Ok(())
    }
    fn build_own_generation(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        seed_derived: bool,
    ) -> Result<Arc<ComposedGeneration>, PlaneError> {
        self.check_owner(access)?;
        if let Some(header) = snapshot.generation().base_header() {
            return self.build_v3_worker_generation(access, snapshot, header.clone());
        }
        if self.owner.registry().is_v3_only() {
            return Err(SiblingLoader::error(
                "v3-only driver requires verified_bind bootstrap before publication",
            ));
        }
        super::io::WORK
            .own_generation_folds
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let store = self
            .owner
            .view_store()
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let base = match snapshot.publication_base() {
            Some(base) => base.clone(),
            None => store
                .current_generation()
                .map_err(|error| SiblingLoader::error(error.to_string()))?,
        };
        let mut manifest = snapshot
            .generation()
            .flatten()
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        for (path, entry) in snapshot.live_entries() {
            let value = match entry.disk {
                super::snapshot::DiskState::Absent => None,
                super::snapshot::DiskState::Present { content, size } => {
                    let mut planes = super::manifest_v2::EntryPlanes::default();
                    for plane in &self.planes {
                        if plane.applies_to(path) {
                            *planes.get_mut(plane.plane()) =
                                Some(super::readiness::PlaneState::pending(
                                    "awaiting checkout materialization",
                                ));
                        }
                    }
                    Some(super::manifest_v2::EntryV2::regular(content, size, planes))
                }
            };
            manifest
                .set(path.clone(), value)
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
        }
        // Materialization changes readiness and therefore the manifest hash.
        // Protect private build files first, then protect the final hashed name
        // before renaming derived artifacts and publishing the pointer.
        let assembly_name = super::manifest_v2::GenerationName::for_manifest(&manifest)
            .map_err(|error| SiblingLoader::error(error.to_string()))?
            .to_string();
        let _assembly = crate::pins::AssemblyPin::create_v2(&self.owner, &assembly_name, &[])
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let mut unpublished =
            super::generation::UnpublishedGenerations::new(&store, self.owner.registry());
        unpublished.track_new(&assembly_name);
        let mut live = crate::pins::LivePin::create(&self.owner)
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        live.protect(&manifest.ready_keys().collect::<Vec<_>>())
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        if let Some(segment) = manifest.segment_id() {
            live.protect_segment(&segment)
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
        }
        let observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .clone();
        for plane in &self.planes {
            plane.materialize(
                &self.owner,
                &assembly_name,
                snapshot,
                &observed,
                &mut manifest,
                &mut live,
                seed_derived,
            )?;
        }
        let name = super::manifest_v2::GenerationName::for_manifest(&manifest)
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let _publication = crate::pins::AssemblyPin::create_v2(
            &self.owner,
            &name.to_string(),
            &manifest.ready_keys().collect::<Vec<_>>(),
        )
        .map_err(|error| SiblingLoader::error(error.to_string()))?;
        unpublished.track_new(&name.to_string());
        for plane in &self.planes {
            plane.finish_generation(&self.owner, &assembly_name, &name.to_string())?;
        }
        let mut prepared = Some(
            store
                .prepare_v2(&name, base.as_deref(), &manifest, None)
                .map_err(|error| SiblingLoader::error(error.to_string()))?,
        );
        let marker =
            crate::root_cache::ReadMarker::create(self.owner.view_dir(), &name.to_string())
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let generation = Arc::new(ComposedGeneration::new(
            name.to_string(),
            manifest,
            Some(super::snapshot::Residency::Marker(marker)),
        ));
        // Reader construction is fallible and may open SQLite. It must finish
        // before publication and never run under the installed-bundle mutex.
        let readers = PlaneReaders::open(access, generation.clone(), &self.adapters)?;
        let mut published = false;
        let mut progress = PublicationProgress::NotAttempted;
        let mut retired_readers = Vec::new();
        let mut commit = || {
            let _publication_guard = self
                .publication_transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.validate_cut(access, snapshot, 0)?;
            let prepared = prepared
                .take()
                .ok_or_else(|| SiblingLoader::error("publication committed twice"))?;
            progress = PublicationProgress::MayHaveLanded;
            store
                .commit_v2_before_sweep(prepared, None, || store.current_generation_read_only().map(|_| ()))
                .map_err(|error| SiblingLoader::error(error.to_string()))
                .and_then(|outcome| match outcome {
                    super::manifest_v2::PublishV2::Published => {
                        progress = PublicationProgress::Published;
                        unpublished.complete();
                        published = true;
                        self.owner.registry().note_publish(self.owner.scope()).map_err(|error| SiblingLoader::error(error.to_string()))?;
                        for adapter in &self.adapters {
                            adapter.generation_published(access, &generation, &|| self.installed_snapshot())?;
                        }
                        let rebound = readers.rebind(access)?;
                        let retired = self.prepared_readers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(generation.name().to_owned(), PreparedReaders { seq: snapshot.install_cut().map_or(0, |cut| cut.seq), readers: rebound });
                        if let Some(retired) = retired { retired_readers.push(retired.readers); }
                        Ok(())
                    },
                    outcome @ (super::manifest_v2::PublishV2::Conflict { .. } | super::manifest_v2::PublishV2::EquivalentWinner { .. }) => {
                        progress = PublicationProgress::NotAttempted;
                        self.retry.store(true, std::sync::atomic::Ordering::Release);
                        Err(SiblingLoader::error(format!("{CHECKOUT_CHANGED_BEFORE_INSTALL}: publication lost ({outcome:?}); revalidate a fresh job")))
                    }
                })
        };
        let fence = self
            .publish_fence
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let publication = match fence {
            Some(fence) => fence.publish(&mut commit),
            None => commit(),
        };
        drop(commit);
        drop(retired_readers);
        if let Err(error) = publication {
            if progress != PublicationProgress::NotAttempted {
                unpublished.complete();
                self.retry.store(true, std::sync::atomic::Ordering::Release);
                return Err(SiblingLoader::error(format!("{CHECKOUT_CHANGED_BEFORE_INSTALL}: publication may have landed; revalidate: {}", error.reason)));
            }
            return Err(error);
        }
        if !published {
            return Err(SiblingLoader::error("publication fence did not commit"));
        }
        Ok(generation)
    }
}

impl CheckoutDriver {
    /// Publishes this checkout's generation as a v3 worker that references the
    /// base named by `header`. The header is only a lookup: the base is pinned
    /// and authenticated (digests recomputed) before any of this worker's plane
    /// artifacts or its manifest is written. The published manifest is sparse: entries whose bytes differ
    /// from the base, tombstones for base paths gone from the checkout, and
    /// readiness overrides from the live delta's readiness patches; inherited
    /// base entries are never copied. Plane rows go to the family's v3 plane
    /// stores under a v3 live pin, derived files to the checkout's v3 member
    /// directory, and the pointer CAS, referrer row and markers follow
    /// `base_refs::publish_worker`. Nothing is written to v1 or v2 storage.
    fn build_v3_worker_generation(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        header: super::manifest_v2::BaseHeader,
    ) -> Result<Arc<ComposedGeneration>, PlaneError> {
        let failed =
            |error: super::base_refs::BaseRefError| SiblingLoader::error(error.to_string());
        let route = self.owner.v3_route();
        let storage = route.storage.clone();
        let family = route.family.clone();
        let scope = route.scope.clone();
        let base = super::base_refs::resolve_base(&storage, &family, &header.scope)
            .and_then(|base| base.check_header(&header).map(|()| base))
            .map_err(failed)?;
        let base_header = base.header();
        let base_generation = base.into_generation(&storage, &family);
        // The sparse delta: fold this cut's changes into the installed
        // worker delta. A path whose bytes equal the base's inherits the base
        // entry (with any checkout-specific readiness patch as an override);
        // a deleted base path becomes a tombstone.
        let generation = snapshot.generation();
        let inherited = |path: &RelPath,
                         content: &crate::blob_store::v2::ContentHash,
                         size: u64| {
            generation.base().and_then(|base| base.entry(path)).filter(|entry| {
                matches!(entry, super::manifest_v2::EntryV2::Regular { content: c, size: s, .. } if c == content && *s == size)
            })
        };
        let patched = |path: &RelPath, entry: &super::manifest_v2::EntryV2| {
            let mut entry = entry.clone();
            if let (
                Some(patch),
                super::manifest_v2::EntryV2::Regular {
                    content, planes, ..
                },
            ) = (snapshot.readiness_patches().get(path), &mut entry)
            {
                if patch.content == *content {
                    for plane in FamilyPlane::ALL {
                        if let Some(state) = patch.planes.get(plane) {
                            *planes.get_mut(plane) = Some(state.clone());
                        }
                    }
                }
            }
            entry
        };
        let mut updates = Vec::new();
        let mut changed = std::collections::BTreeSet::new();
        for (path, entry) in snapshot.live_entries() {
            changed.insert(path.clone());
            let update = match entry.disk {
                super::snapshot::DiskState::Absent => None,
                super::snapshot::DiskState::Present { content, size } => {
                    match inherited(path, &content, size) {
                        Some(base_entry) => Some(patched(path, base_entry)),
                        None => {
                            let mut planes = super::manifest_v2::EntryPlanes::default();
                            for plane in &self.planes {
                                if plane.applies_to(path) {
                                    *planes.get_mut(plane.plane()) =
                                        Some(super::readiness::PlaneState::pending(
                                            "awaiting checkout materialization",
                                        ));
                                }
                            }
                            Some(super::manifest_v2::EntryV2::regular(content, size, planes))
                        }
                    }
                }
            };
            updates.push((path.clone(), update));
        }
        for (path, patch) in snapshot.readiness_patches() {
            if changed.contains(path) {
                continue;
            }
            if let Some(base_entry) = generation.base().and_then(|base| base.entry(path)) {
                if base_entry.content() == Some(patch.content) {
                    updates.push((path.clone(), Some(patched(path, base_entry))));
                }
            }
        }
        let mut delta = generation
            .fold(
                &generation.header().producers,
                updates,
                &super::readiness::FillMap::default(),
            )
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        // A deleted path the base never had (a file this worker added and then
        // removed) leaves the delta without an entry or a tombstone: a
        // tombstone may only name an inherited base entry.
        for (path, entry) in snapshot.live_entries() {
            if entry.disk == super::snapshot::DiskState::Absent
                && generation
                    .base()
                    .is_some_and(|base| base.entry(path).is_none())
            {
                delta.inherit(path.clone(), None);
            }
        }
        delta
            .set_base_header(base_header.clone())
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        // Register the v3 member and protect the private staging name before
        // any plane writes derived files under it.
        super::base_refs::with_base_barrier(&storage, &family, |tx| {
            super::base_refs::register_view_in(tx, &storage, &family, &scope, self.owner.root())
                .map(|_| ())
        })
        .map_err(failed)?;
        let staging = format!(
            "staging-{}-{}",
            std::process::id(),
            super::base_refs::fresh_nonce()
        );
        let _staging_pin = crate::pins::AssemblyPin::create(
            &route.member_dir(),
            family.clone(),
            scope.clone(),
            staging.clone(),
            &[],
        )
        .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let mut live = crate::pins::LivePin::create_v3(&storage, &family, &scope)
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .clone();
        for plane in &self.planes {
            plane.materialize_v3(
                &self.owner,
                &route,
                &staging,
                &base_generation,
                &observed,
                &mut delta,
                &mut live,
            )?;
        }
        // `Wold`: the installed v3 generation of this checkout, or the
        // durable v3 pointer when the installed bundle is not one.
        let previous = match generation.v3_route() {
            Some(installed) if installed.scope == scope => Some(generation.name().to_owned()),
            _ => super::ViewStore::existing_dir(route.member_dir())
                .map(|store| store.current_generation_read_only())
                .transpose()
                .map_err(|error| SiblingLoader::error(error.to_string()))?
                .flatten(),
        };
        let planes = &self.planes;
        let fence = self
            .publish_fence
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let outcome = super::base_refs::publish_worker_guarded(
            super::base_refs::WorkerPublication {
                storage: &storage,
                family: &family,
                worker_scope: &scope,
                root: self.owner.root(),
                base: &base_header,
                delta,
                previous,
            },
            None,
            &mut |published| {
                for plane in planes {
                    plane
                        .finish_generation_v3(&route, &staging, published)
                        .map_err(|error| {
                            super::base_refs::BaseRefError::Io(std::io::Error::other(error.reason))
                        })?;
                }
                if let Some(guard) = snapshot
                    .install_cut()
                    .and_then(|cut| cut.verification.as_ref())
                {
                    let evidence = self
                        .observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .evidence
                        .clone();
                    super::verification::write_evidence(&route, published, guard, &evidence)
                        .map_err(super::base_refs::BaseRefError::Io)?;
                }
                Ok(())
            },
            &mut || {
                if let Some(guard) = snapshot
                    .install_cut()
                    .and_then(|cut| cut.verification.as_ref())
                {
                    guard
                        .validate(self.owner.root())
                        .map_err(super::base_refs::BaseRefError::Io)?;
                }
                self.validate_cut(access, snapshot, 0)
                    .map_err(|error| super::base_refs::BaseRefError::Unavailable(error.reason))
            },
            &mut |commit| {
                let mut actual = None;
                let mut publish = || {
                    let _transition = self
                        .publication_transition
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    actual =
                        Some(commit().map_err(|error| SiblingLoader::error(error.to_string()))?);
                    Ok(())
                };
                match &fence {
                    Some(fence) => fence.publish(&mut publish),
                    None => publish(),
                }
                .map_err(|error| super::base_refs::BaseRefError::Unavailable(error.reason))?;
                drop(publish);
                actual.ok_or_else(|| {
                    super::base_refs::BaseRefError::Unavailable(
                        "publication fence did not invoke CAS".into(),
                    )
                })
            },
        );
        drop(live);
        match outcome {
            Ok(super::base_refs::WorkerOutcome::Published { composed, .. }) => {
                let readers = PlaneReaders::open(access, composed.clone(), &self.adapters)?;
                for adapter in &self.adapters {
                    adapter
                        .generation_published(access, &composed, &|| self.installed_snapshot())?;
                }
                let retired = self
                    .prepared_readers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        composed.name().to_owned(),
                        PreparedReaders {
                            seq: snapshot.install_cut().map_or(0, |cut| cut.seq),
                            readers,
                        },
                    );
                drop(retired);
                Ok(composed)
            }
            Ok(super::base_refs::WorkerOutcome::Lost {
                current,
                equivalent,
            }) => {
                self.retry.store(true, std::sync::atomic::Ordering::Release);
                Err(SiblingLoader::error(format!(
                    "{CHECKOUT_CHANGED_BEFORE_INSTALL}: publication lost ({}, current {current:?}); revalidate a fresh job",
                    if equivalent { "EquivalentWinner" } else { "Conflict" }
                )))
            }
            Err(error) => {
                // If the CAS landed, its referrer is published or left as an
                // intent row for reconciliation; the next job revalidates
                // from disk.
                self.retry.store(true, std::sync::atomic::Ordering::Release);
                Err(SiblingLoader::error(format!(
                    "{CHECKOUT_CHANGED_BEFORE_INSTALL}: v3 worker publication failed or may have landed; revalidate: {error}"
                )))
            }
        }
    }
}

impl QueryState for CheckoutDriver {
    fn installed_state(
        &self,
        access: &ViewAccess,
        plane: FamilyPlane,
    ) -> (Snapshot, Vec<std::path::PathBuf>) {
        self.check_subscription();
        let installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = installed.snapshot.clone();
        let applicable = |path: &RelPath| {
            self.planes
                .iter()
                .any(|driver| driver.plane() == plane && driver.applies_to(path))
        };
        let mut gaps = std::collections::BTreeSet::new();
        if self.adapters.iter().any(|adapter| {
            adapter.plane() == plane
                && matches!(
                    adapter.readiness(access, &snapshot),
                    super::readiness::PlaneReadiness::Absent
                        | super::readiness::PlaneReadiness::Building
                )
        }) {
            gaps.extend(
                snapshot
                    .membership()
                    .into_keys()
                    .filter(|path| applicable(path)),
            );
        }
        gaps.extend(
            snapshot
                .pending_intent()
                .filter(|path| applicable(path))
                .cloned(),
        );
        gaps.extend(
            snapshot
                .live_entries()
                .filter(|(path, entry)| {
                    applicable(path)
                        && (entry.reconciliation != super::readiness::ReconciledDelta::Reconciled
                            || !matches!(
                                snapshot.plane_readiness(path, plane),
                                Some(super::readiness::PlaneState::Ready { .. })
                            ))
                })
                .map(|(path, _)| path.clone()),
        );
        for (path, _entry) in snapshot.generation().entries() {
            if applicable(path)
                && ((plane == FamilyPlane::Callgraph && !installed.own_installed)
                    || !matches!(
                        snapshot.plane_readiness(path, plane),
                        Some(super::readiness::PlaneState::Ready { .. })
                    ))
            {
                gaps.insert(path.clone());
            }
        }
        let mut paths: Vec<_> = gaps
            .into_iter()
            .map(|path| {
                #[cfg(unix)]
                let relative = {
                    use std::os::unix::ffi::OsStringExt;
                    std::ffi::OsString::from_vec(path.as_bytes().to_vec())
                };
                #[cfg(not(unix))]
                let relative =
                    std::ffi::OsString::from(String::from_utf8_lossy(path.as_bytes()).into_owned());
                self.owner.root().join(relative)
            })
            .collect();
        paths.extend(installed.uncertain_paths.iter().cloned());
        paths.sort();
        paths.dedup();
        (snapshot, paths)
    }
}

#[cfg(test)]
mod composite_tests {
    use super::super::contracts::{PlaneAdapter, PlaneLoader, QueryWait, WaitOutcome};
    use super::super::manifest_v2::{EntryPlanes, EntryV2, ManifestV2};
    use super::super::snapshot::{DiskState, PlaneAttachment};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Walker {
        fail: AtomicBool,
        delete_after_listing: AtomicBool,
    }
    impl MembershipWalker for Walker {
        fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError> {
            let first = root.join("file.rs");
            if self.fail.load(Ordering::SeqCst) {
                return Err(SiblingLoader::error(
                    "blocked.rs: membership walk failed after file.rs",
                ));
            }
            if self.delete_after_listing.swap(false, Ordering::SeqCst) {
                std::fs::remove_file(&first).unwrap();
            }
            Ok(vec![first])
        }
    }
    struct Plane {
        fail: AtomicBool,
    }
    impl CompositePlane for Plane {
        fn plane(&self) -> FamilyPlane {
            FamilyPlane::Trigram
        }
        fn applies_to(&self, _: &RelPath) -> bool {
            true
        }
        fn attachment(&self, _: &RelPath, bytes: &[u8]) -> Result<PlaneAttachment, PlaneError> {
            Ok(Arc::new(bytes.to_vec()))
        }
        fn materialize(
            &self,
            owner: &super::super::registry::ViewRegistration,
            _: &str,
            snapshot: &Snapshot,
            _observed: &BTreeMap<RelPath, LiveEntry>,
            manifest: &mut ManifestV2,
            live: &mut crate::pins::LivePin,
            seed_derived: bool,
        ) -> Result<(), PlaneError> {
            assert!(!seed_derived);
            if self.fail.load(Ordering::SeqCst) {
                return Err(SiblingLoader::error("file.rs: materializer refused"));
            }
            for (path, entry) in snapshot.live_entries() {
                if let DiskState::Present { content, size } = entry.disk {
                    let bytes = entry.attachments[&FamilyPlane::Trigram]
                        .downcast_ref::<Vec<u8>>()
                        .unwrap();
                    assert_eq!(DiskState::of_bytes(bytes), entry.disk);
                    let policy = crate::blob_store::v2::TrigramPolicy {
                        max_file_size: 1 << 20,
                    };
                    let key = crate::blob_store::v2::TrigramKey {
                        content,
                        policy: policy.clone(),
                    }
                    .family_key();
                    live.protect(&[key]).unwrap();
                    owner
                        .open_store(FamilyPlane::Trigram)
                        .unwrap()
                        .put_or_touch(
                            &key,
                            &super::super::segment_store::TrigramPayload::extract(bytes, &policy)
                                .encode(),
                        )
                        .unwrap();
                    manifest
                        .set(
                            path.clone(),
                            Some(EntryV2::regular(
                                content,
                                size,
                                EntryPlanes {
                                    trigram: Some(super::super::readiness::PlaneState::ready(&key)),
                                    semantic: None,
                                    callgraph: None,
                                },
                            )),
                        )
                        .unwrap();
                }
            }
            Ok(())
        }
        fn finish_generation(
            &self,
            _: &super::super::registry::ViewRegistration,
            _: &str,
            _: &str,
        ) -> Result<(), PlaneError> {
            Ok(())
        }
    }
    fn fixture(
        root: &std::path::Path,
        storage: &std::path::Path,
    ) -> (Arc<CheckoutDriver>, Arc<Walker>, Arc<Plane>, ViewAccess) {
        std::fs::write(root.join("file.rs"), b"strict content").unwrap();
        let registry = super::super::registry::FamilyRegistry::open(storage, "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", root)
            .unwrap();
        let walker = Arc::new(Walker {
            fail: AtomicBool::new(false),
            delete_after_listing: AtomicBool::new(false),
        });
        let plane = Arc::new(Plane {
            fail: AtomicBool::new(false),
        });
        let producers = Producers {
            trigram: crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            }
            .fingerprint_hex(),
            semantic: None,
            callgraph: "extractor".into(),
        };
        let driver = Arc::new(CheckoutDriver::new(
            owner.clone(),
            producers,
            None,
            walker.clone(),
            vec![plane.clone()],
        ));
        (driver, walker, plane, ViewAccess::Owner(owner))
    }

    struct RetirePauseAdapter {
        armed: AtomicBool,
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    struct PublicationOrderAdapter {
        owner: super::super::registry::ViewRegistration,
        producer: String,
        events: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl PlaneAdapter for PublicationOrderAdapter {
        fn plane(&self) -> FamilyPlane {
            FamilyPlane::Trigram
        }
        fn producer(&self) -> String {
            self.producer.clone()
        }
        fn applies_to(&self, _: &RelPath) -> bool {
            true
        }
        fn open_generation(
            &self,
            access: &ViewAccess,
            generation: &Arc<ComposedGeneration>,
        ) -> Result<(), PlaneError> {
            self.open_reader(access, generation).map(|_| ())
        }
        fn open_reader(
            &self,
            _: &ViewAccess,
            generation: &Arc<ComposedGeneration>,
        ) -> Result<Option<PlaneAttachment>, PlaneError> {
            let view = self
                .owner
                .view_store()
                .map_err(|e| SiblingLoader::error(e.to_string()))?;
            let manifest = view
                .load_manifest_v2(generation.name())
                .map_err(|e| SiblingLoader::error(e.to_string()))?;
            let store = self
                .owner
                .open_store(FamilyPlane::Trigram)
                .map_err(|e| SiblingLoader::error(e.to_string()))?;
            let reader = store.reader();
            let keys: Vec<_> = manifest.ready_keys().collect();
            assert!(
                !keys.is_empty(),
                "the real builder must have materialized a blob"
            );
            for key in keys {
                assert!(reader.get(&key).unwrap().is_some());
            }
            self.events.lock().unwrap().push("reader-open");
            Ok(Some(Arc::new(std::sync::Mutex::new(reader))))
        }
        fn release_generation(&self, _: &ViewAccess, _: &str) {}
        fn readiness(
            &self,
            _: &ViewAccess,
            _: &Snapshot,
        ) -> super::super::readiness::PlaneReadiness {
            super::super::readiness::PlaneReadiness::Ready {
                pending: 0,
                failed: 0,
            }
        }
    }

    struct PublicationOrderFence {
        driver: std::sync::Weak<CheckoutDriver>,
        store: super::super::ViewStore,
        events: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl PublishFence for PublicationOrderFence {
        fn publish(
            &self,
            commit: &mut dyn FnMut() -> Result<(), PlaneError>,
        ) -> Result<(), PlaneError> {
            assert_eq!(*self.events.lock().unwrap(), ["reader-open"]);
            let driver = self.driver.upgrade().unwrap();
            assert_eq!(driver.installed_bundle().composed.name(), "empty");
            assert!(self.store.current_generation_read_only().unwrap().is_none());
            commit()?;
            assert!(self.store.current_generation_read_only().unwrap().is_some());
            assert_eq!(
                driver.installed_bundle().composed.name(),
                "empty",
                "pointer publication must not pretend installation happened"
            );
            self.events.lock().unwrap().push("pointer-published");
            Ok(())
        }
    }

    struct FailAfterLandedCas;
    impl PublishFence for FailAfterLandedCas {
        fn publish(
            &self,
            commit: &mut dyn FnMut() -> Result<(), PlaneError>,
        ) -> Result<(), PlaneError> {
            commit()?;
            Err(SiblingLoader::error("failure after durable CAS"))
        }
    }

    #[test]
    fn real_checkout_post_cas_error_preserves_publication_and_requires_revalidation() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        driver.set_publish_fence(Arc::new(FailAfterLandedCas));
        let error = SiblingLoader::new(driver.clone(), vec![])
            .load(&access)
            .unwrap_err();
        assert!(
            error.reason.contains("publication may have landed"),
            "{error:?}"
        );
        assert!(driver.take_revalidated_job());
        assert_eq!(driver.installed_bundle().composed.name(), "empty");
        let store = driver.owner.view_store().unwrap();
        let published = store.current_generation_read_only().unwrap().unwrap();
        let manifest = store.load_manifest_v2(&published).unwrap();
        let blobs = driver.owner.open_store(FamilyPlane::Trigram).unwrap();
        for key in manifest.ready_keys() {
            assert!(blobs.get(&key).unwrap().is_some());
        }
        driver.set_publish_fence(Arc::new(AcceptPublication));
        drop(
            SiblingLoader::new(driver.clone(), vec![])
                .load(&access)
                .unwrap(),
        );
        assert_ne!(driver.installed_bundle().composed.name(), "empty");
    }

    struct AcceptPublication;
    impl PublishFence for AcceptPublication {
        fn publish(
            &self,
            commit: &mut dyn FnMut() -> Result<(), PlaneError>,
        ) -> Result<(), PlaneError> {
            commit()
        }
    }

    #[test]
    fn real_checkout_publication_opens_blob_readers_before_cas_and_installs_after() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let owner = driver.owner.clone();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let adapter = Arc::new(PublicationOrderAdapter {
            owner: owner.clone(),
            producer: driver.producers.trigram.clone(),
            events: events.clone(),
        });
        let driver = Arc::new(
            Arc::try_unwrap(driver)
                .ok()
                .unwrap()
                .with_adapters(vec![adapter.clone()]),
        );
        let store = owner.view_store().unwrap();
        driver.set_publish_fence(Arc::new(PublicationOrderFence {
            driver: Arc::downgrade(&driver),
            store: store.clone(),
            events: events.clone(),
        }));
        let outcome = SiblingLoader::new(driver.clone(), vec![])
            .load(&access)
            .unwrap();
        let published = store.current_generation_read_only().unwrap().unwrap();
        assert_eq!(outcome.snapshot.generation().name(), published);
        assert_eq!(driver.installed_bundle().composed.name(), published);
        assert!(outcome
            .snapshot
            .readers()
            .unwrap()
            .resource(
                Arc::as_ptr(&adapter) as usize,
                access.family(),
                access.scope(),
                &outcome.snapshot.generation().resident_key()
            )
            .is_some());
        assert_eq!(
            *events.lock().unwrap(),
            ["reader-open", "pointer-published"]
        );
    }

    impl PlaneAdapter for RetirePauseAdapter {
        fn plane(&self) -> FamilyPlane {
            FamilyPlane::Trigram
        }
        fn producer(&self) -> String {
            "test".into()
        }
        fn applies_to(&self, _: &RelPath) -> bool {
            true
        }
        fn open_generation(
            &self,
            _: &ViewAccess,
            _: &Arc<ComposedGeneration>,
        ) -> Result<(), PlaneError> {
            Ok(())
        }
        fn release_generation(&self, _: &ViewAccess, _: &str) {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.send(()).unwrap();
                self.resume
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
        }
        fn readiness(
            &self,
            _: &ViewAccess,
            _: &Snapshot,
        ) -> super::super::readiness::PlaneReadiness {
            super::super::readiness::PlaneReadiness::Ready {
                pending: 0,
                failed: 0,
            }
        }
    }

    #[test]
    fn installed_bundle_retirement_releases_live_and_installed_locks() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let (entered, arrived) = std::sync::mpsc::channel();
        let (resume, released) = std::sync::mpsc::channel();
        let adapter = Arc::new(RetirePauseAdapter {
            armed: false.into(),
            entered,
            resume: std::sync::Mutex::new(released),
        });
        let driver = Arc::new(
            Arc::try_unwrap(driver)
                .ok()
                .unwrap()
                .with_adapters(vec![adapter.clone()]),
        );
        drop(
            SiblingLoader::new(driver.clone(), vec![])
                .load(&access)
                .unwrap(),
        );
        let (previous, revision) = driver.installed_cut();
        let cut = previous.install_cut().unwrap().clone();
        let generation = Arc::new(ComposedGeneration::new(
            "replacement",
            previous.generation().flatten().unwrap(),
            None,
        ));
        let successor = super::super::snapshot::LiveDelta::new(generation)
            .snapshot()
            .with_install_cut(cut);
        drop(previous);
        adapter.armed.store(true, Ordering::SeqCst);
        let installing = driver.clone();
        let worker = std::thread::spawn(move || installing.install(&access, &successor, revision));
        arrived
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let live_unlocked = driver.live.try_lock().is_ok();
        let installed_unlocked = driver.installed.try_lock().is_ok();
        resume.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(
            live_unlocked,
            "reader retirement ran while the live lock was held"
        );
        assert!(
            installed_unlocked,
            "reader retirement ran while the installed lock was held"
        );
    }

    #[test]
    fn install_never_promotes_overflowed_or_stopped_watchers() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        drop(
            SiblingLoader::new(driver.clone(), vec![])
                .load(&access)
                .unwrap(),
        );
        for watcher in [
            super::super::snapshot::WatcherState::Overflowed,
            super::super::snapshot::WatcherState::Stopped,
        ] {
            driver.live.lock().unwrap().set_watcher(watcher);
            driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
            let (snapshot, revision) = driver.installed_cut();
            assert!(driver.install(&access, &snapshot, revision).is_err());
            assert_eq!(driver.live.lock().unwrap().watcher(), watcher);
        }
    }

    #[test]
    fn initial_reconciling_state_requires_a_completed_strict_walk() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        assert!(root.path().join("file.rs").is_file());
        let (snapshot, revision) = driver.installed_cut();
        assert!(snapshot.membership().is_empty());
        assert!(driver.install(&access, &snapshot, revision).is_err());
        assert_eq!(
            driver.live.lock().unwrap().watcher(),
            super::super::snapshot::WatcherState::Reconciling
        );
    }

    #[test]
    fn rejected_install_does_not_consume_prepared_readers() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        drop(
            SiblingLoader::new(driver.clone(), vec![])
                .load(&access)
                .unwrap(),
        );
        let (current, revision) = driver.installed_cut();
        let seq = driver.installed_bundle().seq;
        let mut cut = current.install_cut().unwrap().clone();
        cut.seq = cut.seq.saturating_sub(1);
        let generation = driver
            .build_own_generation(&access, &current, false)
            .unwrap();
        let readers = driver
            .prepared_readers
            .lock()
            .unwrap()
            .get(generation.name())
            .unwrap()
            .readers
            .clone();
        let snapshot = super::super::snapshot::LiveDelta::new(generation.clone())
            .snapshot()
            .with_install_cut(cut);
        assert!(driver.install(&access, &snapshot, revision).is_err());
        let retained = driver
            .prepared_readers
            .lock()
            .unwrap()
            .get(generation.name())
            .unwrap()
            .readers
            .clone();
        assert!(Arc::ptr_eq(&retained, &readers));
        assert_eq!(driver.installed_bundle().seq, seq);
        assert!(driver.take_revalidated_job());
        assert_eq!(
            driver
                .owner
                .view_store()
                .unwrap()
                .current_generation_read_only()
                .unwrap()
                .as_deref(),
            Some(generation.name())
        );
    }

    #[test]
    fn superseded_cut_is_refused_before_its_pointer_can_publish() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        drop(loader.load(&access).unwrap());
        let store = driver.owner.view_store().unwrap();
        let before = store.current_generation_read_only().unwrap();
        std::fs::write(root.path().join("file.rs"), b"candidate B").unwrap();
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        let (candidate, revision) = loader
            .reconciled(&access, driver.installed_snapshot().generation().clone())
            .unwrap();
        driver.install(&access, &candidate, revision).unwrap();
        assert!(loader.fold(&access, &candidate, revision).is_err());
        assert_eq!(store.current_generation_read_only().unwrap(), before);
    }

    #[test]
    fn a_foreign_pointer_after_the_walk_cut_cannot_become_its_cas_base() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        drop(loader.load(&access).unwrap());
        let store = driver.owner.view_store().unwrap();
        let before = store.current_generation_read_only().unwrap();
        std::fs::write(root.path().join("file.rs"), b"candidate B").unwrap();
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        let (candidate, revision) = loader
            .reconciled(&access, driver.installed_snapshot().generation().clone())
            .unwrap();
        assert_eq!(candidate.publication_base(), Some(&before));
        let mut foreign = driver.installed_snapshot().generation().flatten().unwrap();
        foreign.header_mut().ignore_fingerprint = Some("foreign-winner".into());
        let name = super::super::manifest_v2::GenerationName::for_manifest(&foreign).unwrap();
        let prepared = store
            .prepare_v2(&name, before.as_deref(), &foreign, None)
            .unwrap();
        assert_eq!(
            store.commit_v2(prepared, None).unwrap(),
            super::super::manifest_v2::PublishV2::Published
        );
        let error = loader.fold(&access, &candidate, revision).unwrap_err();
        assert!(error.reason.contains("publication lost"), "{error:?}");
        assert_eq!(
            store.current_generation_read_only().unwrap(),
            Some(name.to_string())
        );
        assert!(driver.take_revalidated_job());
    }

    #[test]
    fn strict_walk_preserves_pre_cut_readiness_when_disk_equals_the_base() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        drop(loader.load(&access).unwrap());
        let file = RelPath::new(b"file.rs".to_vec()).unwrap();
        driver.live.lock().unwrap().set_readiness(
            file.clone(),
            super::super::snapshot::PlanePatch {
                content: crate::blob_store::v2::ContentHash::of(b"strict content"),
                planes: EntryPlanes {
                    trigram: Some(super::super::readiness::PlaneState::pending(
                        "checkout repair",
                    )),
                    ..Default::default()
                },
            },
        );
        driver.record_change(file.clone());
        let (walk, _) = loader
            .reconciled(&access, driver.installed_snapshot().generation().clone())
            .unwrap();
        assert_eq!(
            walk.readiness_patches().get(&file).unwrap().planes.trigram,
            Some(super::super::readiness::PlaneState::pending(
                "checkout repair"
            ))
        );
    }

    #[test]
    fn coverage_does_not_invent_work_for_unregistered_planes() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        drop(
            SiblingLoader::new(driver.clone(), vec![])
                .load(&access)
                .unwrap(),
        );
        let (current, revision) = driver.installed_cut();
        std::fs::write(root.path().join("file.rs"), b"new bytes").unwrap();
        let mut delta = super::super::snapshot::LiveDelta::new(current.generation().clone());
        delta.apply(
            RelPath::new(b"file.rs".to_vec()).unwrap(),
            LiveEntry::new(DiskState::of_bytes(b"new bytes"), 1),
        );
        let snapshot = delta
            .snapshot()
            .with_install_cut(current.install_cut().unwrap().clone());
        driver.install(&access, &snapshot, revision).unwrap();
        let installed = driver.installed_bundle();
        for plane in [FamilyPlane::Semantic, FamilyPlane::Callgraph] {
            assert_eq!(
                installed.coverage[&plane],
                super::super::readiness::PlaneReadiness::Ready {
                    pending: 0,
                    failed: 0
                }
            );
        }
    }

    #[test]
    fn directory_uncertainty_survives_non_strict_install_until_a_full_walk() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        drop(loader.load(&access).unwrap());
        let directory = root.path().join("new-directory");
        std::fs::create_dir(&directory).unwrap();
        super::super::intent::WriteIntentListener::record_change(
            &*driver,
            &super::super::intent::WriteIntent::Directory(directory.clone()),
            super::super::intent::WritePhase::Before,
        );
        let (snapshot, revision) = driver.installed_cut();
        assert!(driver.install(&access, &snapshot, revision).is_err());
        assert!(driver
            .installed_bundle()
            .uncertain_paths
            .contains(&directory));
        drop(
            loader
                .refresh(&access, snapshot.generation().clone())
                .unwrap(),
        );
        assert!(driver.installed_bundle().uncertain_paths.is_empty());
    }
    #[test]
    fn composite_reconcile_omits_file_deleted_after_listing() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, walker, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        let before = loader.load(&access).unwrap();
        assert_eq!(before.snapshot.membership().len(), 1);
        walker.delete_after_listing.store(true, Ordering::SeqCst);
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        let after = loader.load(&access).unwrap();
        assert!(after.snapshot.membership().is_empty());
        assert!(after.pending_planes.is_empty());
        assert_ne!(
            before.snapshot.generation().name(),
            after.snapshot.generation().name()
        );
    }

    #[test]
    fn composite_driver_hashes_same_bytes_installs_own_generation_and_reports_intent() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let outcome = SiblingLoader::new(driver.clone(), vec![])
            .load(&access)
            .unwrap();
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        assert_eq!(
            outcome.snapshot.disk_state(&path),
            DiskState::of_bytes(b"strict content")
        );
        assert!(outcome.snapshot.generation().name().starts_with("g2"));
        assert!(driver
            .installed_state(&access, FamilyPlane::Trigram)
            .1
            .is_empty());
        driver.record_change(path);
        let waiter = super::super::query_wait::BoundedQueryWait::new(driver);
        match waiter.wait_for(&access, FamilyPlane::Trigram, std::time::Duration::ZERO) {
            WaitOutcome::TimedOut { unreflected, .. } => {
                assert_eq!(unreflected, vec![root.path().join("file.rs")])
            }
            _ => panic!("acknowledged edit disappeared from readiness"),
        }
    }
    #[test]
    fn composite_walk_failure_never_installs_partial_membership() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, walker, _, access) = fixture(root.path(), storage.path());
        walker.fail.store(true, Ordering::SeqCst);
        let error = SiblingLoader::new(driver.clone(), vec![])
            .load(&access)
            .unwrap_err();
        assert!(error.reason.contains("blocked.rs"));
        assert_eq!(
            driver
                .installed_state(&access, FamilyPlane::Trigram)
                .0
                .generation()
                .name(),
            "empty"
        );
        assert!(driver
            .owner
            .view_store()
            .unwrap()
            .current_generation()
            .unwrap()
            .is_none());
    }
    #[test]
    fn composite_materializer_refusal_preserves_named_gap_and_no_pointer() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, plane, access) = fixture(root.path(), storage.path());
        plane.fail.store(true, Ordering::SeqCst);
        let error = SiblingLoader::new(driver.clone(), vec![])
            .load(&access)
            .unwrap_err();
        assert!(error.reason.contains("file.rs"));
        assert_eq!(
            driver.installed_state(&access, FamilyPlane::Trigram).1,
            vec![root.path().join("file.rs")]
        );
        assert!(driver
            .owner
            .view_store()
            .unwrap()
            .current_generation()
            .unwrap()
            .is_none());
    }
    #[test]
    fn composite_install_epoch_race_refuses_obsolete_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let checkout = driver.reconcile(&access).unwrap();
        let snapshot = driver.installed_state(&access, FamilyPlane::Trigram).0;
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        driver.live.lock().unwrap().bump_epoch();
        let error = driver
            .install(&access, &snapshot, checkout.revision)
            .unwrap_err();
        assert!(error.reason.contains("EpochChanged"));
        assert_eq!(
            driver.installed_state(&access, FamilyPlane::Trigram).1,
            vec![root.path().join("file.rs")]
        );
    }

    struct EditingFence {
        driver: std::sync::Weak<CheckoutDriver>,
    }
    impl PublishFence for EditingFence {
        fn publish(
            &self,
            commit: &mut dyn FnMut() -> Result<(), PlaneError>,
        ) -> Result<(), PlaneError> {
            let driver = self.driver.upgrade().unwrap();
            std::fs::write(
                driver.owner.root().join("file.rs"),
                b"delivered during publication",
            )
            .unwrap();
            driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
            commit()
        }
    }

    #[test]
    fn sustained_writes_install_without_a_quiet_period_and_keep_gaps() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        loader.load(&access).unwrap();
        driver.set_publish_fence(Arc::new(EditingFence {
            driver: Arc::downgrade(&driver),
        }));
        for sequence in 0..32 {
            let (snapshot, revision) = driver.installed_cut();
            let seq = driver.installed_bundle().seq;
            std::fs::write(
                root.path().join("file.rs"),
                format!("edit before fold {sequence}"),
            )
            .unwrap();
            driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
            let installed = loader.fold(&access, &snapshot, revision).unwrap();
            assert_eq!(driver.installed_bundle().seq, seq + 1);
            assert_eq!(
                driver.installed_bundle().coverage[&FamilyPlane::Trigram],
                super::super::readiness::PlaneReadiness::Building
            );
            assert_eq!(installed.pending_intent().count(), 1);
            assert_eq!(
                driver.installed_state(&access, FamilyPlane::Trigram).1,
                vec![root.path().join("file.rs")]
            );
        }
        eprintln!("32/32 installs: edits after every cut, before fold, and inside every publication fence; no quiet-period retry");
    }

    #[test]
    fn install_refuses_a_superseded_bundle_even_with_equal_base_bytes() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let loader = SiblingLoader::new(driver.clone(), vec![]);
        loader.load(&access).unwrap();
        let (snapshot, revision) = driver.installed_cut();
        driver.install(&access, &snapshot, revision).unwrap();
        let current = driver.installed_bundle().seq;
        let error = driver.install(&access, &snapshot, revision).unwrap_err();
        assert!(error.reason.contains("BaseChanged"));
        assert_eq!(driver.installed_bundle().seq, current);
    }

    struct RacingPublisher {
        driver: std::sync::Weak<CheckoutDriver>,
        equivalent: bool,
    }
    impl PublishFence for RacingPublisher {
        fn publish(
            &self,
            commit: &mut dyn FnMut() -> Result<(), PlaneError>,
        ) -> Result<(), PlaneError> {
            let driver = self.driver.upgrade().unwrap();
            let store = driver.owner.view_store().unwrap();
            let current = store.current_generation().unwrap();
            let candidate = std::fs::read_dir(driver.owner.view_dir())
                .unwrap()
                .filter_map(|entry| {
                    let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                    name.strip_prefix("manifest-")
                        .and_then(|name| name.strip_suffix(".json"))
                        .map(str::to_owned)
                })
                .find(|name| Some(name) != current.as_ref())
                .unwrap();
            let mut manifest = store.load_manifest_v2(&candidate).unwrap();
            if !self.equivalent {
                manifest.header_mut().head_tree = Some("foreign proof".into());
            }
            let name = super::super::manifest_v2::GenerationName::for_manifest(&manifest).unwrap();
            let prepared = store
                .prepare_v2(&name, current.as_deref(), &manifest, None)
                .unwrap();
            assert_eq!(
                store.commit_v2(prepared, None).unwrap(),
                super::super::manifest_v2::PublishV2::Published
            );
            commit()
        }
    }

    #[test]
    fn a_base_referencing_worker_publishes_a_sparse_v3_generation_readable_after_restart() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        // A v3 base over the checkout's files as they are now.
        std::fs::write(root.path().join("kept.rs"), b"kept content").unwrap();
        std::fs::write(root.path().join("deleted.rs"), b"deleted content").unwrap();
        let paths = ["deleted.rs", "file.rs", "kept.rs"]
            .map(|name| RelPath::new(name.as_bytes().to_vec()).unwrap());
        let super::super::base_builder::BaseBuild::Published { identity, .. } =
            super::super::base_builder::build_base(
                super::super::base_builder::BaseBuildInput {
                    storage: storage.path(),
                    family: "family",
                    root: root.path(),
                    paths: &paths,
                    policy: crate::blob_store::v2::TrigramPolicy {
                        max_file_size: 1 << 20,
                    },
                    semantic_producer: None,
                    callgraph_producer: "extractor".into(),
                    ignore_fingerprint: None,
                    history: None,
                    budget: super::super::eviction::DiskBudget::default(),
                },
                None,
            )
            .unwrap()
        else {
            panic!("the only builder publishes");
        };
        let base = super::super::base_refs::resolve_base(storage.path(), "family", &identity.scope)
            .unwrap();
        // An installed worker generation that inherits the whole base; the
        // header names the base only by its identity strings.
        let worker = super::super::manifest_v2::ManifestV2::new_v3(
            base.manifest().header().clone(),
            Some(base.header()),
            Default::default(),
        );
        let composed = Arc::new(
            ComposedGeneration::compose(
                "installed-worker",
                worker,
                base.into_generation(storage.path(), "family"),
                None,
            )
            .unwrap(),
        );
        // The checkout then edits one file, adds one and deletes one.
        let mut delta = LiveDelta::new(composed);
        let rel = |name: &str| RelPath::new(name.as_bytes().to_vec()).unwrap();
        std::fs::write(root.path().join("file.rs"), b"edited content").unwrap();
        std::fs::write(root.path().join("added.rs"), b"added content").unwrap();
        std::fs::remove_file(root.path().join("deleted.rs")).unwrap();
        delta.apply(
            rel("file.rs"),
            LiveEntry::new(
                super::super::snapshot::DiskState::of_bytes(b"edited content"),
                1,
            ),
        );
        delta.apply(
            rel("added.rs"),
            LiveEntry::new(
                super::super::snapshot::DiskState::of_bytes(b"added content"),
                2,
            ),
        );
        delta.apply(
            rel("deleted.rs"),
            LiveEntry::new(super::super::snapshot::DiskState::Absent, 3),
        );
        // A path rewritten with its base bytes inherits, rather than copies.
        delta.apply(
            rel("kept.rs"),
            LiveEntry::new(
                super::super::snapshot::DiskState::of_bytes(b"kept content"),
                4,
            ),
        );
        let snapshot = delta.snapshot();
        // The install check needs an identity recorded by the driver. Reconcile
        // and install here; the name `installed-worker` alone is not that proof.
        let (reconciled, revision) = SiblingLoader::reconciled_snapshot(
            driver.as_ref(),
            &access,
            snapshot.generation().clone(),
        )
        .unwrap();
        driver.install(&access, &reconciled, revision).unwrap();
        let snapshot =
            snapshot.with_install_cut(driver.installed_snapshot().install_cut().unwrap().clone());
        let published = driver
            .build_own_generation(&access, &snapshot, false)
            .unwrap();
        let generation = published.name().to_owned();
        assert!(published.v3_route().is_some());
        drop(published);
        // No v2 pointer moved and nothing went outside the v3 namespace.
        assert!(driver
            .owner
            .view_store()
            .unwrap()
            .current_generation()
            .unwrap()
            .is_none());
        assert!(!storage.path().join("views/v3").exists());
        let member = crate::blob_store::v3::view_dir(storage.path(), "family", "local").unwrap();
        let store = super::super::ViewStore::existing_dir(member).unwrap();
        assert_eq!(
            store.current_generation_read_only().unwrap().as_deref(),
            Some(generation.as_str())
        );
        let manifest = store.load_manifest_v2(&generation).unwrap();
        let header = manifest.base_header().unwrap();
        assert_eq!(header.scope, identity.scope);
        identity.check_header(header).unwrap();
        // Sparse: only the changed paths, never the inherited base entries.
        let entries: Vec<_> = manifest.entries().map(|(path, _)| path.clone()).collect();
        assert_eq!(entries, vec![rel("added.rs"), rel("file.rs")]);
        assert_eq!(
            manifest.tombstones().iter().cloned().collect::<Vec<_>>(),
            vec![rel("deleted.rs")]
        );
        assert!(manifest.readiness_overrides().is_empty());
        // With the driver gone, a fresh reader composes the published delta
        // over the authenticated base.
        drop(driver);
        let reader = super::super::base_refs::open_foreign_v3(
            storage.path(),
            "family",
            "local",
            &manifest.header().producers,
        )
        .unwrap()
        .unwrap();
        assert_eq!(reader.name(), generation);
        assert!(reader.entry(&rel("deleted.rs")).is_none());
        assert_eq!(
            reader
                .entry(&rel("kept.rs"))
                .and_then(|entry| entry.content()),
            Some(crate::blob_store::v2::ContentHash::of(b"kept content"))
        );
        assert_eq!(
            reader
                .entry(&rel("file.rs"))
                .and_then(|entry| entry.content()),
            Some(crate::blob_store::v2::ContentHash::of(b"edited content"))
        );
        assert!(reader.entry(&rel("added.rs")).is_some());
    }

    #[test]
    fn a_worker_header_naming_an_unknown_base_is_refused_before_any_write() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let base = driver.installed_snapshot().generation().clone();
        let worker = super::super::manifest_v2::ManifestV2::new_v3(
            base.header().clone(),
            Some(super::super::manifest_v2::BaseHeader {
                scope: super::super::base_refs::base_scope_for(&[1; 32]),
                membership_digest: "11".repeat(32),
                identity_digest: "22".repeat(32),
                bound_generation: base.name().into(),
            }),
            Default::default(),
        );
        let composed = Arc::new(ComposedGeneration::compose("worker", worker, base, None).unwrap());
        let snapshot = LiveDelta::new(composed).snapshot();
        let store = driver.owner.view_store().unwrap();
        let error = driver
            .build_own_generation(&access, &snapshot, false)
            .unwrap_err();
        assert!(error.reason.contains("base unavailable"), "{error:?}");
        assert!(store.current_generation().unwrap().is_none());
        assert!(!storage.path().join("blobs/v3/family/views/local").exists());
    }

    #[test]
    fn actual_cas_losers_never_install_foreign_verification_and_request_fresh_jobs() {
        for equivalent in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let (driver, _, _, access) = fixture(root.path(), storage.path());
            let loader = SiblingLoader::new(driver.clone(), vec![]);
            loader.load(&access).unwrap();
            let before = driver.installed_bundle();
            driver.set_publish_fence(Arc::new(RacingPublisher {
                driver: Arc::downgrade(&driver),
                equivalent,
            }));
            let (snapshot, revision) = driver.installed_cut();
            let error = loader.fold(&access, &snapshot, revision).unwrap_err();
            assert!(error.reason.contains("publication lost"));
            assert!(
                error.reason.contains(if equivalent {
                    "EquivalentWinner"
                } else {
                    "Conflict"
                }),
                "{error:?}"
            );
            assert!(driver.take_revalidated_job());
            assert_eq!(driver.installed_bundle().seq, before.seq);
            assert_eq!(
                driver.installed_snapshot().generation().name(),
                before.composed.name()
            );
            assert_ne!(
                driver
                    .owner
                    .view_store()
                    .unwrap()
                    .current_generation()
                    .unwrap()
                    .as_deref(),
                Some(before.composed.name())
            );
        }
    }
}

/// Bridges the ruled callgraph extractor/materializer into the composite loader.
/// Pass `adapter` to `CheckoutDriver::with_adapters`; use `callgraph::PRODUCER`
/// in the driver's producer header. No global registration occurs here.
#[derive(Default)]
pub struct CallgraphBridge {
    pub adapter: Arc<super::callgraph::CallgraphPlane>,
    /// Source files this bridge parsed for call-graph evidence. Unchanged
    /// files whose blob already exists reuse it and are not counted.
    extractions: std::sync::atomic::AtomicUsize,
}

impl CallgraphBridge {
    pub fn extraction_count(&self) -> usize {
        self.extractions.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The sealed donor of `base`'s derived graph, or why there is none.
fn sealed_donor(base: &ComposedGeneration) -> Result<std::path::PathBuf, super::donor::ColdReason> {
    use super::donor::ColdReason;
    let route = base.v3_route().ok_or(ColdReason::NoSealedDonor)?;
    let donor = super::resolve_derived_path(&route.member_dir(), base.name())
        .map_err(|_| ColdReason::NoSealedDonor)?;
    if !super::donor::is_sealed(&donor) {
        return Err(ColdReason::NoSealedDonor);
    }
    super::donor::verify(&donor).map_err(ColdReason::SealInvalid)?;
    Ok(donor)
}

fn remove_private_database(database: &std::path::Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut name = database.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(name));
    }
}

impl CompositePlane for CallgraphBridge {
    fn completes_fresh_base(&self) -> bool {
        true
    }

    fn complete_v3_base(
        &self,
        claim: &super::base_builder::BuildClaim,
        observed: &BTreeMap<RelPath, LiveEntry>,
    ) -> Result<(), PlaneError> {
        let error = |reason: String| PlaneError {
            plane: Some(FamilyPlane::Callgraph),
            reason,
        };
        let base = super::base_refs::resolve_base(claim.storage(), claim.family(), claim.scope())
            .map_err(|e| error(e.to_string()))?;
        let route = super::base_refs::route(claim.storage(), claim.family(), claim.scope());
        let mut keys = Vec::new();
        let mut payloads = Vec::new();
        for (path, entry) in base.manifest().entries() {
            if !entry
                .plane_state(FamilyPlane::Callgraph)
                .is_some_and(super::readiness::PlaneState::is_pending)
            {
                continue;
            }
            // The base was read independently of the reconciliation; only
            // evidence for the very bytes the base names may complete it.
            let observation = observed.get(path).filter(|live| {
                matches!(
                    (live.disk, entry),
                    (
                        super::snapshot::DiskState::Present { content, size },
                        super::manifest_v2::EntryV2::Regular { content: c, size: s, .. },
                    ) if content == *c && size == *s
                )
            });
            let attachment = observation
                .and_then(|live| live.attachments.get(&FamilyPlane::Callgraph))
                .ok_or_else(|| {
                    error(format!(
                        "{path:?}: base member changed before its graph was completed"
                    ))
                })?;
            if let Some(cached) = attachment.downcast_ref::<CachedAttachment>() {
                keys.push((path.clone(), cached.0));
            } else if let Some(attachment) =
                attachment.downcast_ref::<super::callgraph::CallgraphAttachment>()
            {
                keys.push((path.clone(), attachment.key));
                payloads.push((
                    attachment.key,
                    attachment
                        .blob
                        .to_bytes()
                        .map_err(|e| error(e.to_string()))?,
                ));
            } else {
                return Err(error(format!("{path:?}: callgraph attachment missing")));
            }
        }
        drop(base);
        let mut live =
            crate::pins::LivePin::create_v3(claim.storage(), claim.family(), claim.scope())
                .map_err(|e| error(e.to_string()))?;
        live.protect(&keys.iter().map(|(_, key)| *key).collect::<Vec<_>>())
            .map_err(|e| error(e.to_string()))?;
        if !payloads.is_empty() {
            let request = payloads
                .iter()
                .map(|(_, bytes)| bytes.len() as u64)
                .sum::<u64>()
                .saturating_mul(2)
                .saturating_add(1 << 20);
            let schema = FamilyPlane::Callgraph.payload_schema();
            crate::blob_store::v3::publish(claim.storage(), claim.family(), request, None, |p| {
                for (key, bytes) in &payloads {
                    p.put(key, bytes, schema)?;
                }
                Ok(())
            })
            .map_err(|e| error(e.to_string()))?;
        }
        for (_, key) in &keys {
            if !v3_contains(&route, key) {
                return Err(error(
                    "base callgraph row disappeared before publication".into(),
                ));
            }
        }
        super::base_builder::publish_successor(
            claim,
            |manifest| {
                for (path, key) in &keys {
                    if let Some(super::manifest_v2::EntryV2::Regular { planes, .. }) =
                        manifest.get_mut(path)
                    {
                        planes.callgraph = Some(super::readiness::PlaneState::ready(key));
                    }
                }
            },
            None,
        )
        .map_err(|e| error(e.to_string()))?;
        drop(live);
        Ok(())
    }

    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Callgraph
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        use super::contracts::PlaneAdapter;
        self.adapter.applies_to(path)
    }
    fn key(
        &self,
        path: &RelPath,
        content: crate::blob_store::v2::ContentHash,
    ) -> Option<crate::blob_store::v2::FamilyKey> {
        let language = if super::assembly::is_resolution_input(path.as_bytes()) {
            "config".to_owned()
        } else {
            let path = std::str::from_utf8(path.as_bytes()).ok()?;
            format!(
                "{:?}",
                crate::parser::detect_language(std::path::Path::new(path))?
            )
            .to_lowercase()
        };
        Some(crate::blob_store::v2::FamilyKey::from(
            &crate::blob_store::CallgraphKey::from_source_digest(
                *content.as_bytes(),
                language,
                super::callgraph::PRODUCER,
            )
            .full_key(),
        ))
    }
    fn attachment(
        &self,
        path: &RelPath,
        bytes: &[u8],
    ) -> Result<super::snapshot::PlaneAttachment, PlaneError> {
        let language = if super::assembly::is_resolution_input(path.as_bytes()) {
            "config".to_string()
        } else {
            let path = std::str::from_utf8(path.as_bytes()).map_err(|error| PlaneError {
                plane: Some(FamilyPlane::Callgraph),
                reason: error.to_string(),
            })?;
            format!(
                "{:?}",
                crate::parser::detect_language(std::path::Path::new(path)).ok_or_else(|| {
                    PlaneError {
                        plane: Some(FamilyPlane::Callgraph),
                        reason: format!("unsupported callgraph member: {path}"),
                    }
                })?
            )
            .to_lowercase()
        };
        let mut entry = LiveEntry::new(super::snapshot::DiskState::of_bytes(bytes), 0);
        self.extractions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let attachment = super::callgraph::attach(&mut entry, bytes, &language)?;
        Ok(Arc::new(attachment))
    }
    fn materialize(
        &self,
        owner: &super::registry::ViewRegistration,
        generation: &str,
        _snapshot: &Snapshot,
        observed: &BTreeMap<RelPath, LiveEntry>,
        manifest: &mut super::manifest_v2::ManifestV2,
        live: &mut crate::pins::LivePin,
        seed_derived: bool,
    ) -> Result<(), PlaneError> {
        let error = |reason: String| PlaneError {
            plane: Some(FamilyPlane::Callgraph),
            reason,
        };
        if seed_derived {
            return Err(error("derived callgraph seed reuse is not enabled".into()));
        }
        let store = owner
            .open_store(FamilyPlane::Callgraph)
            .map_err(|e| error(e.to_string()))?;
        let mut attachments = Vec::new();
        for (path, entry) in observed {
            if entry.disk == super::snapshot::DiskState::Absent || !self.applies_to(path) {
                continue;
            }
            // A concurrent reconcile may have replaced the observation cache.
            // Never label manifest content with a key computed for other bytes.
            if super::snapshot::DiskState::in_generation(manifest, path) != entry.disk {
                continue;
            }
            let attachment = entry
                .attachments
                .get(&FamilyPlane::Callgraph)
                .and_then(|value| value.downcast_ref::<super::callgraph::CallgraphAttachment>())
                .ok_or_else(|| error(format!("{path:?}: callgraph attachment missing")))?;
            attachments.push((path, attachment));
        }
        // Pin every blob key in one batch before the first put: protecting key
        // by key rewrote and fsynced the whole pin file once per file.
        let keys: Vec<_> = attachments
            .iter()
            .map(|(_, attachment)| attachment.key)
            .collect();
        live.protect(&keys).map_err(|e| error(e.to_string()))?;
        for (path, attachment) in attachments {
            let bytes = attachment
                .blob
                .to_bytes()
                .map_err(|e| error(e.to_string()))?;
            store
                .put_or_touch(&attachment.key, &bytes)
                .map_err(|e| error(e.to_string()))?;
            if let Some(super::manifest_v2::EntryV2::Regular {
                planes,
                resolution_input,
                ..
            }) = manifest.get_mut(path)
            {
                planes.callgraph = Some(super::readiness::PlaneState::ready(&attachment.key));
                *resolution_input = super::assembly::is_resolution_input(path.as_bytes());
            }
        }
        // Joining uses only protected immutable blobs and the projected manifest,
        // not a source root or a sibling's derived database.
        let reader = super::callgraph::BlobReader(
            crate::blob_store::v2::FamilyStoreReader::open_existing(
                owner.registry().storage(),
                owner.family(),
                FamilyPlane::Callgraph,
            )
            .map_err(|e| error(e.to_string()))?
            .ok_or_else(|| error("callgraph blob store unavailable".into()))?,
        );
        let database = super::resolve_derived_path(owner.view_dir(), generation)
            .map_err(|e| error(e.to_string()))?;
        super::callgraph::materialize(
            &database,
            &ComposedGeneration::new(generation, manifest.clone(), None),
            &reader,
        )
    }
    fn finish_generation(
        &self,
        owner: &super::registry::ViewRegistration,
        staging: &str,
        published: &str,
    ) -> Result<(), PlaneError> {
        let error = |e: super::ViewError| PlaneError {
            plane: Some(FamilyPlane::Callgraph),
            reason: e.to_string(),
        };
        let staging_path = super::resolve_derived_path(owner.view_dir(), staging).map_err(error)?;
        let (busy, frames, checkpointed) = super::sqlite_full_sync_checkpoint(
            &staging_path,
            "first_load::CallgraphBridge::finish_generation",
        )
        .map_err(error)?;
        if busy != 0 || (frames >= 0 && checkpointed != frames) {
            return Err(PlaneError {
                plane: Some(FamilyPlane::Callgraph),
                reason: "private callgraph checkpoint incomplete".into(),
            });
        }
        let published_path =
            super::resolve_derived_path(owner.view_dir(), published).map_err(error)?;
        if staging_path != published_path {
            if crate::db::file_identity::open_connections(&staging_path) != 0
                || published_path.exists()
            {
                return Err(PlaneError {
                    plane: Some(FamilyPlane::Callgraph),
                    reason: "private callgraph install would replace a live database".into(),
                });
            }
            crate::db::file_identity::guard_replacement(&staging_path, "first-load derived rename");
            std::fs::rename(&staging_path, &published_path).map_err(|e| error(e.into()))?;
        }
        super::sync_directory(owner.view_dir()).map_err(|e| error(e.into()))?;
        Ok(())
    }
    fn materialize_v3(
        &self,
        _owner: &super::registry::ViewRegistration,
        route: &super::snapshot::V3Route,
        staging: &str,
        base: &Arc<ComposedGeneration>,
        observed: &BTreeMap<RelPath, LiveEntry>,
        delta: &mut super::manifest_v2::ManifestV2,
        live: &mut crate::pins::LivePin,
    ) -> Result<(), PlaneError> {
        let error = |reason: String| PlaneError {
            plane: Some(FamilyPlane::Callgraph),
            reason,
        };
        let mut attachments = Vec::new();
        for (path, entry) in observed {
            if entry.disk == super::snapshot::DiskState::Absent || !self.applies_to(path) {
                continue;
            }
            // Only the delta's own entries; inherited base entries are the
            // base's work, and their keys are the base's.
            if delta.get(path).is_none()
                || super::snapshot::DiskState::in_generation(delta, path) != entry.disk
            {
                continue;
            }
            if let Some(cached) = entry
                .attachments
                .get(&FamilyPlane::Callgraph)
                .and_then(|a| a.downcast_ref::<CachedAttachment>())
            {
                live.protect(&[cached.0])
                    .map_err(|e| error(e.to_string()))?;
                if !v3_contains(route, &cached.0) {
                    return Err(error(
                        "cached callgraph row disappeared before protection".into(),
                    ));
                }
                if let Some(super::manifest_v2::EntryV2::Regular {
                    planes,
                    resolution_input,
                    ..
                }) = delta.get_mut(path)
                {
                    planes.callgraph = Some(super::readiness::PlaneState::ready(&cached.0));
                    *resolution_input = super::assembly::is_resolution_input(path.as_bytes());
                }
                continue;
            }
            let attachment = entry
                .attachments
                .get(&FamilyPlane::Callgraph)
                .and_then(|value| value.downcast_ref::<super::callgraph::CallgraphAttachment>())
                .ok_or_else(|| error(format!("{path:?}: callgraph attachment missing")))?;
            attachments.push((path.clone(), attachment.clone()));
        }
        live.protect(&attachments.iter().map(|(_, a)| a.key).collect::<Vec<_>>())
            .map_err(|e| error(e.to_string()))?;
        let mut payloads = Vec::new();
        for (path, attachment) in &attachments {
            payloads.push((
                path.clone(),
                attachment.key,
                attachment
                    .blob
                    .to_bytes()
                    .map_err(|e| error(e.to_string()))?,
            ));
        }
        if !payloads.is_empty() {
            let request = payloads
                .iter()
                .map(|(_, _, bytes)| bytes.len() as u64)
                .sum::<u64>()
                .saturating_mul(2)
                .saturating_add(1 << 20);
            let schema = FamilyPlane::Callgraph.payload_schema();
            crate::blob_store::v3::publish(&route.storage, &route.family, request, None, |p| {
                for (_, key, bytes) in &payloads {
                    p.put(key, bytes, schema)?;
                }
                Ok(())
            })
            .map_err(|e| error(e.to_string()))?;
        }
        for (path, key, _) in &payloads {
            if let Some(super::manifest_v2::EntryV2::Regular {
                planes,
                resolution_input,
                ..
            }) = delta.get_mut(path)
            {
                planes.callgraph = Some(super::readiness::PlaneState::ready(key));
                *resolution_input = super::assembly::is_resolution_input(path.as_bytes());
            }
        }
        // The derived graph covers the composed generation. While the base's
        // graph is still pending the projection refuses, and the worker's
        // graph stays building until a later publication; nothing is copied.
        let composed = ComposedGeneration::compose(staging, delta.clone(), base.clone(), None)
            .map_err(|e| error(e.to_string()))?;
        if super::callgraph::project_manifest(&composed).is_err() {
            return Ok(());
        }
        let reader = super::callgraph::V3BlobReader(
            crate::blob_store::v3::PlaneReader::open_existing(
                &route.storage,
                &route.family,
                FamilyPlane::Callgraph,
            )
            .map_err(|e| error(e.to_string()))?,
        );
        let dir = route.member_dir();
        // Open (and so initialize and recover) the member's view store before
        // writing a private derived file into it: first-open recovery removes
        // derived files no generation owns. The caller protects `staging` with
        // an assembly pin, as on the v2 path.
        super::ViewStore::open_dir(dir.clone()).map_err(|e| error(e.to_string()))?;
        let database =
            super::resolve_derived_path(&dir, staging).map_err(|e| error(e.to_string()))?;
        // Prefer a private clone of the base's sealed donor, patched with the
        // manifest diff, over rebuilding the whole graph. `staging` is a fresh
        // name, so the clone never reuses another file's name.
        match sealed_donor(base) {
            Ok(donor) => {
                let base_projected = super::callgraph::project_manifest(base)?;
                let projected = super::callgraph::project_manifest(&composed)?;
                let mechanism = super::donor::clone_sealed(&donor, &database).map_err(error)?;
                match super::materialization::apply_manifest_diff_from_blob_reader(
                    &database,
                    &base_projected,
                    &projected,
                    &reader,
                ) {
                    Ok(_) => {
                        super::donor::record(&dir, super::donor::DonorEvent::Cloned(mechanism));
                        return Ok(());
                    }
                    Err(e) if super::materialization::is_base_fingerprint_mismatch(&e) => {
                        remove_private_database(&database);
                        super::donor::record(
                            &dir,
                            super::donor::DonorEvent::Cold(
                                super::donor::ColdReason::BaseFingerprintMismatch,
                            ),
                        );
                    }
                    Err(e) => {
                        remove_private_database(&database);
                        return Err(error(e.to_string()));
                    }
                }
            }
            Err(reason) => {
                super::donor::record(&dir, super::donor::DonorEvent::Cold(reason));
            }
        }
        super::callgraph::materialize(&database, &composed, &reader)
    }
    fn finish_generation_v3(
        &self,
        route: &super::snapshot::V3Route,
        staging: &str,
        published: &str,
    ) -> Result<(), PlaneError> {
        let error = |e: super::ViewError| PlaneError {
            plane: Some(FamilyPlane::Callgraph),
            reason: e.to_string(),
        };
        let dir = route.member_dir();
        let staging_path = super::resolve_derived_path(&dir, staging).map_err(error)?;
        if !staging_path.exists() {
            // The graph was not built for this generation (base graph pending).
            return Ok(());
        }
        let (busy, frames, checkpointed) = super::sqlite_full_sync_checkpoint(
            &staging_path,
            "first_load::CallgraphBridge::finish_generation_v3",
        )
        .map_err(error)?;
        if busy != 0 || (frames >= 0 && checkpointed != frames) {
            return Err(PlaneError {
                plane: Some(FamilyPlane::Callgraph),
                reason: "private callgraph checkpoint incomplete".into(),
            });
        }
        let published_path = super::resolve_derived_path(&dir, published).map_err(error)?;
        if staging_path != published_path {
            if crate::db::file_identity::open_connections(&staging_path) != 0
                || published_path.exists()
            {
                return Err(PlaneError {
                    plane: Some(FamilyPlane::Callgraph),
                    reason: "private callgraph install would replace a live database".into(),
                });
            }
            crate::db::file_identity::guard_replacement(&staging_path, "v3 derived rename");
            std::fs::rename(&staging_path, &published_path).map_err(|e| error(e.into()))?;
        }
        super::sync_directory(&dir).map_err(|e| error(e.into()))?;
        Ok(())
    }
}

#[cfg(test)]
mod callgraph_bridge_tests {
    use super::super::contracts::{PlaneAdapter, PlaneLoader};
    use super::*;

    struct Walker;
    impl MembershipWalker for Walker {
        fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError> {
            Ok(vec![root.join("file.rs")])
        }
    }

    fn graph_driver(
        root: &std::path::Path,
        storage: &std::path::Path,
        bridge: &Arc<CallgraphBridge>,
        plane: Arc<dyn CompositePlane>,
    ) -> (Arc<CheckoutDriver>, ViewAccess) {
        let registry =
            super::super::registry::FamilyRegistry::open(storage, "coherent-graph").unwrap();
        let owner = registry
            .register_view_with_storage(storage, "local", root)
            .unwrap();
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(Walker),
                vec![plane],
            )
            .with_adapters(vec![bridge.adapter.clone()]),
        );
        (driver, ViewAccess::Owner(owner))
    }

    fn graph_key(bytes: &[u8]) -> crate::blob_store::v2::FamilyKey {
        let mut entry = LiveEntry::new(super::super::snapshot::DiskState::of_bytes(bytes), 0);
        super::super::callgraph::attach(&mut entry, bytes, "rust").unwrap();
        entry.attachments[&FamilyPlane::Callgraph]
            .downcast_ref::<super::super::callgraph::CallgraphAttachment>()
            .unwrap()
            .key
    }

    struct RevertAfterAttachment {
        bridge: Arc<CallgraphBridge>,
        driver: std::sync::Mutex<std::sync::Weak<CheckoutDriver>>,
        root: std::path::PathBuf,
        armed: std::sync::atomic::AtomicBool,
    }
    impl CompositePlane for RevertAfterAttachment {
        fn plane(&self) -> FamilyPlane {
            FamilyPlane::Callgraph
        }
        fn applies_to(&self, path: &RelPath) -> bool {
            self.bridge.applies_to(path)
        }
        fn attachment(
            &self,
            path: &RelPath,
            bytes: &[u8],
        ) -> Result<super::super::snapshot::PlaneAttachment, PlaneError> {
            let attachment = self.bridge.attachment(path, bytes)?;
            if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                std::fs::write(
                    self.root.join("file.rs"),
                    b"fn original() {} fn caller() { original(); }",
                )
                .unwrap();
                let driver = self.driver.lock().unwrap().upgrade().unwrap();
                driver.record_change(path.clone());
            }
            Ok(attachment)
        }
        fn materialize(
            &self,
            owner: &super::super::registry::ViewRegistration,
            generation: &str,
            snapshot: &Snapshot,
            observed: &BTreeMap<RelPath, LiveEntry>,
            manifest: &mut super::super::manifest_v2::ManifestV2,
            live: &mut crate::pins::LivePin,
            seed_derived: bool,
        ) -> Result<(), PlaneError> {
            self.bridge.materialize(
                owner,
                generation,
                snapshot,
                observed,
                manifest,
                live,
                seed_derived,
            )
        }
        fn finish_generation(
            &self,
            owner: &super::super::registry::ViewRegistration,
            staging: &str,
            published: &str,
        ) -> Result<(), PlaneError> {
            self.bridge.finish_generation(owner, staging, published)
        }
    }

    #[test]
    fn a_mid_walk_revert_cannot_publish_a_key_for_other_manifest_bytes() {
        let a = b"fn original() {} fn caller() { original(); }";
        let b = b"fn replacement() {} fn caller() { replacement(); }";
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), a).unwrap();
        let bridge = Arc::new(CallgraphBridge::default());
        let plane = Arc::new(RevertAfterAttachment {
            bridge: bridge.clone(),
            driver: Default::default(),
            root: root.path().to_path_buf(),
            armed: false.into(),
        });
        let (driver, access) = graph_driver(root.path(), storage.path(), &bridge, plane.clone());
        *plane.driver.lock().unwrap() = Arc::downgrade(&driver);
        let loader = SiblingLoader::new(driver.clone(), vec![bridge.adapter.clone()]);
        drop(loader.load(&access).unwrap());
        let key_a = graph_key(a);
        let key_b = graph_key(b);
        assert_ne!(key_a, key_b);
        std::fs::write(root.path().join("file.rs"), b).unwrap();
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        plane.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let snapshot = loader
            .refresh(&access, driver.installed_snapshot().generation().clone())
            .unwrap();
        let store = driver.owner.view_store().unwrap();
        let current = store.current_generation_read_only().unwrap().unwrap();
        let manifest = store.load_manifest_v2(&current).unwrap();
        let file = RelPath::new(b"file.rs".to_vec()).unwrap();
        let entry = manifest.get(&file).unwrap();
        let expected = if entry.content() == Some(crate::blob_store::v2::ContentHash::of(a)) {
            key_a
        } else {
            assert_eq!(
                entry.content(),
                Some(crate::blob_store::v2::ContentHash::of(b))
            );
            key_b
        };
        assert_eq!(
            entry.plane_state(FamilyPlane::Callgraph),
            Some(&super::super::readiness::PlaneState::ready(&expected))
        );
        assert!(snapshot.pending_intent().any(|path| path == &file));
        assert_eq!(
            bridge.adapter.readiness(&access, &snapshot),
            super::super::readiness::PlaneReadiness::Building
        );
    }

    #[test]
    fn callgraph_materialization_ignores_an_observation_for_other_content() {
        let a = b"fn original() {} fn caller() { original(); }";
        let b = b"fn replacement() {} fn caller() { replacement(); }";
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), a).unwrap();
        let bridge = Arc::new(CallgraphBridge::default());
        let (driver, access) = graph_driver(root.path(), storage.path(), &bridge, bridge.clone());
        let snapshot = SiblingLoader::new(driver.clone(), vec![bridge.adapter.clone()])
            .load(&access)
            .unwrap()
            .snapshot;
        let mut manifest = snapshot.generation().flatten().unwrap();
        let file = RelPath::new(b"file.rs".to_vec()).unwrap();
        let mut other = LiveEntry::new(super::super::snapshot::DiskState::of_bytes(b), 1);
        other
            .attachments
            .insert(FamilyPlane::Callgraph, bridge.attachment(&file, b).unwrap());
        let observed = BTreeMap::from([(file.clone(), other)]);
        let _assembly = crate::pins::AssemblyPin::create_v2(
            &driver.owner,
            "guard-build",
            &manifest.ready_keys().collect::<Vec<_>>(),
        )
        .unwrap();
        let mut live = crate::pins::LivePin::create(&driver.owner).unwrap();
        bridge
            .materialize(
                &driver.owner,
                "guard-build",
                &snapshot,
                &observed,
                &mut manifest,
                &mut live,
                false,
            )
            .unwrap();
        assert_eq!(
            manifest
                .get(&file)
                .unwrap()
                .plane_state(FamilyPlane::Callgraph),
            Some(&super::super::readiness::PlaneState::ready(&graph_key(a)))
        );
        assert!(driver
            .owner
            .open_store(FamilyPlane::Callgraph)
            .unwrap()
            .get(&graph_key(b))
            .unwrap()
            .is_none());
    }

    struct RejectAfterBuild {
        directory: std::path::PathBuf,
        panic: bool,
    }

    impl PublishFence for RejectAfterBuild {
        fn publish(&self, _: &mut dyn FnMut() -> Result<(), PlaneError>) -> Result<(), PlaneError> {
            let paths = std::fs::read_dir(&self.directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("derived-")
                        && path.extension().is_some_and(|ext| ext == "sqlite")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                paths.len(),
                1,
                "the private build must finish before rejection"
            );
            std::fs::write(
                format!("{}-journal", paths[0].display()),
                b"interrupted journal",
            )
            .unwrap();
            assert!(!self.panic, "injected first-load publication panic");
            Err(SiblingLoader::error(SUPERSEDED_PUBLISH))
        }
    }

    struct Accept;
    impl PublishFence for Accept {
        fn publish(
            &self,
            commit: &mut dyn FnMut() -> Result<(), PlaneError>,
        ) -> Result<(), PlaneError> {
            commit()
        }
    }

    fn rejected_publication(panic: bool) {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("file.rs"),
            "fn target() {} fn caller() { target(); }",
        )
        .unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "cleanup-family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", root.path())
            .unwrap();
        let bridge = Arc::new(CallgraphBridge::default());
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(Walker),
                vec![bridge.clone()],
            )
            .with_adapters(vec![bridge.adapter.clone()]),
        );
        driver.set_publish_fence(Arc::new(RejectAfterBuild {
            directory: owner.view_dir().to_path_buf(),
            panic,
        }));
        let access = ViewAccess::Owner(owner.clone());
        let loader = SiblingLoader::new(driver.clone(), vec![bridge.adapter.clone()]);
        let (snapshot, revision) = loader
            .reconciled(&access, loader.seed(&access).unwrap())
            .unwrap();
        driver.install(&access, &snapshot, revision).unwrap();
        let snapshot = driver.installed_snapshot();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            driver.build_own_generation(&access, &snapshot, false)
        }));
        if panic {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        for entry in std::fs::read_dir(owner.view_dir()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with("derived-") && !name.starts_with("manifest-"),
                "unpublished file remains: {name}"
            );
        }
        driver.set_publish_fence(Arc::new(Accept));
        let own = driver
            .build_own_generation(&access, &snapshot, false)
            .unwrap();
        assert!(
            super::super::resolve_derived_path(owner.view_dir(), own.name())
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn first_load_rejected_publication_removes_files_and_retry_succeeds() {
        rejected_publication(false);
    }

    #[test]
    fn first_load_panicked_publication_removes_files_and_retry_succeeds() {
        rejected_publication(true);
    }
    #[test]
    fn real_callgraph_bridge_installs_reader_before_wait_reports_ready() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("file.rs"),
            b"fn target() {}\nfn caller() { target(); }\n",
        )
        .unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", root.path())
            .unwrap();
        let bridge = Arc::new(CallgraphBridge::default());
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(Walker),
                vec![bridge.clone()],
            )
            .with_adapters(vec![bridge.adapter.clone()]),
        );
        let access = ViewAccess::Owner(owner);
        let loader = SiblingLoader::new(driver.clone(), vec![bridge.adapter.clone()]);
        let seed = loader.seed(&access).unwrap();
        let (seed_snapshot, revision) = loader.reconciled(&access, seed).unwrap();
        driver.install(&access, &seed_snapshot, revision).unwrap();
        let seed_snapshot = driver.installed_snapshot();
        assert_eq!(
            driver.installed_state(&access, FamilyPlane::Callgraph).1,
            vec![root.path().join("file.rs")]
        );
        let own = driver
            .build_own_generation(&access, &seed_snapshot, false)
            .unwrap();
        let (snapshot, revision) = loader.reconciled(&access, own).unwrap();
        driver.install(&access, &snapshot, revision).unwrap();
        assert_eq!(
            bridge.adapter.readiness(&access, &snapshot),
            super::super::readiness::PlaneReadiness::Ready {
                pending: 0,
                failed: 0
            }
        );
        let reader = bridge.adapter.reader(&access, &snapshot).unwrap();
        assert_eq!(reader.store.indexed_file_count().unwrap(), 1);
        assert_eq!(
            reader
                .store
                .direct_callers_of(std::path::Path::new("file.rs"), "target")
                .unwrap()
                .len(),
            1
        );
        assert!(driver
            .installed_state(&access, FamilyPlane::Callgraph)
            .1
            .is_empty());
        drop(reader);
        let old_bundle = driver.installed_bundle();
        let old_snapshot = old_bundle.snapshot.clone();
        let old_readers = Arc::downgrade(&old_bundle.readers);
        std::fs::write(root.path().join("file.rs"), b"fn replacement() {}\n").unwrap();
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        loader.load(&access).unwrap();
        let new_snapshot = driver.installed_snapshot();
        assert_eq!(
            bridge
                .adapter
                .reader(&access, &new_snapshot)
                .unwrap()
                .store
                .direct_callers_of(std::path::Path::new("file.rs"), "target")
                .unwrap()
                .len(),
            0
        );
        drop(old_bundle);
        assert!(
            old_readers.upgrade().is_some(),
            "an old query still owns its readers"
        );
        // Open lazily after replacement; retaining a reader opened before the
        // swap would not exercise residency in the adapter's generation map.
        assert_eq!(
            bridge
                .adapter
                .reader(&access, &old_snapshot)
                .unwrap()
                .store
                .direct_callers_of(std::path::Path::new("file.rs"), "target")
                .unwrap()
                .len(),
            1
        );
        drop(old_snapshot);
        assert!(
            old_readers.upgrade().is_none(),
            "the last old query releases the reader lease"
        );
    }
}

/// Configured walker membership with traversal errors retained. Source content
/// reads belong exclusively to CheckoutDriver's strict same-buffer reconciliation.
pub struct ConfiguredMembershipWalker;

#[cfg(test)]
thread_local! {
    pub(crate) static MEMBERSHIP_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
impl MembershipWalker for ConfiguredMembershipWalker {
    fn changed_files(
        &self,
        root: &std::path::Path,
        candidates: &std::collections::BTreeSet<std::path::PathBuf>,
    ) -> Result<Option<Vec<std::path::PathBuf>>, PlaneError> {
        let listing = crate::search_index::project_walk_lists(root, candidates);
        if candidates
            .iter()
            .any(|path| listing.gap_for(path).is_some())
        {
            return Ok(None);
        }
        Ok(Some(listing.listed.into_iter().collect()))
    }
    fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError> {
        #[cfg(test)]
        MEMBERSHIP_WALKS.with(|walks| walks.set(walks.get() + 1));
        let mut files = Vec::new();
        for result in crate::search_index::project_walk_builder(root).build() {
            let item = result
                .map_err(|error| SiblingLoader::error(format!("{}: {error}", root.display())))?;
            if let Some(error) = item.error() {
                return Err(SiblingLoader::error(format!(
                    "{}: {error}",
                    item.path().display()
                )));
            }
            if item.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(item.into_path());
            }
        }
        Ok(files)
    }
}

/// A root's installed checkout driver, shared so that the worker which loads
/// a checkout can install its driver from another thread. Installing
/// subscribes the driver to AFT write intents; the watcher drain forwards
/// every changed path to whatever driver is installed.
#[derive(Clone, Default)]
pub struct InstalledDriver(Arc<std::sync::RwLock<Option<Arc<CheckoutDriver>>>>);

impl InstalledDriver {
    pub fn install(&self, driver: Arc<CheckoutDriver>) {
        driver.register_write_intent();
        let retired = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(driver);
        drop(retired);
    }

    /// Removes `driver` unless another one replaced it meanwhile.
    pub fn clear_if(&self, driver: &Arc<CheckoutDriver>) {
        let mut installed = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retired = if installed
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, driver))
        {
            installed.take()
        } else {
            None
        };
        drop(installed);
        drop(retired);
    }

    pub fn clear(&self) {
        let retired = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(retired);
    }

    pub fn get(&self) -> Option<Arc<CheckoutDriver>> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub struct TrigramBridge {
    pub adapter: Arc<super::trigram::TrigramAdapter>,
    policy: crate::blob_store::v2::TrigramPolicy,
}
impl TrigramBridge {
    pub fn new(storage: std::path::PathBuf, policy: crate::blob_store::v2::TrigramPolicy) -> Self {
        Self {
            adapter: Arc::new(super::trigram::TrigramAdapter::new(storage, policy)),
            policy,
        }
    }
}
impl CompositePlane for TrigramBridge {
    fn bootstrap_policy(&self) -> Option<crate::blob_store::v2::TrigramPolicy> {
        Some(self.policy)
    }
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Trigram
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        !path.is_synthetic()
    }
    fn key(
        &self,
        _: &RelPath,
        content: crate::blob_store::v2::ContentHash,
    ) -> Option<crate::blob_store::v2::FamilyKey> {
        Some(
            crate::blob_store::v2::TrigramKey {
                content,
                policy: self.policy,
            }
            .family_key(),
        )
    }
    fn attachment(
        &self,
        _: &RelPath,
        bytes: &[u8],
    ) -> Result<super::snapshot::PlaneAttachment, PlaneError> {
        Ok(super::live_delta::entry(bytes, &self.policy, 0)
            .attachments
            .remove(&FamilyPlane::Trigram)
            .expect("trigram attachment"))
    }
    fn materialize(
        &self,
        owner: &super::registry::ViewRegistration,
        _: &str,
        _: &Snapshot,
        observed: &BTreeMap<RelPath, LiveEntry>,
        manifest: &mut super::manifest_v2::ManifestV2,
        live: &mut crate::pins::LivePin,
        _: bool,
    ) -> Result<(), PlaneError> {
        let materialized =
            super::trigram::materialize(owner, owner.root(), manifest, observed, self.policy)?;
        // Transfer protection while the materializer's pin is still held; the
        // composite live pin survives publication and verified reader admission.
        live.protect(&manifest.ready_keys().collect::<Vec<_>>())
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        live.protect_segment(&materialized.segment)
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        drop(materialized);
        Ok(())
    }
    fn finish_generation(
        &self,
        _: &super::registry::ViewRegistration,
        _: &str,
        _: &str,
    ) -> Result<(), PlaneError> {
        Ok(())
    }
    fn materialize_v3(
        &self,
        _owner: &super::registry::ViewRegistration,
        route: &super::snapshot::V3Route,
        _: &str,
        _: &Arc<ComposedGeneration>,
        observed: &BTreeMap<RelPath, LiveEntry>,
        delta: &mut super::manifest_v2::ManifestV2,
        live: &mut crate::pins::LivePin,
    ) -> Result<(), PlaneError> {
        if delta.header().producers.trigram != self.policy.fingerprint_hex() {
            return Err(SiblingLoader::error("trigram producer mismatch"));
        }
        let mut keys = Vec::new();
        let mut payloads = Vec::new();
        for (path, entry) in delta.entries() {
            let super::manifest_v2::EntryV2::Regular { content, .. } = entry else {
                continue;
            };
            let key = self.key(path, *content).unwrap();
            live.protect(&[key])
                .map_err(|e| SiblingLoader::error(e.to_string()))?;
            if !v3_contains(route, &key) {
                let attachment = observed
                    .get(path)
                    .filter(|e| e.disk == super::snapshot::DiskState::in_generation(delta, path))
                    .and_then(|e| e.attachments.get(&FamilyPlane::Trigram))
                    .and_then(|a| a.downcast_ref::<super::live_delta::Attachment>())
                    .filter(|a| a.content == *content && a.policy == self.policy.fingerprint())
                    .ok_or_else(|| {
                        SiblingLoader::error(
                            "trigram row missing; reconcile source before retrying",
                        )
                    })?;
                payloads.push((key, attachment.payload.encode()));
            }
            keys.push((path.clone(), key));
        }
        if !payloads.is_empty() {
            let request = payloads
                .iter()
                .map(|(_, p)| p.len() as u64)
                .sum::<u64>()
                .saturating_mul(2)
                .saturating_add(1 << 20);
            crate::blob_store::v3::publish(&route.storage, &route.family, request, None, |p| {
                for (key, payload) in &payloads {
                    p.put(key, payload, FamilyPlane::Trigram.payload_schema())?;
                }
                Ok(())
            })
            .map_err(|e| SiblingLoader::error(e.to_string()))?;
        }
        for (path, key) in keys {
            if let Some(super::manifest_v2::EntryV2::Regular { planes, .. }) = delta.get_mut(&path)
            {
                planes.trigram = Some(super::readiness::PlaneState::ready(&key));
            }
        }
        Ok(())
    }
}

impl CheckoutDriver {
    fn check_subscription(&self) {
        let stale = {
            let installed = self
                .installed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            installed.snapshot.watcher() == super::snapshot::WatcherState::Healthy
                && installed
                    .snapshot
                    .install_cut()
                    .and_then(|cut| cut.verification.as_ref())
                    .is_some_and(|guard| {
                        !guard.steady_supported
                            || guard.subscription.is_none()
                            || crate::watcher_backend::subscription::current(self.owner.root())
                                != guard.subscription
                    })
        };
        if stale {
            self.record_watcher_overflow();
        }
    }

    fn source_stat(
        &self,
        path: &std::path::Path,
        recorded_ns: u128,
    ) -> Result<Option<ObservedStat>, PlaneError> {
        #[cfg(test)]
        {
            self.work.lock().unwrap().stats += 1;
        }
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                let mut stat = ObservedStat::new(&metadata, recorded_ns);
                stat.native = super::verification::NativeStat::strict_read(path).ok();
                Ok(Some(stat))
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(SiblingLoader::error(error.to_string())),
        }
    }

    pub(crate) fn installed_cut(&self) -> (Snapshot, u64) {
        let installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (installed.snapshot.clone(), installed.revision)
    }

    #[cfg(test)]
    pub(crate) fn take_reconcile_work(&self) -> ReconcileWork {
        std::mem::take(&mut *self.work.lock().unwrap())
    }

    /// The snapshot last installed for this checkout, without computing the
    /// per-plane gaps `installed_state` reports.
    pub fn installed_snapshot(&self) -> Snapshot {
        self.check_subscription();
        self.installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    pub fn record_absolute_change(&self, absolute: &std::path::Path) {
        if let Ok(relative) = absolute.strip_prefix(self.owner.root()) {
            if let Ok(path) = RelPath::from_os_path(relative) {
                self.record_change(path);
            }
        }
    }

    /// Subscribe before serving the first snapshot. Weak writer registration
    /// stops notifying this checkout automatically when its driver is dropped.
    pub fn register_write_intent(self: &Arc<Self>) {
        let listener: Arc<dyn super::intent::WriteIntentListener> = self.clone();
        super::intent::register_listener(&listener);
    }
}
impl super::intent::WriteIntentListener for CheckoutDriver {
    fn record_change(&self, change: &super::intent::WriteIntent, _: super::intent::WritePhase) {
        match change {
            super::intent::WriteIntent::Paths(paths) => {
                for absolute in paths {
                    if let Ok(relative) = absolute.strip_prefix(self.owner.root()) {
                        if let Ok(path) = RelPath::from_os_path(relative) {
                            self.record_change(path);
                        }
                    }
                }
            }
            super::intent::WriteIntent::Directory(absolute) => {
                if !absolute.starts_with(self.owner.root()) {
                    return;
                }
                {
                    let mut live = self
                        .live
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    live.bump_epoch();
                    let mut installed = self
                        .installed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let installed = Arc::make_mut(&mut installed);
                    installed.revision += 1;
                    installed.uncertain_paths.insert(absolute.clone());
                    installed.snapshot = live
                        .snapshot()
                        .with_readers(installed.readers.clone())
                        .with_install_cut(InstallCut {
                            seq: installed.seq,
                            resident: installed.composed.clone(),
                            epoch: live.epoch(),
                            version: live.version(),
                            intent_version: live.snapshot().intent_version(),
                            verification: None,
                        });
                }
                let snapshot = self
                    .installed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .snapshot
                    .clone();
                for path in snapshot.membership().into_keys() {
                    self.record_change(path);
                }
                // A directory write can create previously unknown members. Keep
                // an explicit subtree intent even when the previous view was empty.
                if let Ok(relative) = absolute.strip_prefix(self.owner.root()) {
                    if let Ok(path) = RelPath::from_os_path(relative) {
                        self.record_change(path);
                    } else {
                        let mut installed = self
                            .installed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let installed = Arc::make_mut(&mut installed);
                        installed.revision += 1;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod integrated_plane_tests {
    use super::super::contracts::{PlaneAdapter, PlaneLoader};
    use super::*;
    #[test]
    fn acknowledged_shared_write_keeps_all_planes_unready_until_revision_install() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let absolute = root.path().join("file.rs");
        std::fs::write(&absolute, b"fn before() {}\n").unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "local", root.path())
            .unwrap();
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let trigram = Arc::new(TrigramBridge::new(storage.path().to_path_buf(), policy));
        let callgraph = Arc::new(CallgraphBridge::default());
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![trigram.clone(), callgraph.clone()],
            )
            .with_adapters(vec![trigram.adapter.clone(), callgraph.adapter.clone()]),
        );
        driver.register_write_intent();
        let access = ViewAccess::Owner(owner);
        let loader = SiblingLoader::new(
            driver.clone(),
            vec![trigram.adapter.clone(), callgraph.adapter.clone()],
        );
        let initial = loader.load(&access).unwrap();
        assert!(initial.pending_planes.is_empty());
        for plane in [FamilyPlane::Trigram, FamilyPlane::Callgraph] {
            assert!(driver.installed_state(&access, plane).1.is_empty());
        }
        let revision = driver.revision(&access);
        {
            let _intent = super::super::intent::record_paths([absolute.as_path()]);
            assert!(driver.revision(&access) > revision);
            for plane in [FamilyPlane::Trigram, FamilyPlane::Callgraph] {
                assert_eq!(
                    driver.installed_state(&access, plane).1,
                    vec![absolute.clone()]
                );
            }
            std::fs::write(&absolute, b"fn after_write() {}\n").unwrap();
        }
        assert!(driver.revision(&access) >= revision + 2);
        for plane in [FamilyPlane::Trigram, FamilyPlane::Callgraph] {
            assert_eq!(
                driver.installed_state(&access, plane).1,
                vec![absolute.clone()]
            );
        }
        let loaded = loader.load(&access).unwrap();
        assert!(loaded.pending_planes.is_empty());
        for plane in [FamilyPlane::Trigram, FamilyPlane::Callgraph] {
            assert!(driver.installed_state(&access, plane).1.is_empty());
        }
        let index = trigram
            .adapter
            .resident(access.scope(), loaded.snapshot.generation().name())
            .unwrap();
        let answer = index.query(root.path(), &loaded.snapshot, "after_write");
        assert_eq!(answer.matches.len(), 1);
        assert!(answer.gaps.is_empty());
        assert_eq!(
            callgraph.adapter.readiness(&access, &loaded.snapshot),
            super::super::readiness::PlaneReadiness::Ready {
                pending: 0,
                failed: 0
            }
        );
    }
}

#[cfg(test)]
mod checkout_kind_tests {
    use super::super::contracts::PlaneLoader;
    use super::*;
    fn git(root: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn all_checkout_kinds_have_independent_writable_generations_and_cold_membership() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("primary");
        std::fs::create_dir(&primary).unwrap();
        git(&primary, &["init", "-q"]);
        git(&primary, &["config", "user.email", "test@example.invalid"]);
        git(&primary, &["config", "user.name", "Test"]);
        std::fs::write(primary.join("file.rs"), b"fn seed() {}\n").unwrap();
        git(&primary, &["add", "file.rs"]);
        git(&primary, &["commit", "-qm", "fixture"]);
        let linked = temp.path().join("linked");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        let shared = temp.path().join("shared");
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                "--shared",
                primary.to_str().unwrap(),
                shared.to_str().unwrap(),
            ],
        );
        let plain = temp.path().join("plain");
        git(
            temp.path(),
            &[
                "clone",
                "-q",
                "--no-local",
                primary.to_str().unwrap(),
                plain.to_str().unwrap(),
            ],
        );
        let copied = temp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::copy(primary.join("file.rs"), copied.join("file.rs")).unwrap();
        let storage = temp.path().join("storage");
        let registry = super::super::registry::FamilyRegistry::open(&storage, "family").unwrap();
        for (scope, root) in [
            ("primary", primary),
            ("linked", linked),
            ("shared", shared),
            ("plain", plain),
            ("copied", copied),
        ] {
            std::fs::write(root.join("file.rs"), format!("fn {scope}_local() {{}}\n")).unwrap();
            std::fs::write(root.join("untracked.rs"), b"fn untracked() {}\n").unwrap();
            std::fs::write(root.join(".aftignore"), b"excluded.rs\n").unwrap();
            std::fs::write(root.join("excluded.rs"), b"fn excluded() {}\n").unwrap();
            let owner = registry
                .register_view_with_storage(registry.storage(), scope, &root)
                .unwrap();
            let policy = crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            };
            let trigram = Arc::new(TrigramBridge::new(storage.clone(), policy));
            let callgraph = Arc::new(CallgraphBridge::default());
            let driver = Arc::new(
                CheckoutDriver::new(
                    owner.clone(),
                    Producers {
                        trigram: policy.fingerprint_hex(),
                        semantic: None,
                        callgraph: super::super::callgraph::PRODUCER.into(),
                    },
                    None,
                    Arc::new(ConfiguredMembershipWalker),
                    vec![trigram.clone(), callgraph.clone()],
                )
                .with_adapters(vec![trigram.adapter.clone(), callgraph.adapter.clone()]),
            );
            let started = std::time::Instant::now();
            let loaded = SiblingLoader::new(
                driver,
                vec![trigram.adapter.clone(), callgraph.adapter.clone()],
            )
            .load(&ViewAccess::Owner(owner.clone()))
            .unwrap();
            assert!(
                loaded.pending_planes.is_empty(),
                "{scope}: {:?}",
                loaded.pending_planes
            );
            // Independent source oracle, with no tested seed/blob/generation reuse.
            let cold = super::super::live_delta::strict_walk(&root, &policy, 0);
            assert!(cold.gaps.is_empty());
            let cold_members = cold
                .entries
                .iter()
                .map(|(path, entry)| (path.clone(), entry.disk))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(loaded.snapshot.membership(), cold_members, "{scope}");
            assert!(!loaded
                .snapshot
                .membership()
                .contains_key(&RelPath::new(b"excluded.rs".to_vec()).unwrap()));
            let index = trigram
                .adapter
                .resident(scope, loaded.snapshot.generation().name())
                .unwrap();
            let answer = index.query(&root, &loaded.snapshot, &format!("{scope}_local"));
            assert_eq!(answer.matches.len(), 1, "{scope}");
            assert!(answer.gaps.is_empty());
            eprintln!(
                "checkout={scope} files={} elapsed_ms={} snapshot_metadata_lower_bound_bytes={}",
                loaded.snapshot.membership().len(),
                started.elapsed().as_millis(),
                loaded.snapshot.membership().len()
                    * std::mem::size_of::<super::super::snapshot::DiskState>()
            );
            let path = owner
                .view_store()
                .unwrap()
                .manifest_path(loaded.snapshot.generation().name())
                .unwrap();
            assert!(path.is_file());
        }
    }
}

#[cfg(test)]
mod v3_bootstrap_tests {
    use super::*;

    struct NamedPlane(FamilyPlane);
    impl CompositePlane for NamedPlane {
        fn plane(&self) -> FamilyPlane {
            self.0
        }
        fn applies_to(&self, _: &RelPath) -> bool {
            false
        }
        fn attachment(
            &self,
            _: &RelPath,
            _: &[u8],
        ) -> Result<super::super::snapshot::PlaneAttachment, PlaneError> {
            panic!("unsupported bootstrap must not extract")
        }
        fn materialize(
            &self,
            _: &super::super::registry::ViewRegistration,
            _: &str,
            _: &Snapshot,
            _: &BTreeMap<RelPath, LiveEntry>,
            _: &mut super::super::manifest_v2::ManifestV2,
            _: &mut crate::pins::LivePin,
            _: bool,
        ) -> Result<(), PlaneError> {
            panic!("unsupported bootstrap must not materialize")
        }
        fn finish_generation(
            &self,
            _: &super::super::registry::ViewRegistration,
            _: &str,
            _: &str,
        ) -> Result<(), PlaneError> {
            Ok(())
        }
    }

    #[test]
    fn fresh_nonlexical_bootstrap_has_a_typed_refusal_and_takes_no_claim() {
        let _serial = crate::blob_store::v3::serial_for_test();
        for plane in [FamilyPlane::Semantic, FamilyPlane::Callgraph] {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("file.rs"), b"fn untouched() {}\n").unwrap();
            let policy = crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            };
            let driver = CheckoutDriver::new_v3(
                storage.path(),
                "family",
                "worker",
                root.path(),
                Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: (plane == FamilyPlane::Semantic).then(|| "model".into()),
                    callgraph: if plane == FamilyPlane::Callgraph {
                        super::super::callgraph::PRODUCER.into()
                    } else {
                        "unregistered".into()
                    },
                },
                Arc::new(ConfiguredMembershipWalker),
                vec![
                    Arc::new(TrigramBridge::new(storage.path().to_owned(), policy)),
                    Arc::new(NamedPlane(plane)),
                ],
            )
            .unwrap();
            assert_eq!(
                driver.fresh_bootstrap_policy(),
                Err(FreshBootstrapRefusal::NonLexicalPlane(plane))
            );
            let error = driver
                .verified_bind(
                    &driver.access(),
                    driver.installed_snapshot().generation().clone(),
                )
                .unwrap_err();
            assert!(error.reason.contains("fresh_bootstrap_nonlexical_plane"));
            let work = driver.take_reconcile_work();
            assert_eq!((work.walks, work.reads), (0, 0));
            let registry =
                crate::blob_store::v3::LedgerReader::open_existing(storage.path(), "family")
                    .unwrap();
            assert_eq!(
                registry
                    .read(
                        |c| Ok(c.query_row("SELECT COUNT(*) FROM base_builds", [], |r| r
                            .get::<_, u64>(0))?)
                    )
                    .unwrap(),
                0
            );
            assert_eq!(
                registry
                    .read(|c| Ok(c.query_row(
                        "SELECT COUNT(*) FROM members WHERE kind='base'",
                        [],
                        |r| r.get::<_, u64>(0)
                    )?))
                    .unwrap(),
                0
            );
        }
    }

    fn trigram_driver(root: &std::path::Path, storage: &std::path::Path) -> CheckoutDriver {
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let plane = Arc::new(TrigramBridge::new(storage.to_owned(), policy));
        CheckoutDriver::new_v3(
            storage,
            "family",
            "worker",
            root,
            Producers {
                trigram: policy.fingerprint_hex(),
                semantic: None,
                callgraph: "unregistered".into(),
            },
            Arc::new(ConfiguredMembershipWalker),
            vec![plane.clone()],
        )
        .unwrap()
        .with_adapters(vec![plane.adapter.clone()])
    }

    #[test]
    fn continuous_refresh_reads_only_reported_paths_and_gaps_or_policy_edits_walk() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let _attributes = super::super::verification::known_attribute_sources_for_test(root.path());
        for name in ["file.rs", "other.rs"] {
            std::fs::write(root.path().join(name), b"fn before() {}\n").unwrap();
        }
        let subscription = crate::watcher_backend::subscription::start(
            &std::fs::canonicalize(root.path()).unwrap(),
            crate::watcher_backend::InitialCoverage::Complete,
        );
        let driver = trigram_driver(root.path(), storage.path());
        let access = driver.access();
        let (first, revision) = driver
            .verified_bind(&access, driver.installed_snapshot().generation().clone())
            .unwrap();
        driver.install(&access, &first, revision).unwrap();
        driver.take_reconcile_work();
        std::fs::write(root.path().join("file.rs"), b"fn edited() {}\n").unwrap();
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        let (changed, revision) = driver
            .verified_bind(&access, first.generation().clone())
            .unwrap();
        let work = driver.take_reconcile_work();
        assert_eq!(work.walks, 0);
        assert_eq!(work.reads, 1);
        assert!(work.stats <= 3);
        driver.install(&access, &changed, revision).unwrap();
        assert!(!driver.installed_snapshot().needs_reconcile());
        driver.record_watcher_overflow();
        let (reconciled, revision) = driver
            .verified_bind(&access, changed.generation().clone())
            .unwrap();
        assert_eq!(driver.take_reconcile_work().walks, 1);
        driver.install(&access, &reconciled, revision).unwrap();
        std::fs::write(root.path().join(".gitignore"), b"other.rs\n").unwrap();
        driver.record_change(RelPath::new(b".gitignore".to_vec()).unwrap());
        let (ignored, revision) = driver
            .verified_bind(&access, reconciled.generation().clone())
            .unwrap();
        assert_eq!(driver.take_reconcile_work().walks, 1);
        assert_eq!(
            ignored.disk_state(&RelPath::new(b"other.rs".to_vec()).unwrap()),
            super::super::snapshot::DiskState::Absent
        );
        driver.install(&access, &ignored, revision).unwrap();
        drop(subscription);
        assert!(driver.installed_snapshot().needs_reconcile());
        assert_eq!(
            driver.installed_snapshot().watcher(),
            super::super::snapshot::WatcherState::Overflowed
        );
    }

    #[test]
    fn no_subscription_keeps_v3_queries_on_the_strict_path_until_registration() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let _attributes = super::super::verification::known_attribute_sources_for_test(root.path());
        std::fs::write(root.path().join("file.rs"), b"fn observed() {}\n").unwrap();
        let driver = trigram_driver(root.path(), storage.path());
        let access = driver.access();
        let (snapshot, revision) = driver
            .verified_bind(&access, driver.installed_snapshot().generation().clone())
            .unwrap();
        driver.install(&access, &snapshot, revision).unwrap();
        assert!(driver.installed_snapshot().needs_reconcile());
        driver.take_reconcile_work();
        let _subscription = crate::watcher_backend::subscription::start(
            &std::fs::canonicalize(root.path()).unwrap(),
            crate::watcher_backend::InitialCoverage::Complete,
        );
        let (snapshot, revision) = driver
            .verified_bind(&access, snapshot.generation().clone())
            .unwrap();
        assert_eq!(driver.take_reconcile_work().walks, 1);
        driver.install(&access, &snapshot, revision).unwrap();
        assert!(!driver.installed_snapshot().needs_reconcile());
        let installed = driver.installed_snapshot();
        let mut cut = installed.install_cut().unwrap().clone();
        let mut guard = cut.verification.as_ref().unwrap().as_ref().clone();
        guard.steady_supported = false;
        cut.verification = Some(Arc::new(guard));
        driver
            .install(&access, &installed.with_install_cut(cut), revision)
            .unwrap();
        assert!(driver.installed_snapshot().needs_reconcile());
    }

    #[test]
    fn v3_lazy_attachments_probe_the_actual_producer_key_before_extraction() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let bytes = b"pub fn cached() -> u32 { 42 }\n";
        std::fs::write(root.path().join("file.rs"), bytes).unwrap();
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        let content = crate::blob_store::v2::ContentHash::of(bytes);
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let graph = CallgraphBridge::default();
        let attachment = graph.attachment(&path, bytes).unwrap();
        let extracted = attachment
            .downcast_ref::<super::super::callgraph::CallgraphAttachment>()
            .unwrap();
        assert_eq!(graph.key(&path, content), Some(extracted.key));
        let trigram_key = crate::blob_store::v2::TrigramKey { content, policy }.family_key();
        let trigram = super::super::segment_store::TrigramPayload::extract(bytes, &policy).encode();
        let graph_payload = extracted.blob.to_bytes().unwrap();
        crate::blob_store::v3::publish(storage.path(), "family", 1 << 20, None, |p| {
            p.put(
                &trigram_key,
                &trigram,
                FamilyPlane::Trigram.payload_schema(),
            )?;
            p.put(
                &extracted.key,
                &graph_payload,
                FamilyPlane::Callgraph.payload_schema(),
            )?;
            Ok(())
        })
        .unwrap();
        for (family, expected_extractions) in [("family", 0), ("miss", 2)] {
            let driver = CheckoutDriver::new_v3(
                storage.path(),
                family,
                "worker",
                root.path(),
                Producers {
                    trigram: policy.fingerprint_hex(),
                    semantic: None,
                    callgraph: super::super::callgraph::PRODUCER.into(),
                },
                Arc::new(ConfiguredMembershipWalker),
                vec![
                    Arc::new(TrigramBridge::new(storage.path().to_owned(), policy)),
                    Arc::new(CallgraphBridge::default()),
                ],
            )
            .unwrap();
            driver.reconcile(&driver.access()).unwrap();
            let work = driver.take_reconcile_work();
            assert_eq!(work.reads, 1);
            assert_eq!(work.attachments, expected_extractions);
            let mut header = driver.installed_snapshot().generation().header().clone();
            header.producers.trigram = "wrong-producer".into();
            let mut delta =
                super::super::manifest_v2::ManifestV2::new_v3(header, None, Default::default());
            let bridge = TrigramBridge::new(storage.path().to_owned(), policy);
            let mut pin =
                crate::pins::LivePin::create_v3(storage.path(), family, "worker").unwrap();
            assert!(bridge
                .materialize_v3(
                    &driver.owner,
                    &driver.owner.v3_route(),
                    "unused",
                    driver.installed_snapshot().generation(),
                    &BTreeMap::new(),
                    &mut delta,
                    &mut pin
                )
                .is_err());
        }
    }

    #[test]
    fn cached_callgraph_configuration_keeps_resolution_input_metadata() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let owner = super::super::registry::ViewRegistration::register_v3(
            storage.path(),
            "family",
            "worker",
            root.path(),
        )
        .unwrap();
        let bridge = CallgraphBridge::default();
        let path = RelPath::new(b"package.json".to_vec()).unwrap();
        let bytes = b"{\"name\":\"cached-configuration\"}";
        let content = crate::blob_store::v2::ContentHash::of(bytes);
        let attachment = bridge.attachment(&path, bytes).unwrap();
        let attachment = attachment
            .downcast_ref::<super::super::callgraph::CallgraphAttachment>()
            .unwrap();
        let key = attachment.key;
        crate::blob_store::v3::publish(storage.path(), "family", 1 << 20, None, |p| {
            p.put(
                &key,
                &attachment.blob.to_bytes().unwrap(),
                FamilyPlane::Callgraph.payload_schema(),
            )?;
            Ok(())
        })
        .unwrap();
        let header = super::super::manifest_v2::ManifestHeader {
            producers: Producers {
                trigram: "unused".into(),
                semantic: None,
                callgraph: super::super::callgraph::PRODUCER.into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: None,
        };
        let mut base_manifest =
            super::super::manifest_v2::ManifestV2::new_v3(header.clone(), None, Default::default());
        base_manifest
            .insert(
                RelPath::new(b"base.rs".to_vec()).unwrap(),
                super::super::manifest_v2::EntryV2::regular(
                    crate::blob_store::v2::ContentHash::of(b"base"),
                    4,
                    super::super::manifest_v2::EntryPlanes {
                        callgraph: Some(super::super::readiness::PlaneState::pending(
                            "base graph pending",
                        )),
                        ..Default::default()
                    },
                ),
            )
            .unwrap();
        let identity = super::super::base_refs::BaseIdentity::of("family", &base_manifest).unwrap();
        let base = Arc::new(ComposedGeneration::new("base", base_manifest, None));
        let mut delta = super::super::manifest_v2::ManifestV2::new_v3(
            header,
            Some(identity.header("base")),
            Default::default(),
        );
        delta
            .insert(
                path.clone(),
                super::super::manifest_v2::EntryV2::regular(
                    content,
                    bytes.len() as u64,
                    Default::default(),
                ),
            )
            .unwrap();
        let mut entry = LiveEntry::new(super::super::snapshot::DiskState::of_bytes(bytes), 0);
        entry.attachments.insert(
            FamilyPlane::Callgraph,
            bridge.cached_attachment(content, key),
        );
        let mut pin = crate::pins::LivePin::create_v3(storage.path(), "family", "worker").unwrap();
        bridge
            .materialize_v3(
                &owner,
                &owner.v3_route(),
                "private",
                &base,
                &BTreeMap::from([(path.clone(), entry)]),
                &mut delta,
                &mut pin,
            )
            .unwrap();
        assert!(matches!(
            delta.get(&path),
            Some(super::super::manifest_v2::EntryV2::Regular {
                resolution_input: true,
                ..
            })
        ));
    }

    fn legacy_inventory(storage: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(
            base: &std::path::Path,
            path: &std::path::Path,
            files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            if path == base.join("v3") {
                return;
            }
            if !path.exists() {
                return;
            }
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(base, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(base).unwrap().to_owned(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        let root = storage.join("blobs");
        visit(&root, &root, &mut files);
        files
    }

    #[test]
    fn verified_bind_fresh_trigram_driver_bootstraps_and_publishes_only_v3() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let _attributes = super::super::verification::known_attribute_sources_for_test(root.path());
        let _subscription = crate::watcher_backend::subscription::start(
            &std::fs::canonicalize(root.path()).unwrap(),
            crate::watcher_backend::InitialCoverage::Complete,
        );
        let source = b"fn fresh_checkout() { let marker = 42; }\n";
        std::fs::write(root.path().join("file.rs"), source).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(root.path().join("file.rs"))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(60)),
            )
            .unwrap();
        let legacy_v1 = storage.path().join("blobs/family");
        let legacy_v2 = storage.path().join("blobs/v2/family");
        for dir in [&legacy_v1, &legacy_v2] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("untouched"), b"legacy sentinel").unwrap();
        }
        let before = legacy_inventory(storage.path());
        let policy = crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let producers = Producers {
            trigram: policy.fingerprint_hex(),
            semantic: None,
            callgraph: "unregistered".into(),
        };
        let plane = Arc::new(TrigramBridge::new(storage.path().to_owned(), policy));
        let driver = CheckoutDriver::new_v3(
            storage.path(),
            "family",
            "worker",
            root.path(),
            producers.clone(),
            Arc::new(ConfiguredMembershipWalker),
            vec![plane.clone()],
        )
        .unwrap()
        .with_adapters(vec![plane.adapter.clone()]);
        let access = driver.access();
        assert!(driver.owner.open_store(FamilyPlane::Trigram).is_err());
        assert_eq!(legacy_inventory(storage.path()), before);
        let empty = driver.installed_snapshot().generation().clone();
        let (snapshot, revision) = driver.verified_bind(&access, empty).unwrap();
        assert!(snapshot.generation().base_header().is_some());
        assert_eq!(snapshot.generation().v3_route().unwrap().scope, "worker");
        assert!(snapshot.generation().header().head_tree.is_none());
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        assert_eq!(
            snapshot.disk_state(&path),
            super::super::snapshot::DiskState::of_bytes(source)
        );
        assert!(matches!(
            snapshot.plane_readiness(&path, FamilyPlane::Trigram),
            Some(super::super::readiness::PlaneState::Ready { .. })
        ));
        driver.install(&access, &snapshot, revision).unwrap();
        assert!(!driver.installed_snapshot().needs_reconcile());
        assert!(driver
            .installed_state(&access, FamilyPlane::Trigram)
            .1
            .is_empty());
        let route = driver.owner.v3_route();
        let store = super::super::ViewStore::existing_dir(route.member_dir()).unwrap();
        let generation = store.current_generation_read_only().unwrap().unwrap();
        let manifest = store.load_manifest_v2(&generation).unwrap();
        assert_eq!(
            manifest.len(),
            0,
            "unchanged bytes inherit the lexical-ready base"
        );
        assert!(
            super::super::verification::evidence_path(&route.member_dir(), &generation).exists()
        );
        assert_eq!(legacy_inventory(storage.path()), before);
        let ledger =
            crate::blob_store::v3::LedgerReader::open_existing(storage.path(), "family").unwrap();
        assert_eq!(
            ledger
                .read(|c| Ok(c.query_row(
                    "SELECT COUNT(*) FROM view_sessions WHERE scope='worker'",
                    [],
                    |r| r.get::<_, u32>(0)
                )?))
                .unwrap(),
            1
        );
        let second = CheckoutDriver::new_v3(
            storage.path(),
            "family",
            "worker",
            root.path(),
            producers,
            Arc::new(ConfiguredMembershipWalker),
            vec![Arc::new(TrigramBridge::new(
                storage.path().to_owned(),
                policy,
            ))],
        )
        .unwrap();
        let base = super::super::base_refs::open_foreign_v3(
            storage.path(),
            "family",
            "worker",
            &second.producers,
        )
        .unwrap()
        .unwrap();
        second.verified_bind(&second.access(), base).unwrap();
        let work = second.take_reconcile_work();
        assert_eq!(
            work.walks, 1,
            "membership-keyed bases still take strict startup verification"
        );
        assert_eq!(
            work.reads, 0,
            "generation-paired evidence avoids rereading stable source"
        );
        let other_content = crate::blob_store::v2::ContentHash::of(b"other bytes");
        let other_key = crate::blob_store::v2::TrigramKey {
            content: other_content,
            policy,
        }
        .family_key();
        let other_payload =
            super::super::segment_store::TrigramPayload::extract(b"other bytes", &policy).encode();
        crate::blob_store::v3::publish(storage.path(), "family", 1 << 20, None, |p| {
            p.put(
                &other_key,
                &other_payload,
                FamilyPlane::Trigram.payload_schema(),
            )?;
            Ok(())
        })
        .unwrap();
        let evidence = rusqlite::Connection::open(super::super::verification::evidence_path(
            &route.member_dir(),
            &generation,
        ))
        .unwrap();
        let bytes: Vec<u8> = evidence
            .query_row(
                "SELECT record FROM stat_evidence WHERE path=?1",
                [path.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        let mut record: super::super::verification::Evidence =
            serde_json::from_slice(&bytes).unwrap();
        record.keys.insert("Trigram".into(), other_key.to_hex());
        evidence
            .execute(
                "UPDATE stat_evidence SET record=?1 WHERE path=?2",
                rusqlite::params![serde_json::to_vec(&record).unwrap(), path.as_bytes()],
            )
            .unwrap();
        drop(evidence);
        let third = trigram_driver(root.path(), storage.path());
        let base = super::super::base_refs::open_foreign_v3(
            storage.path(),
            "family",
            "worker",
            &third.producers,
        )
        .unwrap()
        .unwrap();
        third.set_view_token("forged-advisory-token".into());
        let (checked, _) = third.verified_bind(&third.access(), base).unwrap();
        assert_eq!(
            third.take_reconcile_work().reads,
            1,
            "an existing but unrelated key is not stat provenance, even with a token"
        );
        assert_eq!(
            checked.disk_state(&path),
            super::super::snapshot::DiskState::of_bytes(source)
        );
        assert_eq!(legacy_inventory(storage.path()), before);
    }
}
