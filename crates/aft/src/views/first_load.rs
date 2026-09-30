//! Sibling-seeded loading contracts. These hooks are opt-in until all planes
//! can build checkout generations; registering a driver does not change legacy routing.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::blob_store::v2::FamilyPlane;

use super::contracts::{PlaneError, ViewAccess};
use super::manifest_v2::Producers;
use super::snapshot::{LiveEntry, OpenGeneration, Snapshot};
use super::RelPath;

/// A complete strict content walk under the checkout's current ignore rules.
/// The revision must change for every delivered edit, write intent, watcher gap,
/// or membership change; a partial walk must return an error, not this value.
#[derive(Clone, Debug)]
pub struct ReconciledCheckout {
    pub revision: u64,
    pub entries: BTreeMap<RelPath, LiveEntry>,
}

/// The runtime owner supplies source reads and generation construction. Plane
/// implementations attach their data here rather than writing shared runtime code.
pub trait FirstLoadDriver: Send + Sync {
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
    ) -> Result<Arc<OpenGeneration>, PlaneError>;

    /// Called with a reconciled seed snapshot, then with the rebased own snapshot.
    /// It must atomically install resident data and snapshot under the root lock,
    /// rejecting an obsolete revision. It may block on that lock, never build,
    /// publish, repair another checkout's state, or report success before data is installed.
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
        PlaneError {
            plane: FamilyPlane::Trigram,
            reason: reason.into(),
        }
    }

    fn seed(&self, access: &ViewAccess) -> Result<Arc<OpenGeneration>, PlaneError> {
        let producers = self.driver.producers(access);
        if let ViewAccess::Reader {
            registration,
            scope,
        } = access
        {
            return super::read::open_foreign_generation(registration, scope, &producers)
                .map_err(|error| Self::error(error.to_string()))?
                .ok_or_else(|| {
                    Self::error(format!("foreign view {scope} unavailable: no generation"))
                });
        }
        let ViewAccess::Owner(owner) = access else {
            unreachable!()
        };
        let reader = owner
            .registry()
            .register_reader("first-load")
            .map_err(|error| Self::error(error.to_string()))?;
        let mut candidates = Vec::new();
        let head = self.driver.head_tree(access);
        for member in reader
            .members()
            .map_err(|error| Self::error(error.to_string()))?
        {
            // A broken sibling is not repaired and cannot prevent a full walk.
            if let Ok(Some(generation)) =
                super::read::open_foreign_generation(&reader, &member.scope, &producers)
            {
                let exact_head = head.is_some() && generation.manifest().header().head_tree == head;
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
        Ok(Arc::new(OpenGeneration::new(
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
        base: Arc<OpenGeneration>,
    ) -> Result<(Snapshot, u64), PlaneError> {
        for _ in 0..3 {
            let checkout = self.driver.reconcile(access)?;
            if self.driver.revision(access) != checkout.revision {
                continue;
            }
            let mut delta = super::snapshot::LiveDelta::new(Arc::clone(&base));
            delta.bump_epoch();
            // Full walker membership replaces seed membership, including ignored
            // sibling paths and files deleted in this checkout.
            for (path, _) in base.manifest().entries() {
                if !checkout.entries.contains_key(path) {
                    delta.apply(
                        path.clone(),
                        LiveEntry::new(super::snapshot::DiskState::Absent, checkout.revision),
                    );
                }
            }
            for (path, entry) in checkout.entries {
                delta.apply(path, entry);
            }
            return Ok((delta.snapshot(), checkout.revision));
        }
        Err(Self::error(
            "checkout changed during strict reconciliation; retry load",
        ))
    }

    fn open_planes(&self, access: &ViewAccess, snapshot: &Snapshot) -> Vec<PlaneError> {
        let mut pending = Vec::new();
        for adapter in &self.adapters {
            if let Err(error) = adapter.open_generation(access, snapshot.generation()) {
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
                    plane: adapter.plane(),
                    reason: "plane has unreflected checkout work".into(),
                });
            }
        }
        pending
    }
}

impl super::contracts::PlaneLoader for SiblingLoader {
    fn load(&self, access: &ViewAccess) -> Result<super::contracts::LoadOutcome, PlaneError> {
        let seed = self.seed(access)?;
        if access.is_read_only() {
            let snapshot = super::snapshot::LiveDelta::new(seed).snapshot();
            let pending_planes = self.open_planes(access, &snapshot);
            return Ok(super::contracts::LoadOutcome {
                snapshot,
                pending_planes,
            });
        }
        let (snapshot, revision) = self.reconciled(access, seed)?;
        self.driver.install(access, &snapshot, revision)?;
        let own = self
            .driver
            .build_own_generation(access, &snapshot, DERIVED_CALLGRAPH_SEEDING)?;
        own.manifest()
            .ensure_producers(&self.driver.producers(access))
            .map_err(|error| Self::error(error.to_string()))?;
        // The builder may have read an earlier cut. Re-walk rather than assuming
        // publication also installed every edit delivered while it was building.
        let (snapshot, revision) = self.reconciled(access, own)?;
        self.driver.install(access, &snapshot, revision)?;
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
            Ok(ReconciledCheckout { revision, entries })
        }
        fn build_own_generation(
            &self,
            _: &ViewAccess,
            _: &Snapshot,
            seed_derived: bool,
        ) -> Result<Arc<OpenGeneration>, PlaneError> {
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
    fn loader_retries_edits_and_removes_sibling_members_before_serving() {
        let temp = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(temp.path(), "family").unwrap();
        let sibling = registry.register_view("sibling", temp.path()).unwrap();
        publish(&sibling, &manifest(&[("excluded.rs", b"foreign")]));
        let owner = registry.register_view("local", temp.path()).unwrap();
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
        let owner = registry.register_view("foreign", temp.path()).unwrap();
        publish(&owner, &manifest(&[("file.rs", b"hello")]));
        let store = owner.view_store().unwrap();
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
        assert_eq!(generation.manifest().len(), 1);
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
        let sibling = registry.register_view("sibling", temp.path()).unwrap();
        publish(&sibling, &manifest(&[("old.rs", b"old")]));
        let owner = registry.register_view("local", temp.path()).unwrap();
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
        let owner = registry.register_view("local", &storage).unwrap();
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
                let sibling = registry.register_view("sibling", temp.path()).unwrap();
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
        let owner = registry.register_view("local", temp.path()).unwrap();
        let access = ViewAccess::Owner(owner);
        let snapshot =
            LiveDelta::new(Arc::new(OpenGeneration::new("test", manifest(&[]), None))).snapshot();
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
}

/// Supplies the same full membership as the configured lexical walker. Runtime
/// adapters must return walker errors rather than silently truncating membership.
pub trait MembershipWalker: Send + Sync {
    fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError>;
}

struct InstalledCheckout {
    revision: u64,
    snapshot: Snapshot,
    prepared_generation: Option<String>,
    uncertain_paths: std::collections::BTreeSet<std::path::PathBuf>,
    own_installed: bool,
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
    observed: std::sync::Mutex<BTreeMap<RelPath, LiveEntry>>,
    installed: std::sync::Mutex<InstalledCheckout>,
}

impl CheckoutDriver {
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
        let mut delta =
            super::snapshot::LiveDelta::new(Arc::new(OpenGeneration::new("empty", empty, None)));
        delta.set_watcher(super::snapshot::WatcherState::Reconciling);
        Self {
            owner,
            producers,
            head_tree,
            walker,
            planes,
            adapters: Vec::new(),
            observed: std::sync::Mutex::new(BTreeMap::new()),
            installed: std::sync::Mutex::new(InstalledCheckout {
                revision: 0,
                snapshot: delta.snapshot(),
                prepared_generation: None,
                uncertain_paths: std::collections::BTreeSet::new(),
                own_installed: false,
            }),
        }
    }

    /// Registers resident plane readers for atomic installation with snapshots.
    /// The driver remains opt-in; this does not change process-global routing.
    pub fn with_adapters(mut self, adapters: Vec<Arc<dyn super::contracts::PlaneAdapter>>) -> Self {
        self.adapters = adapters;
        self
    }

    /// Invalidates in-flight walks/builds and records an unreflected path under
    /// the same lock as installation. This method performs no filesystem writes.
    pub fn record_change(&self, path: RelPath) {
        let mut installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        installed.revision += 1;
        // Keep prior membership until reconciliation, but mark the edited path
        // as unreflected so queries cannot report stale data as installed.
        let mut delta =
            super::snapshot::LiveDelta::new(Arc::clone(installed.snapshot.generation()));
        for (path, entry) in installed.snapshot.live_entries() {
            delta.apply(path.clone(), entry.clone());
        }
        for pending in installed.snapshot.pending_intent() {
            delta.record_intent(pending.clone());
        }
        delta.record_intent(path);
        installed.snapshot = delta.snapshot();
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
}

impl FirstLoadDriver for CheckoutDriver {
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
            return Err(SiblingLoader::error(
                "checkout write still active during reconciliation",
            ));
        }
        let revision = self.revision(access);
        let mut entries = BTreeMap::new();
        // No git diff or stat shortcut: copied trees and non-git roots take
        // the same complete content walk as every other checkout kind.
        for absolute in self.walker.files(self.owner.root())? {
            let relative = absolute
                .strip_prefix(self.owner.root())
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
            let path = RelPath::from_os_path(relative)
                .map_err(|error| SiblingLoader::error(error.to_string()))?;
            let bytes = std::fs::read(&absolute).map_err(|error| {
                SiblingLoader::error(format!("{}: {error}", absolute.display()))
            })?;
            let mut entry = LiveEntry::new(super::snapshot::DiskState::of_bytes(&bytes), revision);
            for plane in &self.planes {
                if plane.applies_to(&path) {
                    entry
                        .attachments
                        .insert(plane.plane(), plane.attachment(&path, &bytes)?);
                }
            }
            entries.insert(path, entry);
        }
        *self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entries.clone();
        Ok(ReconciledCheckout { revision, entries })
    }
    fn install(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        revision: u64,
    ) -> Result<(), PlaneError> {
        self.check_owner(access)?;
        let mut installed = self
            .installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if revision != installed.revision {
            return Err(SiblingLoader::error(
                "checkout changed before snapshot installation",
            ));
        }
        let own_installed =
            installed.prepared_generation.as_deref() == Some(snapshot.generation().name());
        for adapter in &self.adapters {
            if snapshot.generation().name() != "empty"
                && (adapter.plane() != FamilyPlane::Callgraph || own_installed)
            {
                adapter.open_generation(access, snapshot.generation())?;
            }
        }
        let old_name = installed.snapshot.generation().name().to_owned();
        installed.snapshot = snapshot.clone();
        installed.uncertain_paths.clear();
        installed.own_installed = own_installed;
        if old_name != snapshot.generation().name() {
            for adapter in &self.adapters {
                adapter.release_generation(access, &old_name);
            }
        }
        Ok(())
    }
    fn build_own_generation(
        &self,
        access: &ViewAccess,
        snapshot: &Snapshot,
        seed_derived: bool,
    ) -> Result<Arc<OpenGeneration>, PlaneError> {
        self.check_owner(access)?;
        let store = self
            .owner
            .view_store()
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let base = store
            .current_generation()
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let mut manifest = snapshot.generation().manifest().clone();
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
        for plane in &self.planes {
            plane.finish_generation(&self.owner, &assembly_name, &name.to_string())?;
        }
        let prepared = store
            .prepare_v2(&name, base.as_deref(), &manifest, None)
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        store
            .commit_v2(prepared, None)
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        self.owner
            .registry()
            .note_publish(self.owner.scope())
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let reader = self
            .owner
            .registry()
            .register_reader("installed-owner")
            .map_err(|error| SiblingLoader::error(error.to_string()))?;
        let generation =
            super::read::open_foreign_generation(&reader, self.owner.scope(), &self.producers)
                .map_err(|error| SiblingLoader::error(error.to_string()))?
                .ok_or_else(|| {
                    SiblingLoader::error("own publication has no installed generation")
                })?;
        self.installed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prepared_generation = Some(generation.name().to_owned());
        Ok(generation)
    }
}

impl QueryState for CheckoutDriver {
    fn installed_state(
        &self,
        access: &ViewAccess,
        plane: FamilyPlane,
    ) -> (Snapshot, Vec<std::path::PathBuf>) {
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
                .filter(|(path, _)| applicable(path))
                .map(|(path, _)| path.clone()),
        );
        for (path, entry) in snapshot.generation().manifest().entries() {
            if applicable(path)
                && ((plane == FamilyPlane::Callgraph && !installed.own_installed)
                    || !matches!(
                        entry.plane_state(plane),
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
    use super::super::contracts::{PlaneLoader, QueryWait, WaitOutcome};
    use super::super::manifest_v2::{EntryPlanes, EntryV2, ManifestV2};
    use super::super::snapshot::{DiskState, PlaneAttachment};
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Walker {
        fail: AtomicBool,
    }
    impl MembershipWalker for Walker {
        fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError> {
            let first = root.join("file.rs");
            if self.fail.load(Ordering::SeqCst) {
                return Err(SiblingLoader::error(
                    "blocked.rs: membership walk failed after file.rs",
                ));
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
        let owner = registry.register_view("local", root).unwrap();
        let walker = Arc::new(Walker {
            fail: AtomicBool::new(false),
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
    fn composite_install_losing_revision_race_refuses_obsolete_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let (driver, _, _, access) = fixture(root.path(), storage.path());
        let checkout = driver.reconcile(&access).unwrap();
        let snapshot = driver.installed_state(&access, FamilyPlane::Trigram).0;
        driver.record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        let error = driver
            .install(&access, &snapshot, checkout.revision)
            .unwrap_err();
        assert!(error
            .reason
            .contains("changed before snapshot installation"));
        assert_eq!(
            driver.installed_state(&access, FamilyPlane::Trigram).1,
            vec![root.path().join("file.rs")]
        );
    }
}

/// Bridges the ruled callgraph extractor/materializer into the composite loader.
/// Pass `adapter` to `CheckoutDriver::with_adapters`; use `callgraph::PRODUCER`
/// in the driver's producer header. No global registration occurs here.
#[derive(Default)]
pub struct CallgraphBridge {
    pub adapter: Arc<super::callgraph::CallgraphPlane>,
}

impl CompositePlane for CallgraphBridge {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Callgraph
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        use super::contracts::PlaneAdapter;
        self.adapter.applies_to(path)
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
                plane: FamilyPlane::Callgraph,
                reason: error.to_string(),
            })?;
            format!(
                "{:?}",
                crate::parser::detect_language(std::path::Path::new(path)).ok_or_else(|| {
                    PlaneError {
                        plane: FamilyPlane::Callgraph,
                        reason: format!("unsupported callgraph member: {path}"),
                    }
                })?
            )
            .to_lowercase()
        };
        let mut entry = LiveEntry::new(super::snapshot::DiskState::of_bytes(bytes), 0);
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
            plane: FamilyPlane::Callgraph,
            reason,
        };
        if seed_derived {
            return Err(error("derived callgraph seed reuse is not enabled".into()));
        }
        let store = owner
            .open_store(FamilyPlane::Callgraph)
            .map_err(|e| error(e.to_string()))?;
        for (path, entry) in observed {
            if entry.disk == super::snapshot::DiskState::Absent || !self.applies_to(path) {
                continue;
            }
            let attachment = entry
                .attachments
                .get(&FamilyPlane::Callgraph)
                .and_then(|value| value.downcast_ref::<super::callgraph::CallgraphAttachment>())
                .ok_or_else(|| error(format!("{path:?}: callgraph attachment missing")))?;
            live.protect(&[attachment.key])
                .map_err(|e| error(e.to_string()))?;
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
        super::callgraph::materialize(&database, manifest, &reader)
    }
    fn finish_generation(
        &self,
        owner: &super::registry::ViewRegistration,
        staging: &str,
        published: &str,
    ) -> Result<(), PlaneError> {
        let error = |e: super::ViewError| PlaneError {
            plane: FamilyPlane::Callgraph,
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
                plane: FamilyPlane::Callgraph,
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
                    plane: FamilyPlane::Callgraph,
                    reason: "private callgraph install would replace a live database".into(),
                });
            }
            crate::db::file_identity::guard_replacement(&staging_path, "first-load derived rename");
            std::fs::rename(&staging_path, &published_path).map_err(|e| error(e.into()))?;
        }
        super::sync_directory(owner.view_dir()).map_err(|e| error(e.into()))?;
        Ok(())
    }
}

#[cfg(test)]
mod callgraph_bridge_tests {
    use super::super::contracts::PlaneAdapter;
    use super::*;

    struct Walker;
    impl MembershipWalker for Walker {
        fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError> {
            Ok(vec![root.join("file.rs")])
        }
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
        let owner = registry.register_view("local", root.path()).unwrap();
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
    }
}

/// Configured walker membership with traversal errors retained. Source content
/// reads belong exclusively to CheckoutDriver's strict same-buffer reconciliation.
pub struct ConfiguredMembershipWalker;
impl MembershipWalker for ConfiguredMembershipWalker {
    fn files(&self, root: &std::path::Path) -> Result<Vec<std::path::PathBuf>, PlaneError> {
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
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Trigram
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        !path.is_synthetic()
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
}

impl CheckoutDriver {
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
                    let mut installed = self
                        .installed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    installed.revision += 1;
                    installed.uncertain_paths.insert(absolute.clone());
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
        let owner = registry.register_view("local", root.path()).unwrap();
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
            let owner = registry.register_view(scope, &root).unwrap();
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
