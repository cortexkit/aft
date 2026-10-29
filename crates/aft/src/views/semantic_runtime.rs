//! Views-on semantic search for one checkout.
//!
//! With `views.enabled`, a root's semantic lane is served by its own
//! per-checkout view instead of the resident legacy index: a
//! [`CheckoutSemantic`] owns the checkout's v3 member (`<scope>.semantic`),
//! the composite [`CheckoutDriver`] with the trigram, semantic and call-graph
//! planes registered, and the verified bind that installs the checkout's
//! generation. Identical content is embedded once per family, so worktrees of
//! one repository (and later sessions on them) reuse each other's vectors
//! instead of calling the model again.
//!
//! A fresh member bootstraps a v3 base whose semantic entries start pending.
//! The checkout that holds the base's build claim fills them on its fill
//! worker and publishes a readiness successor of the base; every checkout of
//! the base then serves those vectors. Until then they are reported pending.
//!
//! Loading, filling and folding run on the root's semantic view worker, never
//! on the request path. Queries read the installed snapshot and name every
//! path without current vectors as a gap.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use crate::semantic_index::SemanticIndex;

use super::contracts::{PlaneAdapter, ViewAccess};
use super::first_load::{
    CheckoutDriver, CompositePlane, ConfiguredMembershipWalker, FirstLoadDriver, InstalledDriver,
};
use super::intent::{WriteIntent, WriteIntentListener, WritePhase};
use super::manifest_v2::Producers;
use super::registry::ViewRegistration;
use super::semantic::{
    FillAdmission, FillBudget, FillReport, SemanticPlane, SemanticProducer, SemanticQuery,
};
use super::snapshot::{DiskState, LiveDelta, Snapshot};

/// Producer written into a manifest header for a plane its view does not
/// register (for example the call graph of a lexical-only view).
pub const UNREGISTERED_PRODUCER: &str = "unregistered";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// [`lock`] without waiting: `None` only while another thread holds it.
fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The process's semantic plane for `storage` and `producer`.
///
/// Every checkout of this process that uses the same storage root and
/// embedding producer gets the same plane, so their family arenas, resident
/// generations and single-flight embedding claims are shared. The registry
/// holds weak references: when the last checkout releases a plane, its
/// resident vectors are freed.
pub fn shared_plane(storage: &Path, producer: SemanticProducer) -> Arc<SemanticPlane> {
    type Registry = Vec<(PathBuf, String, Weak<SemanticPlane>)>;
    static PLANES: OnceLock<Mutex<Registry>> = OnceLock::new();
    let mut planes = lock(PLANES.get_or_init(|| Mutex::new(Vec::new())));
    planes.retain(|(_, _, plane)| plane.strong_count() > 0);
    let id = producer.id();
    if let Some(plane) = planes
        .iter()
        .find(|(root, producer, _)| root == storage && *producer == id)
        .and_then(|(_, _, plane)| plane.upgrade())
    {
        return plane;
    }
    let plane = Arc::new(SemanticPlane::new(storage.to_path_buf(), producer));
    planes.push((storage.to_path_buf(), id, Arc::downgrade(&plane)));
    plane
}

/// The v3 member scope of a checkout's semantic view, beside its lexical
/// (`scope`) and call-graph (`<scope>.callgraph`) members.
pub(crate) fn view_scope(scope: &str) -> String {
    format!("{scope}.semantic")
}

/// One checkout's semantic view: registration, driver and plane.
pub struct CheckoutSemantic {
    root: PathBuf,
    owner: ViewRegistration,
    /// The artifact storage root of the family's v3 namespace.
    storage: PathBuf,
    /// The producers this member's generations are published under.
    producers: Producers,
    access: ViewAccess,
    driver: Arc<CheckoutDriver>,
    plane: Arc<SemanticPlane>,
    /// Serializes reload, fill and fold, so two refreshes never interleave
    /// their publications.
    refresh: Mutex<()>,
    /// Fills installed since the last fold into a published generation.
    unfolded: AtomicBool,
    refresh_attempts: AtomicUsize,
    /// Why this checkout's vectors cannot be brought up to date, including
    /// the scheduled deadline when a transient refresh is being retried. Searches
    /// report it as `semantic: unavailable: <reason>`.
    unavailable: Mutex<Option<String>>,
    /// Semantic searches answered from this view.
    searches: AtomicU64,
    /// Readiness successors this view published as its base's owner.
    base_successors: AtomicU64,
    #[cfg(test)]
    before_bind_verify: Mutex<Option<Box<dyn Fn(&CheckoutSemantic) + Send + Sync>>>,
    /// Wakes the fill worker after an AFT write to this checkout. It is held
    /// here because the intent registry keeps only weak references.
    _wake_on_write: Arc<dyn WriteIntentListener>,
}

impl CheckoutSemantic {
    /// Registers `root` as view `scope` of `family` and builds its driver.
    /// Nothing is walked or published until [`Self::load`].
    ///
    /// `wake` is told about AFT writes under `root`; pass `Weak::new()` when
    /// nothing needs waking.
    pub fn new(
        storage: &crate::root_cache::StoragePaths,
        family: &str,
        scope: &str,
        root: &Path,
        producer: SemanticProducer,
        wake: Weak<CheckoutSemanticSlot>,
    ) -> Result<Self, String> {
        Self::with_trigram_policy(
            storage,
            family,
            scope,
            root,
            producer,
            crate::blob_store::v2::TrigramPolicy {
                max_file_size: crate::config::Config::default().search_index_max_file_size,
            },
            wake,
        )
    }

    /// [`Self::new`] for a root whose lexical views use `policy`. The view is a
    /// v3 member (`<scope>.semantic`) with all three planes registered, so its
    /// base carries trigram and call-graph rows too and its publications go
    /// to the v3 namespace only.
    pub fn with_trigram_policy(
        storage: &crate::root_cache::StoragePaths,
        family: &str,
        scope: &str,
        root: &Path,
        producer: SemanticProducer,
        policy: crate::blob_store::v2::TrigramPolicy,
        wake: Weak<CheckoutSemanticSlot>,
    ) -> Result<Self, String> {
        let plane = shared_plane(&storage.artifacts, producer);
        let producers = Producers {
            trigram: policy.fingerprint_hex(),
            semantic: Some(plane.semantic_producer().id()),
            callgraph: super::callgraph::PRODUCER.into(),
        };
        let composite: Arc<dyn CompositePlane> = plane.clone();
        let adapter: Arc<dyn PlaneAdapter> = plane.clone();
        let trigram = Arc::new(super::first_load::TrigramBridge::new(
            storage.artifacts.clone(),
            policy,
        ));
        let callgraph = Arc::new(super::first_load::CallgraphBridge::default());
        let member_scope = view_scope(scope);
        let driver = Arc::new(
            CheckoutDriver::new_v3(
                &storage.artifacts,
                family,
                &member_scope,
                root,
                producers.clone(),
                Arc::new(ConfiguredMembershipWalker),
                vec![trigram, composite, callgraph],
            )
            .map_err(|error| error.to_string())?
            .with_adapters(vec![adapter]),
        );
        driver.register_write_intent();
        let ViewAccess::Owner(owner) = driver.access() else {
            unreachable!("a checkout driver owns its view")
        };
        let listener: Arc<dyn WriteIntentListener> = Arc::new(WakeOnWrite {
            root: root.to_path_buf(),
            slot: wake,
        });
        super::intent::register_listener(&listener);
        let access = ViewAccess::Owner(owner.clone());
        hold_view(plane.as_ref(), &access);
        Ok(Self {
            root: root.to_path_buf(),
            access,
            owner,
            storage: storage.artifacts.clone(),
            producers,
            driver,
            plane,
            refresh: Mutex::new(()),
            unfolded: AtomicBool::new(false),
            refresh_attempts: AtomicUsize::new(0),
            unavailable: Mutex::new(None),
            searches: AtomicU64::new(0),
            base_successors: AtomicU64::new(0),
            #[cfg(test)]
            before_bind_verify: Mutex::new(None),
            _wake_on_write: listener,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn access(&self) -> &ViewAccess {
        &self.access
    }

    pub fn driver(&self) -> &Arc<CheckoutDriver> {
        &self.driver
    }

    pub fn plane(&self) -> &Arc<SemanticPlane> {
        &self.plane
    }

    /// The snapshot queries currently read: the cut the last bind installed
    /// after verifying it against the disk, plus the edits recorded since.
    ///
    /// It is read without the subscription check, as the lexical and
    /// call-graph sources do. `installed_snapshot` marks a checkout without a
    /// steady watcher subscription (every macOS checkout among them)
    /// overflowed on each read; a query doing that between the worker's bind
    /// and its fold would make the fold's install refuse, and every answer
    /// would be unvouched. The fill worker applies the check once per refresh
    /// instead (see `refresh_admitted`), so the next bind still walks the
    /// whole checkout.
    pub fn installed(&self) -> Snapshot {
        self.driver.installed_cut().0
    }

    /// Verified-binds the checkout's v3 member, publishes its edits and
    /// installs the result.
    pub fn load(&self) -> Result<Snapshot, String> {
        self.bind()
    }

    /// One verified bind of this member. The seed is the member's own
    /// published generation composed with its base's current generation, so a
    /// restart resumes from the last publication and a base readiness
    /// successor is picked up; a fresh member bootstraps its base. Edits are
    /// published as a new worker generation before the single install, as
    /// the call-graph view does. Nothing is written outside the v3 namespace.
    ///
    /// The bind starts with the driver's subscription check. Without a steady
    /// watcher subscription (every macOS checkout among them) the check marks
    /// the checkout overflowed, so the verified bind below walks the whole
    /// checkout and the install accepts its strictly reconciled cut. Checking
    /// later, between the bind and its install, would bump the live epoch
    /// under the cut and the install would refuse (`EpochChanged`): `install`
    /// itself repeats the check, which is a no-op only on a checkout already
    /// marked overflowed.
    fn bind(&self) -> Result<Snapshot, String> {
        self.bind_with_subscription_check(true)
    }

    fn bind_with_subscription_check(&self, check_subscription: bool) -> Result<Snapshot, String> {
        #[cfg(test)]
        let _profile = crate::blob_store::v3::work_profile::span("semantic_bind");
        #[cfg(test)]
        let seed_profile = crate::blob_store::v3::work_profile::span("bind_seed");
        let installed = if check_subscription {
            self.driver.installed_snapshot()
        } else {
            self.installed()
        };
        #[cfg(test)]
        if let Some(hook) = &*lock(&self.before_bind_verify) {
            hook(self);
        }
        let seed = match super::base_refs::open_foreign_v3(
            &self.storage,
            self.owner.family(),
            self.owner.scope(),
            &self.producers,
        ) {
            Ok(Some(generation)) => generation,
            Ok(None) => installed.generation().clone(),
            Err(error) => {
                crate::slog_debug!(
                    "semantic v3 seed unavailable; reconciling current tree: {}",
                    error
                );
                installed.generation().clone()
            }
        };
        #[cfg(test)]
        drop(seed_profile);
        #[cfg(test)]
        let verify_profile = crate::blob_store::v3::work_profile::span("bind_verify");
        let (mut snapshot, revision) = self
            .driver
            .verified_bind(&self.access, seed)
            .map_err(|error| error.to_string())?;
        #[cfg(test)]
        drop(verify_profile);
        if super::callgraph::has_unpublished_changes(&snapshot) {
            #[cfg(test)]
            let _profile = crate::blob_store::v3::work_profile::span("bind_own_generation");
            let own = self
                .driver
                .build_own_generation(
                    &self.access,
                    &snapshot,
                    super::first_load::DERIVED_CALLGRAPH_SEEDING,
                )
                .map_err(|error| error.to_string())?;
            let cut = snapshot
                .install_cut()
                .cloned()
                .ok_or("semantic view snapshot has no install cut")?;
            let mut delta = LiveDelta::from_snapshot(&snapshot);
            let draft = super::snapshot::derive_successor(
                &delta.cut(),
                own,
                &super::snapshot::carry_disk_state_only,
            );
            delta
                .replay_and_swap(draft, &super::snapshot::carry_disk_state_only)
                .map_err(|error| format!("{error:?}"))?;
            snapshot = delta
                .snapshot()
                .with_install_cut(cut.clone())
                .with_reconciled_epoch(snapshot.reconciled_epoch().unwrap_or(cut.epoch));
        }
        if check_subscription {
            self.driver.install(&self.access, &snapshot, revision)
        } else {
            self.driver
                .install_without_subscription_check(&self.access, &snapshot)
        }
        .map_err(|error| error.to_string())?;
        Ok(self.driver.installed_cut().0)
    }

    /// Folds this checkout's fill completions into a newly published worker
    /// generation and installs it, as `SiblingLoader::fold` does: the disk
    /// facts the last bind verified are reused, so a fold walks and reads
    /// nothing. The semantic plane marks every vector the store holds ready
    /// when it materializes the generation.
    ///
    /// The install skips the driver's subscription check. The refresh applied
    /// it once already; repeating it here would, on a checkout without a
    /// steady watcher subscription, mark the healthy bundle the bind just
    /// installed overflowed, bump the live epoch under this cut and refuse
    /// every fold. The next refresh applies the check again; searches read
    /// the installed cut without it. An edit recorded since the cut still
    /// refuses the install, and the refresh retries the fold.
    fn fold(&self) -> Result<(), String> {
        #[cfg(test)]
        let _profile = crate::blob_store::v3::work_profile::span("semantic_fold");
        let (snapshot, revision) = self.driver.installed_cut();
        self.driver
            .validate_cut(&self.access, &snapshot, revision)
            .map_err(|error| error.to_string())?;
        let cut = snapshot
            .install_cut()
            .cloned()
            .ok_or("semantic view snapshot has no install cut")?;
        let own = self
            .driver
            .build_own_generation(
                &self.access,
                &snapshot,
                super::first_load::DERIVED_CALLGRAPH_SEEDING,
            )
            .map_err(|error| error.to_string())?;
        own.ensure_producers(&self.driver.producers(&self.access))
            .map_err(|error| error.to_string())?;
        // Rebase the verified disk facts, not the new manifest's claims.
        let mut delta = LiveDelta::from_snapshot(&snapshot);
        let draft = super::snapshot::derive_successor(
            &delta.cut(),
            own,
            &super::snapshot::carry_disk_state_only,
        );
        delta
            .replay_and_swap(draft, &super::snapshot::carry_disk_state_only)
            .map_err(|error| format!("{error:?}"))?;
        self.driver
            .install_without_subscription_check(
                &self.access,
                &delta.snapshot().with_install_cut(cut),
            )
            .map_err(|error| error.to_string())
    }

    /// Completes, as the base's owner, the semantic entries this checkout
    /// inherits from its base while the base still has them pending. The base
    /// build claim makes the owner single-flight across processes: a checkout
    /// that cannot take it leaves the entries pending and searches report
    /// them as pending. Embedding runs on the ordinary budgeted fill path,
    /// the completed keys are published as one readiness successor of the
    /// base, and the member is bound again so it composes that successor.
    fn fill_base<F>(
        &self,
        budget: FillBudget,
        embed: &mut F,
        admit: &mut dyn FnMut(FillAdmission) -> Result<(), String>,
    ) -> Result<FillReport, RefreshError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        #[cfg(test)]
        let _profile = crate::blob_store::v3::work_profile::span("base_fill");
        let snapshot = self.installed();
        let Some(header) = snapshot.generation().base_header().cloned() else {
            return Ok(FillReport::default());
        };
        let pending = self.plane.inherited_pending(&snapshot);
        if pending.is_empty() {
            return Ok(FillReport::default());
        }
        let claim = match super::base_builder::claim_base(
            &self.storage,
            self.owner.family(),
            &header.scope,
            None,
        ) {
            Ok(super::base_builder::ClaimOutcome::Claimed(claim)) => claim,
            Ok(outcome) => {
                crate::slog_debug!(
                    "semantic base fill left to its owner root={} base={} outcome={:?}",
                    self.root.display(),
                    header.scope,
                    outcome
                );
                return Ok(FillReport::default());
            }
            Err(error) => return Err(RefreshError::classify(error.to_string())),
        };
        let result = (|| {
            // A base fill reads the files the base names, which on a first
            // bind is the whole checkout: such a fill takes a cold slot. One
            // within both SMALL_PREPARATION limits stays on the preparation
            // lane, as a small fill of the checkout's own edits does.
            if !small_work(&snapshot, pending.iter().map(|(path, _)| path)) {
                admit(FillAdmission::Cold).map_err(RefreshError::classify)?;
            }
            let mut delta = LiveDelta::from_snapshot(&snapshot);
            for (path, content) in &pending {
                delta.set_readiness(
                    path.clone(),
                    super::snapshot::PlanePatch {
                        content: *content,
                        planes: super::manifest_v2::EntryPlanes {
                            semantic: Some(super::readiness::PlaneState::pending(
                                "awaiting the base owner",
                            )),
                            ..Default::default()
                        },
                    },
                );
            }
            let draft = delta.snapshot();
            let driver = Arc::clone(&self.driver);
            let report = self
                .plane
                .fill_admitted(
                    &self.owner,
                    &draft,
                    budget,
                    embed,
                    &move || driver.installed_cut().0,
                    &mut |phase| {
                        admit(phase)?;
                        if phase == FillAdmission::Cold {
                            crate::cold_build_limiter::progress::stage("embedding");
                        }
                        Ok(())
                    },
                    // Read at most each file's recorded size plus one byte:
                    // a file that grew is left for its checkout to reconcile.
                    true,
                )
                .map_err(|error| RefreshError::classify(error.to_string()))?;
            let keys = self
                .plane
                .completed_keys(&self.owner, &draft, &pending)
                .map_err(|error| RefreshError::classify(error.to_string()))?;
            if !keys.is_empty() {
                #[cfg(test)]
                let _profile = crate::blob_store::v3::work_profile::span("base_successor");
                super::base_builder::publish_successor(
                    &claim,
                    |manifest| {
                        for (path, content, key) in &keys {
                            if let Some(super::manifest_v2::EntryV2::Regular {
                                content: base_content,
                                planes,
                                ..
                            }) = manifest.get_mut(path)
                            {
                                // Only the very bytes the base names, and only
                                // an entry still waiting for its owner.
                                if base_content == content
                                    && planes
                                        .semantic
                                        .as_ref()
                                        .is_some_and(super::readiness::PlaneState::is_pending)
                                {
                                    planes.semantic =
                                        Some(super::readiness::PlaneState::ready(key));
                                }
                            }
                        }
                    },
                    None,
                )
                .map_err(|error| RefreshError::classify(error.to_string()))?;
                self.base_successors.fetch_add(1, Ordering::Relaxed);
            }
            Ok((report, !keys.is_empty()))
        })();
        let released = claim.release();
        let (report, published) = result?;
        released.map_err(|error| RefreshError::classify(error.to_string()))?;
        if published {
            #[cfg(test)]
            let _profile = crate::blob_store::v3::work_profile::span("base_rebind");
            // The refresh checked subscription continuity before filling. A
            // base readiness publication changes no source files, so repeating
            // that check here would mark an unwatched checkout overflowed while
            // its already-filled vectors are being served. Still verify-bind
            // the successor and validate its cut; recorded edits and real
            // watcher gaps must be reconciled before installation.
            self.bind_with_subscription_check(false)
                .map_err(RefreshError::classify)?;
        }
        Ok(report)
    }

    /// Semantic searches this view answered.
    pub fn search_count(&self) -> u64 {
        self.searches.load(Ordering::Relaxed)
    }

    /// Readiness successors this view published as its base's owner.
    pub fn base_successor_count(&self) -> u64 {
        self.base_successors.load(Ordering::Relaxed)
    }

    /// Brings the checkout's vectors up to date within `budget`.
    ///
    /// The checkout is reconciled first when the installed snapshot carries
    /// edits it has not reconciled (a watcher change or an AFT write records
    /// an intent), so the fill sees the current bytes. Once no budgeted work
    /// remains (or a round makes no progress), the completions are folded
    /// into a newly published generation, which keeps them across restarts
    /// and lets sibling checkouts seed from them. Folding once per catch-up
    /// rather than per round avoids repeated manifest publications. A fold
    /// reuses reconciled disk facts unless an edit arrives during publication.
    /// Until then the fill map serves them and the live pin protects them.
    ///
    /// Errors are classified: [`RefreshError::Transient`] for failures that
    /// can clear by themselves (see [`is_transient_reason`]), and
    /// [`RefreshError::Unavailable`] for everything else. A fill that could
    /// not store what it embedded reports the store errors in its report.
    pub fn refresh<F>(&self, budget: FillBudget, embed: &mut F) -> Result<FillReport, RefreshError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        self.refresh_admitted(budget, embed, &mut |_| Ok(()))
    }

    fn refresh_admitted<F>(
        &self,
        budget: FillBudget,
        embed: &mut F,
        admit: &mut dyn FnMut(bool) -> Result<(), String>,
    ) -> Result<FillReport, RefreshError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        self.refresh_with_admission(budget, embed, &mut |phase| match phase {
            FillAdmission::Preparation => admit(true),
            FillAdmission::Cold => admit(false),
            FillAdmission::SuspendCold | FillAdmission::ResumeCold => Ok(()),
        })
    }

    fn refresh_with_admission<F>(
        &self,
        budget: FillBudget,
        embed: &mut F,
        admit: &mut dyn FnMut(FillAdmission) -> Result<(), String>,
    ) -> Result<FillReport, RefreshError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        let mutex_started = Instant::now();
        let _serial = lock(&self.refresh);
        let mutex_wait = mutex_started.elapsed();
        self.refresh_attempts.fetch_add(1, Ordering::SeqCst);
        // The refresh's first subscription check: without a steady watcher
        // subscription it marks the checkout overflowed, so the bind below
        // walks the whole checkout. Each bind repeats the check before it
        // verifies the tree.
        let mut snapshot = self.driver.installed_snapshot();
        // A first load, or unreconciled edits that are many or large, takes
        // cold admission before reconciling (see `needs_cold_reconcile`). A few
        // edits reconcile on the preparation lane, and a queue within both
        // SMALL_PREPARATION limits then prepares there too. Such a fill takes a
        // cold slot only at its first missing vector, so one that only reuses
        // stored vectors never queues behind cold builds.
        let mut queue_setup = Duration::ZERO;
        let cold_reconcile = needs_cold_reconcile(&snapshot, &self.root);
        admit(if cold_reconcile {
            FillAdmission::Cold
        } else {
            FillAdmission::Preparation
        })
        .map_err(RefreshError::classify)?;
        crate::cold_build_limiter::progress::stage("preparing");
        let reconcile_started = Instant::now();
        if needs_reload(&snapshot) {
            snapshot = if snapshot.generation().name() == "empty" {
                self.load().map_err(RefreshError::classify)?
            } else {
                self.bind().map_err(RefreshError::classify)?
            };
        }
        let reconcile = reconcile_started.elapsed();
        // Inherited base entries first: once their successor is published,
        // every checkout of the base serves them, not only this one.
        let base_report = self.fill_base(budget, embed, admit)?;
        if base_report != FillReport::default() {
            snapshot = self.installed();
        }
        // The size probe runs on the reconciled snapshot, so it counts the
        // work the edits actually left. It also runs under the process-wide
        // preparation limit.
        let small = if cold_reconcile {
            false
        } else {
            let probe_started = Instant::now();
            let small = self.small_preparation(&snapshot);
            queue_setup = probe_started.elapsed();
            if !small {
                admit(FillAdmission::Cold).map_err(RefreshError::classify)?;
            }
            small
        };
        let driver = Arc::clone(&self.driver);
        let mut report = self
            .plane
            .fill_admitted(
                &self.owner,
                &snapshot,
                budget,
                embed,
                &move || driver.installed_cut().0,
                &mut |phase| {
                    admit(phase)?;
                    if phase == FillAdmission::Cold {
                        crate::cold_build_limiter::progress::stage("embedding");
                    }
                    Ok(())
                },
                small,
            )
            .map_err(|error| RefreshError::classify(error.to_string()))?;
        report.timing.mutex_wait = mutex_wait;
        report.timing.reconcile = reconcile;
        report.timing.queue_setup += queue_setup;
        // Only this checkout's own completions are folded into its worker
        // generation; the base's were published as a base successor.
        let own_progress = report.installed > 0 || report.failed > 0;
        // One report for the round: the base owner's fill and this
        // checkout's own fill together.
        report.queued += base_report.queued;
        report.deferred += base_report.deferred;
        report.moved += base_report.moved;
        report.resident_hits += base_report.resident_hits;
        report.stored_hits += base_report.stored_hits;
        report.shared_waits += base_report.shared_waits;
        report.embedded_keys += base_report.embedded_keys;
        report.embedded_texts += base_report.embedded_texts;
        report.model_calls += base_report.model_calls;
        report.chunks += base_report.chunks;
        report.chunk_reused += base_report.chunk_reused;
        report.chunk_resident_hits += base_report.chunk_resident_hits;
        report.source_reads += base_report.source_reads;
        report.source_bytes += base_report.source_bytes;
        report.installed += base_report.installed;
        report.dropped += base_report.dropped;
        report.failed += base_report.failed;
        report.errors.extend(base_report.errors);
        report.store_errors.extend(base_report.store_errors);
        if own_progress {
            self.unfolded.store(true, Ordering::SeqCst);
        }
        let caught_up = report.deferred == 0 || (report.installed == 0 && report.failed == 0);
        if caught_up && self.unfolded.swap(false, Ordering::SeqCst) {
            let fold_started = Instant::now();
            crate::cold_build_limiter::progress::phase("publishing", None);
            if let Err(error) = self.fold() {
                self.unfolded.store(true, Ordering::SeqCst);
                return Err(RefreshError::classify(error));
            }
            report.timing.fold = fold_started.elapsed();
        }
        Ok(report)
    }

    fn small_preparation(&self, snapshot: &Snapshot) -> bool {
        let items = self.plane.pending_work(&self.access, snapshot);
        small_work(snapshot, items.iter().map(|item| &item.rel_path))
    }

    /// How many times `refresh` has run, successful or not.
    pub fn refresh_attempts(&self) -> usize {
        self.refresh_attempts.load(Ordering::SeqCst)
    }

    /// Why the checkout's vectors cannot be brought up to date, including a
    /// scheduled retry deadline for transient errors.
    pub fn unavailable_reason(&self) -> Option<String> {
        lock(&self.unavailable).clone()
    }

    /// Stores why the checkout's vectors cannot be brought up to date. True
    /// when the reason changed, so the caller logs it once rather than on
    /// every attempt.
    fn set_unavailable(&self, reason: String) -> bool {
        let mut unavailable = lock(&self.unavailable);
        let changed = unavailable.as_deref() != Some(reason.as_str());
        *unavailable = Some(reason);
        changed
    }

    fn clear_unavailable(&self) {
        *lock(&self.unavailable) = None;
    }

    /// Scores `query_vector` against the installed snapshot.
    pub fn search(
        &self,
        query_vector: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<SemanticQuery, String> {
        let mut answer = self
            .plane
            .search(
                &self.access,
                &self.root,
                &self.installed(),
                query_vector,
                top_k,
                include,
            )
            .map_err(|error| error.to_string())?;
        answer.unavailable = self.unavailable_reason();
        self.searches.fetch_add(1, Ordering::Relaxed);
        Ok(answer)
    }

    /// The resident index `search` scores for the installed snapshot, for
    /// callers (such as the search engine's readiness snapshot) that hold it.
    /// Builds it when the cached one is out of date.
    pub fn index(&self) -> Result<Arc<SemanticIndex>, String> {
        self.plane
            .overlay(&self.access, &self.root, &self.installed())
            .map(|overlay| overlay.index)
            .map_err(|error| error.to_string())
    }

    /// The index searches score for the installed snapshot, for the search
    /// engine's readiness snapshot. Returns the one already built for that
    /// snapshot, even when later fills made it out of date, so a sample
    /// normally builds nothing. It builds one only when the installed snapshot
    /// has none yet: right after a fill is published and installed, before the
    /// worker rebuilds it, or for a borrower's fresh reader binding. Reporting
    /// no index there would make the engine plan no semantic lane and answer
    /// with lexical hits alone, and the search that follows would build the
    /// same index anyway.
    ///
    /// The lookup goes through the installed snapshot's reader binding, which
    /// is where `search` caches its index. The owner's scope-wide state can
    /// differ: a publication replaces it while held snapshots keep the state
    /// they were bound to.
    ///
    /// An error names why the index could not be built (for example, the
    /// installed generation is not resident); the caller reports the semantic
    /// lane as unavailable for that reason.
    pub fn serving_index(&self) -> Result<Arc<SemanticIndex>, String> {
        let installed = self.installed();
        if let Some(index) = self.plane.cached_index_for(&self.access, &installed) {
            return Ok(index);
        }
        #[cfg(test)]
        if let Some(reason) = lock(&SERVING_INDEX_BUILD_FAILURES).get(&self.root).cloned() {
            return Err(reason);
        }
        self.plane
            .overlay(&self.access, &self.root, &installed)
            .map(|overlay| overlay.index)
            .map_err(|error| error.to_string())
    }

    /// Resident bytes this checkout accounts for: the family arena (shared
    /// by every checkout of the family in this process, counted once there)
    /// and this checkout's own overlay.
    pub fn memory(&self) -> CheckoutSemanticMemory {
        CheckoutSemanticMemory {
            family_arena: self.plane.arena(self.owner.family()).memory().bytes,
            private: self.plane.private_memory(&self.access),
        }
    }

    /// This checkout's own overlay bytes (the `private` part of
    /// [`Self::memory`]) without waiting; `None` while an overlay build holds
    /// the view. The family arena is counted process-wide, not here.
    pub fn try_private_memory(&self) -> Option<u64> {
        self.plane.try_private_memory(&self.access)
    }

    /// The resident bytes of this checkout's family arena, shared with every
    /// other view of the family.
    pub fn family_arena_bytes(&self) -> u64 {
        self.plane.arena(self.owner.family()).memory().bytes
    }
}

impl Drop for CheckoutSemantic {
    fn drop(&mut self) {
        // A lane restarted with the same producer shares this plane and view
        // (same family and scope). Only the last checkout runtime of a view
        // releases its state, or a superseded worker exiting would wipe the
        // fills and resident generations of the lane that replaced it.
        if release_view(self.plane.as_ref(), &self.access) {
            self.plane.unbind(&self.access);
        }
    }
}

/// A view of one plane: the plane's process-unique id (never reused, unlike
/// its address after it is freed), the family and the scope.
type LiveViewKey = (u64, String, String);

fn live_views() -> &'static Mutex<std::collections::HashMap<LiveViewKey, usize>> {
    static LIVE: OnceLock<Mutex<std::collections::HashMap<LiveViewKey, usize>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

fn live_view_key(plane: &SemanticPlane, access: &ViewAccess) -> LiveViewKey {
    (
        plane.id(),
        access.family().to_owned(),
        access.scope().to_owned(),
    )
}

/// How many live `CheckoutSemantic`s hold the view `access` names on `plane`.
pub fn live_holders(plane: &SemanticPlane, access: &ViewAccess) -> usize {
    lock(live_views())
        .get(&live_view_key(plane, access))
        .copied()
        .unwrap_or(0)
}

fn hold_view(plane: &SemanticPlane, access: &ViewAccess) {
    *lock(live_views())
        .entry(live_view_key(plane, access))
        .or_default() += 1;
}

/// Drops one holder of the view; true when it was the last.
fn release_view(plane: &SemanticPlane, access: &ViewAccess) -> bool {
    let mut live = lock(live_views());
    let key = live_view_key(plane, access);
    let Some(count) = live.get_mut(&key) else {
        // Every `CheckoutSemantic::new` holds exactly once and every drop
        // releases exactly once, so a missing entry is a bookkeeping bug.
        debug_assert!(false, "released a semantic view that was never held");
        return true;
    };
    *count -= 1;
    if *count == 0 {
        live.remove(&key);
        true
    } else {
        false
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CheckoutSemanticMemory {
    pub family_arena: u64,
    pub private: u64,
}

/// Whether fill work on `paths` is within both SMALL_PREPARATION limits.
fn small_work<'a>(
    snapshot: &Snapshot,
    paths: impl ExactSizeIterator<Item = &'a super::RelPath>,
) -> bool {
    if paths.len() > SMALL_PREPARATION_FILES {
        return false;
    }
    let mut bytes = 0u64;
    for path in paths {
        let DiskState::Present { size, .. } = snapshot.disk_state(path) else {
            return false;
        };
        bytes = bytes.saturating_add(size);
        if bytes > SMALL_PREPARATION_BYTES {
            return false;
        }
    }
    true
}

/// Whether the installed snapshot has edits a strict reconcile must resolve
/// before its vectors can be brought up to date. The initial snapshot (before
/// any generation was published) always needs one.
fn needs_reload(snapshot: &Snapshot) -> bool {
    snapshot.needs_reconcile()
}

/// Whether reconciling `snapshot` needs a cold-build slot. A reconcile walks
/// the whole checkout and reads what changed. The first load (an empty
/// generation) seeds and reads every member file. More unreconciled paths than
/// the small-preparation file bound can mean a branch switch or a large
/// generated change, and changed files larger in total than the
/// small-preparation byte bound cost a large read even when they are few. Only
/// a few small edits reconcile on the preparation lane, which caps how many
/// such reconciles run at once.
///
/// The byte total comes from `stat` alone, before anything is read. The probe
/// states at most the file bound's worth of paths and checks elapsed time
/// against [`RECONCILE_SIZE_PROBE_DEADLINE`] between `stat` calls and after
/// the scan; it cannot interrupt a call already in progress. A probe found past
/// its deadline, a path that cannot be stated, or a changed path that is not a
/// regular file counts as large. A changed path that no longer exists counts
/// as zero bytes, because the reconcile has nothing of it to read.
fn needs_cold_reconcile(snapshot: &Snapshot, root: &Path) -> bool {
    // Reconciled bytes may still have pending planes. Their fill admission
    // does not require probing source files for another reconciliation.
    if !needs_reload(snapshot) {
        return false;
    }
    if snapshot.generation().name() == "empty" {
        return true;
    }
    let started = Instant::now();
    let mut changed = std::collections::BTreeSet::new();
    let mut bytes = 0u64;
    for path in snapshot
        .pending_intent()
        .chain(snapshot.live_entries().map(|(path, _)| path))
    {
        if !changed.insert(path) {
            continue;
        }
        if changed.len() > SMALL_PREPARATION_FILES
            || started.elapsed() > RECONCILE_SIZE_PROBE_DEADLINE
        {
            return true;
        }
        let Ok(relative) = super::segment_store::rel_path_to_os(path) else {
            return true;
        };
        let size = match std::fs::metadata(root.join(relative)) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            Ok(_) => return true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(_) => return true,
        };
        bytes = bytes.saturating_add(size);
        if bytes > SMALL_PREPARATION_BYTES {
            return true;
        }
    }
    started.elapsed() > RECONCILE_SIZE_PROBE_DEADLINE
}

/// Wakes the fill worker after an AFT write lands under the checkout root.
/// The driver records the write intent itself; this only schedules the
/// refresh that resolves it.
struct WakeOnWrite {
    root: PathBuf,
    slot: Weak<CheckoutSemanticSlot>,
}

impl WriteIntentListener for WakeOnWrite {
    fn record_change(&self, change: &WriteIntent, phase: WritePhase) {
        if phase != WritePhase::After {
            return;
        }
        let touches = match change {
            WriteIntent::Paths(paths) => paths
                .iter()
                .any(|path| super::intent::relative_to_root(path, &self.root).is_some()),
            WriteIntent::Directory(directory) => {
                super::intent::relative_to_root(directory, &self.root).is_some()
                    || super::intent::relative_to_root(&self.root, directory).is_some()
            }
        };
        if touches {
            if let Some(slot) = self.slot.upgrade() {
                slot.wake();
            }
        }
    }
}

/// The semantic index status stage of a views-on lane whose checkout view is
/// still loading.
pub const CHECKOUT_VIEW_LOADING_STAGE: &str = "loading_checkout_view";

/// What a root's views-on semantic lane currently is.
#[derive(Clone)]
pub enum CheckoutSemanticState {
    /// The worker is starting the model or loading the checkout.
    Loading,
    Ready(Arc<CheckoutSemantic>),
    /// Registration or loading failed; the reason is reported as the
    /// semantic lane's named cause, never replaced by another index.
    Unavailable(String),
}

/// A root's views-on semantic lane, shared between its context and its fill
/// worker. Each start takes a new epoch; a worker whose epoch is no longer
/// current stops and never installs its result.
#[derive(Default)]
pub struct CheckoutSemanticSlot {
    state: RwLock<Option<CheckoutSemanticState>>,
    epoch: AtomicU64,
    wake: Mutex<Option<crossbeam_channel::Sender<()>>>,
    /// Worker threads currently running for this slot, of any epoch.
    workers: AtomicUsize,
    /// Held while an epoch changes and while a lane publishes its view, so a
    /// publication and the check that its lane is still current are atomic.
    publish: Mutex<()>,
    retry: Mutex<Option<RetryDeadline>>,
}

struct RetryDeadline {
    due: Instant,
    query_allowed_at: Instant,
}

impl CheckoutSemanticSlot {
    /// Worker threads of this lane still running, including superseded ones
    /// that have not noticed yet.
    pub fn workers(&self) -> usize {
        self.workers.load(Ordering::SeqCst)
    }

    /// One line describing the lane, for diagnostics.
    pub fn describe(&self) -> String {
        let state = match self.state() {
            None => "none".to_owned(),
            Some(CheckoutSemanticState::Loading) => "loading".to_owned(),
            Some(CheckoutSemanticState::Ready(runtime)) => match runtime.unavailable_reason() {
                Some(reason) => format!("unavailable: {reason}"),
                None => format!(
                    "ready producer={}",
                    runtime.plane().semantic_producer().id()
                ),
            },
            Some(CheckoutSemanticState::Unavailable(reason)) => format!("unavailable: {reason}"),
        };
        format!(
            "{state} epoch={} workers={}",
            self.epoch.load(Ordering::SeqCst),
            self.workers()
        )
    }

    /// Starts a new epoch in the `Loading` state and returns it with the
    /// receiver the new worker waits on.
    pub fn begin(&self) -> (u64, crossbeam_channel::Receiver<()>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        // Under the publish lock: once this returns, no earlier epoch's
        // publication is in flight or can start (see `LaneFence`).
        let epoch = {
            let _publishing = lock(&self.publish);
            self.epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
        };
        *lock(&self.wake) = Some(sender);
        *lock(&self.retry) = None;
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(CheckoutSemanticState::Loading);
        (epoch, receiver)
    }

    pub fn is_current(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::SeqCst) == epoch
    }

    fn set(&self, epoch: u64, state: CheckoutSemanticState) -> bool {
        let mut slot = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.is_current(epoch) {
            return false;
        }
        *slot = Some(state);
        true
    }

    pub fn install(&self, epoch: u64, runtime: Arc<CheckoutSemantic>) -> bool {
        self.set(epoch, CheckoutSemanticState::Ready(runtime))
    }

    pub fn fail(&self, epoch: u64, reason: String) -> bool {
        self.set(epoch, CheckoutSemanticState::Unavailable(reason))
    }

    /// Forgets the lane. Its worker sees the channel close and exits.
    pub fn clear(&self) {
        {
            let _publishing = lock(&self.publish);
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
        *lock(&self.wake) = None;
        *lock(&self.retry) = None;
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Forgets the lane only if `epoch` still owns it, for a worker that is
    /// stopping without a replacement. The slot then reads as never started,
    /// so the root's next artifact check starts a new worker instead of
    /// leaving search waiting on a lane nobody serves. True when it released.
    pub(crate) fn release(&self, epoch: u64) -> bool {
        // The `publish` lock is held throughout, so a racing `begin` cannot
        // have its new state or wake sender reset: `begin` advances the epoch
        // under the same lock.
        let _publishing = lock(&self.publish);
        if self
            .epoch
            .compare_exchange(
                epoch,
                epoch.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return false;
        }
        *lock(&self.wake) = None;
        *lock(&self.retry) = None;
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        true
    }

    pub fn state(&self) -> Option<CheckoutSemanticState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// [`Self::state`] without waiting: `None` while the state is being
    /// replaced, so status observers that must never block can report the
    /// lane as changing.
    pub fn try_state(&self) -> Option<Option<CheckoutSemanticState>> {
        match self.state.try_read() {
            Ok(state) => Some(state.clone()),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner().clone()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// Forgets the lane of an idle root without waiting. A loaded lane's
    /// checkout runtime can then drop, the view unbinds and the family arena
    /// drops the runs no other view holds. A lane still loading (waiting for
    /// its start signal, or retrying an unreachable backend) is forgotten as
    /// well: it would otherwise keep its worker thread and partial load for
    /// as long as the root stays unbound. Either worker sees its epoch end or
    /// its channel close and exits. The slot then reads as never started, so
    /// the next semantic query's reload check starts a new lane, which loads
    /// the view again from the family store.
    ///
    /// `None` when the lane's locks are held (the caller retries later);
    /// `Some(None)` when the slot held no runtime to hand back: a still-loading
    /// lane was released and its worker exits on its own, and a failed lane
    /// keeps its named cause; `Some(Some(runtime))` with the loaded runtime
    /// the slot let go.
    pub(crate) fn try_release_idle(&self) -> Option<Option<Arc<CheckoutSemantic>>> {
        let _publishing = try_lock(&self.publish)?;
        let mut state = match self.state.try_write() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        if !matches!(
            &*state,
            Some(CheckoutSemanticState::Ready(_) | CheckoutSemanticState::Loading)
        ) {
            return Some(None);
        }
        // Under the publish lock, like every other epoch change, so no
        // publication of the released lane can still be in flight.
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let released = match state.take() {
            Some(CheckoutSemanticState::Ready(runtime)) => Some(runtime),
            _ => None,
        };
        drop(state);
        *lock(&self.wake) = None;
        *lock(&self.retry) = None;
        Some(released)
    }

    /// Whether a views-on semantic lane was started for the root, in any
    /// state. While it is, the legacy semantic index is not built.
    pub fn active(&self) -> bool {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    pub fn runtime(&self) -> Option<Arc<CheckoutSemantic>> {
        match self.state()? {
            CheckoutSemanticState::Ready(runtime) => Some(runtime),
            _ => None,
        }
    }

    /// Asks the worker to refresh after its quiet window.
    pub fn wake(&self) {
        if let Some(sender) = lock(&self.wake).as_ref() {
            let _ = sender.send(());
        }
    }

    /// Expedites a transient retry at most once per probe interval.
    /// The request path only wakes the worker; it never loads or embeds sources.
    pub fn request_retry(&self) {
        if self.request_retry_at(Instant::now()) {
            self.wake();
        }
    }

    fn request_retry_at(&self, now: Instant) -> bool {
        let mut retry = lock(&self.retry);
        let Some(retry) = retry.as_mut() else {
            return false;
        };
        if now < retry.query_allowed_at || now >= retry.due {
            return false;
        }
        retry.due = now;
        true
    }
}

/// Everything the fill worker needs besides the slot.
pub(crate) struct WorkerConfig {
    pub root: PathBuf,
    pub storage: crate::root_cache::StoragePaths,
    pub family: String,
    pub scope: String,
    pub semantic: crate::config::SemanticBackendConfig,
    /// The trigram policy of the root's lexical views; the semantic member's
    /// base carries trigram rows under it.
    pub trigram_policy: crate::blob_store::v2::TrigramPolicy,
    /// The root's semantic status, which every status and readiness surface
    /// reads. The worker keeps it in step with the lane.
    pub status: Arc<RwLock<crate::context::SemanticIndexStatus>>,
    /// The root's installed checkout driver, which receives watcher changes.
    pub drivers: InstalledDriver,
    pub schedule: FillSchedule,
}

/// When and under what admission the worker fills.
pub(crate) struct FillSchedule {
    pub root: PathBuf,
    /// How long wakes must stop arriving before a fill runs, so a burst of
    /// edits is embedded once.
    pub quiet_window: Duration,
    /// First and longest wait before retrying a fill that left work behind
    /// because of a transient error (an unreachable or failing embedding
    /// backend, a checkout that changed during reconciliation). The wait
    /// doubles after each retry that makes no progress and starts over after
    /// one that does, so a backend that comes back is used again without
    /// any edit or reconfigure.
    pub retry_initial: Duration,
    pub retry_max: Duration,
    pub limiter: Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    /// True while the root has been unbound past the abandon grace window;
    /// refreshes wait for a rebind instead of loading the embedder.
    pub paused: Box<dyn Fn() -> bool + Send>,
    pub max_batch: usize,
}

/// Production retry waits for fills that hit a transient error.
pub(crate) const RETRY_INITIAL: Duration = Duration::from_secs(5);
pub(crate) const RETRY_MAX: Duration = Duration::from_secs(300);

const LIMITER_KIND: &str = "semantic view fill";

// These file-count and byte bounds admit small edits, including a typical file
// with dozens of reusable chunks, but not thousands of fresh worktree files.
// Count the entire queue, not just one budgeted round.
const SMALL_PREPARATION_FILES: usize = 32;
const SMALL_PREPARATION_BYTES: u64 = 256 * 1024;
/// Elapsed-time limit for `needs_cold_reconcile`'s `stat` probe, checked
/// between calls and after the scan. Past it, the changed files count as large
/// and the fill asks for cold admission. At most `SMALL_PREPARATION_FILES`
/// paths are stated, so this only trips on a slow filesystem; a single stalled
/// call still runs to completion before the check.
const RECONCILE_SIZE_PROBE_DEADLINE: Duration = Duration::from_millis(250);
const PREPARATION_KIND: &str = "semantic view preparation";
const PREPARATION_LIMIT: usize = 4;

fn preparation_limiter() -> &'static Arc<crate::cold_build_limiter::ColdBuildLimiter> {
    static LIMITER: OnceLock<Arc<crate::cold_build_limiter::ColdBuildLimiter>> = OnceLock::new();
    LIMITER.get_or_init(|| crate::cold_build_limiter::isolated_limiter(PREPARATION_LIMIT))
}

const INFLIGHT_FILL_KIND: &str = "semantic view in-flight fill";
const INFLIGHT_FILL_LIMIT: usize = 2;

fn inflight_fill_limiter() -> &'static Arc<crate::cold_build_limiter::ColdBuildLimiter> {
    static LIMITER: OnceLock<Arc<crate::cold_build_limiter::ColdBuildLimiter>> = OnceLock::new();
    // Parallel unit tests have independent roots and admission deadlines. Like
    // the global cold limiter, leave their shared default unconstrained; tests
    // of the memory bound inject the production limit explicitly.
    let limit = if cfg!(test) {
        1024
    } else {
        INFLIGHT_FILL_LIMIT
    };
    LIMITER.get_or_init(|| crate::cold_build_limiter::isolated_limiter(limit))
}

/// Lets a lane's driver publish its checkout's view only while the lane's
/// epoch is current. The check and the pointer publication run under the
/// slot's publish lock, which every epoch change also takes, so a worker
/// superseded by a restart (a reconfigure with another model) can never
/// publish its old producer's manifest over the replacement's: readers of
/// the view never see the old producer again once the new lane starts.
pub struct LaneFence {
    slot: Weak<CheckoutSemanticSlot>,
    epoch: u64,
}

impl LaneFence {
    pub fn new(slot: Weak<CheckoutSemanticSlot>, epoch: u64) -> Self {
        Self { slot, epoch }
    }
}

impl super::first_load::PublishFence for LaneFence {
    fn publish(
        &self,
        commit: &mut dyn FnMut() -> Result<(), super::contracts::PlaneError>,
    ) -> Result<(), super::contracts::PlaneError> {
        let refused = || {
            crate::slog_debug!(
                "semantic view publication refused for superseded lane epoch {}",
                self.epoch
            );
            Err(super::contracts::PlaneError {
                plane: Some(crate::blob_store::v2::FamilyPlane::Semantic),
                reason: super::first_load::SUPERSEDED_PUBLISH.to_owned(),
            })
        };
        let Some(slot) = self.slot.upgrade() else {
            return refused();
        };
        let _publishing = lock(&slot.publish);
        if !slot.is_current(self.epoch) {
            return refused();
        }
        commit()
    }
}

fn set_status(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    status: &RwLock<crate::context::SemanticIndexStatus>,
    value: crate::context::SemanticIndexStatus,
) {
    if slot.upgrade().is_some_and(|slot| slot.is_current(epoch)) {
        *status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
    }
}

fn is_current(slot: &Weak<CheckoutSemanticSlot>, epoch: u64) -> bool {
    slot.upgrade().is_some_and(|slot| slot.is_current(epoch))
}

/// Arms one bounded retry and names its deadline on every status surface.
fn schedule_retry(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    reason: &str,
    wait: Duration,
    initial: Duration,
) -> String {
    let now = Instant::now();
    if let Some(slot) = slot.upgrade() {
        let mut retry = lock(&slot.retry);
        // Recheck after acquiring the mutex: an epoch can change while a
        // superseded worker waits for the replacement's retry state.
        if slot.is_current(epoch) {
            *retry = Some(RetryDeadline {
                due: now + wait,
                query_allowed_at: now + initial.min(Duration::from_secs(1)),
            });
        }
    }
    let due = (std::time::SystemTime::now() + wait)
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "{}; next retry due at unix_ms={due} (scheduled in {} ms)",
        crate::semantic_index::strip_transient_embedding_marker(reason),
        wait.as_millis()
    )
}

fn clear_retry(slot: &Weak<CheckoutSemanticSlot>, epoch: u64) {
    if let Some(slot) = slot.upgrade() {
        let mut retry = lock(&slot.retry);
        if slot.is_current(epoch) {
            *retry = None;
        }
    }
}

/// Ordinary edit wakes do not bypass backoff. Only a rate-limited query can
/// advance the deadline. Channel closure or an epoch change cancels the wait.
fn wait_retry(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
) -> bool {
    loop {
        let Some(current) = slot.upgrade().filter(|slot| slot.is_current(epoch)) else {
            return false;
        };
        let remaining = lock(&current.retry)
            .as_ref()
            .map(|retry| retry.due.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        drop(current);
        if remaining.is_zero() {
            return true;
        }
        match wake.recv_timeout(remaining) {
            Ok(()) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => return is_current(slot, epoch),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Retry the failed stage, not the whole activation. In particular, a failed
/// fingerprint probe cannot cause a checkout walk, read or content embedding.
fn retry_activation<T>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
    config: &WorkerConfig,
    stage: &str,
    mut attempt: impl FnMut() -> Result<T, String>,
) -> Result<T, RefreshError> {
    let mut backoff = config.schedule.retry_initial;
    loop {
        if !is_current(slot, epoch) {
            return Err(RefreshError::Superseded);
        }
        match crate::semantic_index::retry_lane_pause_while(&mut attempt, &mut || {
            !(config.schedule.paused)()
        })
        .map_err(RefreshError::classify)
        {
            Ok(value) => {
                clear_retry(slot, epoch);
                return Ok(value);
            }
            Err(RefreshError::Transient(reason)) => {
                let reason = schedule_retry(
                    slot,
                    epoch,
                    &format!("{stage}: {reason}"),
                    backoff,
                    config.schedule.retry_initial,
                );
                if let Some(slot) = slot.upgrade() {
                    slot.fail(epoch, reason.clone());
                }
                set_status(
                    slot,
                    epoch,
                    &config.status,
                    crate::context::SemanticIndexStatus::Failed(format!(
                        "semantic: unavailable: {reason}"
                    )),
                );
                crate::slog_warn!(
                    "semantic view unavailable root={} reason={}",
                    config.root.display(),
                    reason
                );
                #[cfg(test)]
                observe_fill(&config.root, "unavailable");
                if !wait_retry(slot, epoch, wake) {
                    return Err(RefreshError::Superseded);
                }
                backoff = (backoff * 2).min(config.schedule.retry_max);
            }
            // The pause wait gave up although this lane is still current: the
            // root lost its route during an announced pause (or stayed
            // unbound past the grace window). That is not a replacement; park
            // until the rebind wakes the lane, then retry the stage.
            Err(RefreshError::Superseded) if is_current(slot, epoch) => {
                #[cfg(test)]
                observe_fill(&config.root, "parked");
                if !wait_parked(slot, epoch, wake) {
                    return Err(RefreshError::Superseded);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Blocks a parked lane until something wakes it: a rebind (which always
/// wakes the lane), or an edit. False when the lane was replaced or cleared,
/// which closes the channel or moves the epoch. Wakes are never drained
/// before parking, so a rebind that raced the park is not lost.
fn wait_parked(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
) -> bool {
    wake.recv().is_ok() && is_current(slot, epoch)
}

/// Counts a running worker on its slot for as long as it lives.
struct WorkerGuard(Weak<CheckoutSemanticSlot>);

impl WorkerGuard {
    fn enter(slot: &Weak<CheckoutSemanticSlot>) -> Self {
        if let Some(slot) = slot.upgrade() {
            slot.workers.fetch_add(1, Ordering::SeqCst);
        }
        Self(slot.clone())
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.0.upgrade() {
            slot.workers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Runs a root's views-on semantic lane: starts the embedding model,
/// registers and loads the checkout, then fills it now and whenever the
/// fill scheduler decides to.
///
/// The worker holds the slot only weakly, so it stops when the root's
/// context is dropped or the lane is cleared.
pub(crate) fn run_worker(
    slot: Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: crossbeam_channel::Receiver<()>,
    config: WorkerConfig,
) {
    let _counted = WorkerGuard::enter(&slot);
    let pause_slot = slot.clone();
    let _pause_retry = crate::semantic_index::PauseRetryScope::new(
        config.schedule.retry_initial,
        config.schedule.retry_max,
        move || is_current(&pause_slot, epoch),
    );
    // A lane cleared or restarted before this worker began (two quick
    // configures) does nothing at all: no model, no registration.
    if !is_current(&slot, epoch) {
        return;
    }
    let fail = |reason: String| {
        clear_retry(&slot, epoch);
        crate::slog_warn!(
            "semantic view unavailable root={} reason={}",
            config.root.display(),
            reason
        );
        if let Some(slot) = slot.upgrade() {
            slot.fail(epoch, reason.clone());
        }
        set_status(
            &slot,
            epoch,
            &config.status,
            crate::context::SemanticIndexStatus::Failed(format!("semantic: unavailable: {reason}")),
        );
        #[cfg(test)]
        observe_fill(&config.root, "unavailable");
    };
    let mut model = None;
    let fingerprint =
        match retry_activation(&slot, epoch, &wake, &config, "embedding model", || {
            if model.is_none() {
                model = Some(crate::semantic_index::EmbeddingModel::from_config(
                    &config.semantic,
                )?);
            }
            model
                .as_mut()
                .unwrap()
                .fingerprint_for_view(&config.semantic)
        }) {
            Ok(fingerprint) => fingerprint,
            Err(RefreshError::Superseded) => {
                release_lane(&slot, epoch);
                return;
            }
            Err(error) => return fail(format!("embedding model: {error}")),
        };
    let mut model = model.unwrap();
    let producer = SemanticProducer::current(fingerprint.as_string(), fingerprint.embed_text_caps);
    if !is_current(&slot, epoch) {
        return;
    }
    let runtime = match CheckoutSemantic::with_trigram_policy(
        &config.storage,
        &config.family,
        &config.scope,
        &config.root,
        producer,
        config.trigram_policy,
        slot.clone(),
    ) {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => return fail(format!("view registration: {error}")),
    };
    if !is_current(&slot, epoch) {
        return;
    }
    // Publications of this lane's view pass the lane fence, so a superseded
    // worker can no longer publish once this lane's epoch began.
    runtime
        .driver()
        .set_publish_fence(Arc::new(LaneFence::new(slot.clone(), epoch)));
    // Subscribe to watcher changes and write intents before the first
    // snapshot is served, so no edit made during the load is missed.
    config.drivers.install(Arc::clone(runtime.driver()));
    let started = Instant::now();
    #[cfg(any(test, feature = "test-timing-hooks"))]
    delay_startup_io_for_test("SEMANTIC_VIEW_LOAD");
    if let Err(error) =
        retry_activation(&slot, epoch, &wake, &config, "view load", || runtime.load())
    {
        config.drivers.clear_if(runtime.driver());
        if error == RefreshError::Superseded {
            release_lane(&slot, epoch);
        } else {
            fail(format!("view load: {error}"));
        }
        return;
    }
    // Build the index queries score before the lane is served, so neither
    // the first query nor a readiness sample has to build it.
    let _ = runtime.index();
    crate::slog_info!(
        "semantic view loaded root={} family={} scope={} load_ms={}",
        config.root.display(),
        config.family,
        config.scope,
        started.elapsed().as_millis()
    );
    let installed = slot
        .upgrade()
        .is_some_and(|slot| slot.install(epoch, Arc::clone(&runtime)));
    if !installed {
        config.drivers.clear_if(runtime.driver());
        return;
    }
    set_status(
        &slot,
        epoch,
        &config.status,
        crate::context::SemanticIndexStatus::ready(),
    );
    let driver = Arc::clone(runtime.driver());
    drop(runtime);
    serve_fills_with_status(
        &slot,
        epoch,
        &wake,
        &config.schedule,
        Some(&config.status),
        &mut |texts| model.embed(texts),
    );
    // Fills end when the lane is replaced, cleared or dropped, and then this
    // does nothing. Should they end while the lane is still this worker's,
    // the slot is given up so a new worker can be started for the root.
    if release_lane(&slot, epoch) {
        config.drivers.clear_if(&driver);
    }
}

/// Gives up the slot when a worker stops while its epoch still owns it. A
/// lane left current with no worker would read as started forever: artifact
/// checks never start another one, and wakes reach a channel nobody reads.
fn release_lane(slot: &Weak<CheckoutSemanticSlot>, epoch: u64) -> bool {
    let released = slot.upgrade().is_some_and(|slot| slot.release(epoch));
    if released {
        crate::slog_warn!("semantic view worker stopped; released its lane epoch {epoch}");
    }
    released
}

/// What one catch-up left behind.
enum FillOutcome {
    /// Nothing more to do until the next wake.
    Settled,
    /// Work is left because of a transient error or a paused root; retry
    /// after a wait. `progress` is whether this attempt got anything done.
    Retry {
        progress: bool,
        reason: Option<String>,
    },
    /// The lane is no longer this worker's.
    Stop,
    /// The lane is still this worker's, but its root lost its route while the
    /// embedding lane was paused: wait for the rebind's wake, not a timer.
    Parked,
}

/// Simulate a slow storage read, not CPU contention. Child-process tests get
/// an observation file before the delay so requests can target this exact phase.
#[cfg(any(test, feature = "test-timing-hooks"))]
pub(crate) fn delay_startup_io_for_test(phase: &str) {
    if let Some(path) = std::env::var_os(format!("AFT_TEST_{phase}_START_FILE")) {
        std::fs::write(path, b"started").expect("startup I/O observation");
    }
    if let Some(ms) = std::env::var(format!("AFT_TEST_{phase}_DELAY_MS"))
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Reasons `serving_index` reports instead of building the index, by checkout
/// root, so a test can make the build fail without corrupting a generation.
#[cfg(test)]
static SERVING_INDEX_BUILD_FAILURES: std::sync::LazyLock<
    Mutex<std::collections::HashMap<PathBuf, String>>,
> = std::sync::LazyLock::new(|| Mutex::new(Default::default()));

/// Makes `serving_index` fail with `reason` for the checkout at `root` whenever
/// it would build the index, until the returned guard drops.
#[cfg(test)]
pub(crate) fn fail_serving_index_build_for_test(root: &Path, reason: &str) -> ServingIndexFailure {
    lock(&SERVING_INDEX_BUILD_FAILURES).insert(root.to_path_buf(), reason.to_owned());
    ServingIndexFailure(root.to_path_buf())
}

#[cfg(test)]
pub(crate) struct ServingIndexFailure(PathBuf);

#[cfg(test)]
impl Drop for ServingIndexFailure {
    fn drop(&mut self) {
        lock(&SERVING_INDEX_BUILD_FAILURES).remove(&self.0);
    }
}

#[cfg(test)]
static FILL_OBSERVERS: std::sync::LazyLock<
    Mutex<std::collections::HashMap<PathBuf, crossbeam_channel::Sender<&'static str>>>,
> = std::sync::LazyLock::new(|| Mutex::new(Default::default()));

#[cfg(test)]
pub(crate) struct FillObserver {
    root: PathBuf,
    rx: crossbeam_channel::Receiver<&'static str>,
}
#[cfg(test)]
impl FillObserver {
    pub(crate) fn new(root: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        lock(&FILL_OBSERVERS).insert(root.clone(), tx);
        Self { root, rx }
    }
    pub(crate) fn next(&self) -> &'static str {
        self.rx
            .recv_timeout(Duration::from_secs(60))
            .expect("semantic fill event")
    }
}
#[cfg(test)]
impl Drop for FillObserver {
    fn drop(&mut self) {
        lock(&FILL_OBSERVERS).remove(&self.root);
    }
}
#[cfg(test)]
fn observe_fill(root: &Path, event: &'static str) {
    if let Some(tx) = lock(&FILL_OBSERVERS).get(root) {
        let _ = tx.send(event);
    }
}

/// Fills the lane's installed checkout now, then again after the quiet
/// window following each wake, and after a backoff when a fill left work
/// behind because of a transient error. Returns when the lane is cleared,
/// restarted or dropped.
#[cfg(test)]
pub(crate) fn serve_fills<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
    schedule: &FillSchedule,
    embed: &mut F,
) where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    serve_fills_with_status(slot, epoch, wake, schedule, None, embed);
}

fn serve_fills_with_status<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    wake: &crossbeam_channel::Receiver<()>,
    schedule: &FillSchedule,
    status: Option<&RwLock<crate::context::SemanticIndexStatus>>,
    embed: &mut F,
) where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    let budget = FillBudget {
        max_batch: schedule.max_batch.max(1),
        ..FillBudget::default()
    };
    let mut backoff = schedule.retry_initial;
    let mut retry_after = None;
    let mut first = true;
    loop {
        if !first {
            let woke = match retry_after {
                Some((_, true)) => {
                    if !wait_retry(slot, epoch, wake) {
                        return;
                    }
                    false
                }
                // Rebinding a paused checkout should resume it immediately;
                // only failed backend probes are held to a retry deadline.
                Some((wait, false)) => match wake.recv_timeout(wait) {
                    Ok(()) => true,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => false,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                },
                None => match wake.recv() {
                    Ok(()) => true,
                    Err(_) => return,
                },
            };
            #[cfg(test)]
            if woke {
                observe_fill(&schedule.root, "wake");
                if lock(&FILL_OBSERVERS).contains_key(&schedule.root) {
                    let delay = std::env::var("AFT_TEST_TIMING_DELAY_MS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    std::thread::sleep(Duration::from_millis(delay));
                }
            }
            if woke && !wait_quiet(wake, schedule.quiet_window) {
                return;
            }
        }
        first = false;
        let outcome = if (schedule.paused)() {
            #[cfg(test)]
            observe_fill(&schedule.root, "paused");
            FillOutcome::Retry {
                progress: false,
                reason: None,
            }
        } else {
            match slot
                .upgrade()
                .filter(|slot| slot.is_current(epoch))
                .and_then(|slot| slot.runtime())
            {
                Some(runtime) => catch_up(slot, epoch, &runtime, schedule, budget, embed),
                None => FillOutcome::Stop,
            }
        };
        match outcome {
            FillOutcome::Stop => return,
            FillOutcome::Parked => {
                #[cfg(test)]
                observe_fill(&schedule.root, "parked");
                retry_after = None;
            }
            FillOutcome::Settled => {
                clear_retry(slot, epoch);
                if let Some(status) = status {
                    let value = match slot
                        .upgrade()
                        .and_then(|slot| slot.runtime())
                        .and_then(|runtime| runtime.unavailable_reason())
                    {
                        Some(reason) => crate::context::SemanticIndexStatus::Failed(format!(
                            "semantic: unavailable: {reason}"
                        )),
                        None => crate::context::SemanticIndexStatus::ready(),
                    };
                    set_status(slot, epoch, status, value);
                }
                #[cfg(test)]
                observe_fill(&schedule.root, "settled");
                backoff = schedule.retry_initial;
                retry_after = None;
            }
            FillOutcome::Retry { progress, reason } => {
                if progress {
                    backoff = schedule.retry_initial;
                }
                retry_after = Some((backoff, reason.is_some()));
                if let Some(reason) = reason {
                    let reason =
                        schedule_retry(slot, epoch, &reason, backoff, schedule.retry_initial);
                    if let Some(runtime) = slot.upgrade().and_then(|slot| slot.runtime()) {
                        runtime.set_unavailable(reason.clone());
                    }
                    if let Some(status) = status {
                        set_status(
                            slot,
                            epoch,
                            status,
                            crate::context::SemanticIndexStatus::Failed(format!(
                                "semantic: unavailable: {reason}"
                            )),
                        );
                    }
                    #[cfg(test)]
                    observe_fill(&schedule.root, "retry");
                }
                backoff = (backoff * 2).min(schedule.retry_max);
            }
        }
    }
}

/// Waits until wakes stop arriving for `quiet_window`. False when the lane's
/// wake channel closed.
fn wait_quiet(wake: &crossbeam_channel::Receiver<()>, quiet_window: Duration) -> bool {
    let mut deadline = Instant::now() + quiet_window;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };
        match wake.recv_timeout(remaining) {
            Ok(()) => deadline = Instant::now() + quiet_window,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => return true,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Fills `runtime` in budgeted rounds until no budgeted work remains or a
/// round makes no progress.
fn catch_up<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    runtime: &CheckoutSemantic,
    schedule: &FillSchedule,
    budget: FillBudget,
    embed: &mut F,
) -> FillOutcome
where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    catch_up_with_preparation_limiter(
        slot,
        epoch,
        runtime,
        schedule,
        budget,
        embed,
        preparation_limiter(),
    )
}

fn catch_up_with_preparation_limiter<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    runtime: &CheckoutSemantic,
    schedule: &FillSchedule,
    budget: FillBudget,
    embed: &mut F,
    preparation_limiter: &Arc<crate::cold_build_limiter::ColdBuildLimiter>,
) -> FillOutcome
where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    catch_up_with_limiters(
        slot,
        epoch,
        runtime,
        schedule,
        budget,
        embed,
        preparation_limiter,
        inflight_fill_limiter(),
    )
}

fn catch_up_with_limiters<F>(
    slot: &Weak<CheckoutSemanticSlot>,
    epoch: u64,
    runtime: &CheckoutSemantic,
    schedule: &FillSchedule,
    budget: FillBudget,
    embed: &mut F,
    preparation_limiter: &Arc<crate::cold_build_limiter::ColdBuildLimiter>,
    inflight_limiter: &Arc<crate::cold_build_limiter::ColdBuildLimiter>,
) -> FillOutcome
where
    F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
{
    let mut progress = false;
    let mut telemetry = None;
    loop {
        if !is_current(slot, epoch) {
            return FillOutcome::Stop;
        }
        let elapsed_started = Instant::now();
        let embedding_timing_scope = crate::semantic_index::EmbeddingTimingScope::new();
        let admission_wait = Cell::new(Duration::ZERO);
        let mut cold_admitted = false;
        let refresh = crate::semantic_index::retry_lane_pause_while(
            || {
                let cold_permit = RefCell::new(None);
                let mut preparation_permit = None;
                let mut inflight_permit = None;
                let acquire = |limiter: &Arc<crate::cold_build_limiter::ColdBuildLimiter>, kind| {
                    crate::cold_build_limiter::progress::stage("waiting for admission");
                    let started = Instant::now();
                    let permit =
                        crate::cold_build_limiter::acquire_blocking_while_for_root_with_limiter(
                            limiter,
                            kind,
                            &schedule.root,
                            || is_current(slot, epoch),
                        );
                    admission_wait.set(admission_wait.get() + started.elapsed());
                    permit.ok_or_else(|| "semantic lane superseded".to_owned())
                };
                let mut embedding = |texts| {
                    cold_permit.borrow_mut().take();
                    crate::cold_build_limiter::progress::stage("waiting for embedding backend");
                    let result = embed(texts);
                    // Chunk-gate waiters release cold admission before blocking,
                    // so they cannot keep cold slots while the gate owner resumes.
                    *cold_permit.borrow_mut() = Some(acquire(&schedule.limiter, LIMITER_KIND)?);
                    crate::cold_build_limiter::progress::stage("storing embeddings");
                    result
                };
                let report = runtime
                    .refresh_with_admission(budget, &mut embedding, &mut |phase| {
                        if phase == FillAdmission::SuspendCold {
                            cold_permit.borrow_mut().take();
                            crate::cold_build_limiter::progress::stage(
                                "waiting for shared semantic work",
                            );
                            return Ok(());
                        }
                        if phase == FillAdmission::ResumeCold && inflight_permit.is_none() {
                            return Ok(());
                        }
                        if cold_permit.borrow().is_some() {
                            return Ok(());
                        }
                        if phase == FillAdmission::Preparation {
                            if preparation_permit.is_none() {
                                preparation_permit =
                                    Some(acquire(preparation_limiter, PREPARATION_KIND)?);
                            }
                        } else {
                            // A refresh retains all selected sources and chunks, not only
                            // the current model batch. Bound those allocations across the
                            // entire refresh, including backend waits. Small preparation
                            // keeps its own permit until this memory admission succeeds.
                            if inflight_permit.is_none() {
                                inflight_permit =
                                    Some(acquire(inflight_limiter, INFLIGHT_FILL_KIND)?);
                            }
                            preparation_permit.take();
                            cold_admitted = true;
                            *cold_permit.borrow_mut() =
                                Some(acquire(&schedule.limiter, LIMITER_KIND)?);
                        }
                        if telemetry.is_none() {
                            telemetry = Some(crate::cold_build_limiter::progress::start(
                                &schedule.root,
                                LIMITER_KIND,
                                None,
                                crate::cold_build_limiter::progress::StartLog::Info,
                            ));
                        }
                        Ok(())
                    })
                    .map_err(|error| error.to_string())?;
                if let Some(pause) = report
                    .errors
                    .iter()
                    .find(|error| crate::synapse_embed::LanePause::from_error(error).is_some())
                {
                    return Err(pause.clone());
                }
                Ok(report)
            },
            &mut || is_current(slot, epoch) && !(schedule.paused)(),
        )
        .map_err(RefreshError::classify);
        let embedding_timing = embedding_timing_scope.snapshot();
        drop(embedding_timing_scope);
        // Keep elapsed_ms comparable to older summaries, which started after
        // admission. Admission can now occur in the middle of a refresh.
        let elapsed_ms = elapsed_started
            .elapsed()
            .saturating_sub(admission_wait.get())
            .as_millis();
        if !is_current(slot, epoch) {
            return FillOutcome::Stop;
        }
        match refresh {
            Ok(report) => {
                if report.model_calls > 0
                    || report.installed > 0
                    || !report.errors.is_empty()
                    || !report.store_errors.is_empty()
                {
                    crate::slog_info!(
                        "semantic view fill root={} queued={} deferred={} embedded_keys={} chunks={} chunk_reused={} texts={} calls={} stored_hits={} resident_hits={} installed={} failed={} errors={} store_errors={} elapsed_ms={} embed_ms={} embed_max_ms={} admission_wait_ms={} cold_admitted={} mutex_wait_ms={} reconcile_ms={} queue_setup_ms={} read_hash_ms={} file_lookup_ms={} chunk_ms={} chunk_wait_ms={} vector_lookup_ms={} store_write_ms={} shared_wait_ms={} fold_ms={} source_reads={} source_bytes={} files_filled={} chunk_resident_hits={}",
                        schedule.root.display(),
                        report.queued,
                        report.deferred,
                        report.embedded_keys,
                        report.chunks,
                        report.chunk_reused,
                        report.embedded_texts,
                        report.model_calls,
                        report.stored_hits,
                        report.resident_hits,
                        report.installed,
                        report.failed,
                        report.errors.len(),
                        report.store_errors.len(),
                        elapsed_ms,
                        embedding_timing.total_ms(),
                        embedding_timing.max_ms(),
                        admission_wait.get().as_millis(),
                        cold_admitted,
                        report.timing.mutex_wait.as_millis(),
                        report.timing.reconcile.as_millis(),
                        report.timing.queue_setup.as_millis(),
                        report.timing.read_hash.as_millis(),
                        report.timing.file_lookup.as_millis(),
                        report.timing.chunk.as_millis(),
                        report.timing.chunk_wait.as_millis(),
                        report.timing.vector_lookup.as_millis(),
                        report.timing.store_write.as_millis(),
                        report.timing.shared_wait.as_millis(),
                        report.timing.fold.as_millis(),
                        report.source_reads,
                        report.source_bytes,
                        report.source_reads,
                        report.chunk_resident_hits
                    );
                }
                let advanced = report.installed > 0 || report.failed > 0;
                progress |= advanced;
                // A store that refuses what was embedded will refuse it again
                // on a timer: record it as the checkout's unavailable reason
                // and wait for the next wake.
                if let Some(error) = report.store_errors.first() {
                    mark_unavailable(runtime, schedule, format!("semantic store: {error}"));
                    warm_index(runtime);
                    return FillOutcome::Settled;
                }
                if !report.errors.is_empty() {
                    if let Some(error) = report
                        .errors
                        .iter()
                        .find(|error| !crate::semantic_index::embedding_failure_is_transient(error))
                    {
                        mark_unavailable(runtime, schedule, error.clone());
                        warm_index(runtime);
                        return FillOutcome::Settled;
                    }
                    crate::slog_warn!(
                        "semantic view fill left work for a retry root={} error={}",
                        schedule.root.display(),
                        report.errors[0]
                    );
                    warm_index(runtime);
                    return FillOutcome::Retry {
                        progress,
                        reason: report.errors.first().cloned(),
                    };
                }
                runtime.clear_unavailable();
                // Work beyond the budget continues at once; a round that made
                // no progress without an error has nothing it can do now.
                if report.deferred == 0 || !advanced {
                    warm_index(runtime);
                    return FillOutcome::Settled;
                }
            }
            Err(RefreshError::Transient(error)) => {
                crate::slog_warn!(
                    "semantic view refresh will retry root={} error={}",
                    schedule.root.display(),
                    error
                );
                return FillOutcome::Retry {
                    progress,
                    reason: Some(error),
                };
            }
            Err(RefreshError::Unavailable(error)) => {
                mark_unavailable(runtime, schedule, error);
                return FillOutcome::Settled;
            }
            // A replaced lane already stopped at the currency check above, so
            // a refusal that gets here came from the pause wait while the lane
            // is still current: the root lost its route during an announced
            // pause, or stayed unbound past the grace window. Park until the
            // rebind wakes the lane.
            Err(RefreshError::Superseded) => return FillOutcome::Parked,
        }
    }
}

/// Records a refresh error that retrying on a timer would not fix as the
/// checkout's unavailable reason, logging it once. The next wake (an edit, a rebind, a reconfigure)
/// tries again, and a clean refresh clears it.
fn mark_unavailable(runtime: &CheckoutSemantic, schedule: &FillSchedule, reason: String) {
    if runtime.set_unavailable(reason.clone()) {
        crate::slog_warn!(
            "semantic view unavailable root={} reason={}",
            schedule.root.display(),
            reason
        );
    }
}

/// A failed refresh, by whether waiting can fix it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefreshError {
    /// Clears by itself: the checkout changed or was being written while it
    /// was loaded, or a SQLite database was busy or locked by another
    /// connection.
    Transient(String),
    /// Needs something to change first: an unreadable or corrupt store, a
    /// refused registration or publication, a manifest that does not match.
    Unavailable(String),
    /// The lane was replaced; the lane fence refused this publication.
    Superseded,
}

impl RefreshError {
    fn classify(reason: String) -> Self {
        if reason.contains(super::first_load::SUPERSEDED_PUBLISH)
            || reason == "semantic lane superseded"
        {
            Self::Superseded
        } else if is_transient_reason(&reason) {
            Self::Transient(reason)
        } else {
            Self::Unavailable(reason)
        }
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(reason) | Self::Unavailable(reason) => f.write_str(reason),
            Self::Superseded => f.write_str(super::first_load::SUPERSEDED_PUBLISH),
        }
    }
}

/// Whether a model, load or store failure clears by itself. Embedding errors
/// carry the backend's authoritative transient marker. The only store failure
/// retried is SQLite's busy/locked
/// (another connection holds the database for a moment); every other store
/// error is surfaced as the checkout's named gap.
/// A publication or installation that may already have committed carries the
/// checkout-change marker so the worker revalidates a fresh job before serving it.
pub fn is_transient_reason(reason: &str) -> bool {
    crate::semantic_index::embedding_failure_is_transient(reason)
        || [
            super::first_load::CHECKOUT_CHANGED_DURING_RECONCILE,
            super::first_load::CHECKOUT_WRITE_ACTIVE,
            super::first_load::CHECKOUT_CHANGED_BEFORE_INSTALL,
            // SQLITE_BUSY and SQLITE_LOCKED, as rusqlite renders them.
            "database is locked",
            "database table is locked",
            "database is busy",
        ]
        .iter()
        .any(|marker| reason.contains(marker))
}

/// Rebuilds the scored index after a fill, on the worker rather than on the
/// next query.
fn warm_index(runtime: &CheckoutSemantic) {
    let _ = runtime.index();
}

#[cfg(test)]
#[path = "semantic_runtime_tests.rs"]
mod tests;
