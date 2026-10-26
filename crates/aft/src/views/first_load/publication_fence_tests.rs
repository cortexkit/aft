use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct LeaseFence {
    live: Mutex<bool>,
    calls: AtomicUsize,
    policy_change: Mutex<Option<PathBuf>>,
}
impl LeaseFence {
    fn live() -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(true),
            calls: AtomicUsize::new(0),
            policy_change: Mutex::new(None),
        })
    }
    fn revoke(&self) {
        *self.live.lock().unwrap() = false;
    }
}
impl PublishFence for LeaseFence {
    fn publish(
        &self,
        commit: &mut dyn FnMut() -> Result<(), PlaneError>,
    ) -> Result<(), PlaneError> {
        let live = self.live.lock().unwrap();
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !*live {
            return Err(SiblingLoader::error("driver ownership retired"));
        }
        if let Some(path) = self.policy_change.lock().unwrap().take() {
            std::fs::write(path, b"*.rs text\n").unwrap();
        }
        commit()
    }
}

struct Fixture {
    root: tempfile::TempDir,
    storage: tempfile::TempDir,
    driver: CheckoutDriver,
    access: ViewAccess,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.rs"), b"fn before() {}\n").unwrap();
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
                semantic: None,
                callgraph: "unregistered".into(),
            },
            Arc::new(ConfiguredMembershipWalker),
            vec![Arc::new(TrigramBridge::new(
                storage.path().to_owned(),
                policy,
            ))],
        )
        .unwrap();
        let access = driver.access();
        Self {
            root,
            storage,
            driver,
            access,
        }
    }
    fn bootstrap(&self) {
        let (snapshot, revision) = self
            .driver
            .verified_bind(
                &self.access,
                self.driver.installed_snapshot().generation().clone(),
            )
            .unwrap();
        self.driver
            .install(&self.access, &snapshot, revision)
            .unwrap();
    }
    fn pointer(&self) -> Option<String> {
        super::super::ViewStore::existing_dir(self.driver.owner.v3_route().member_dir())
            .map(|store| store.current_generation_read_only().unwrap())
            .flatten()
    }
    fn validated_change(&self) -> Snapshot {
        std::fs::write(
            self.root.path().join("file.rs"),
            b"fn after() { let changed = true; }\n",
        )
        .unwrap();
        self.driver
            .record_change(RelPath::new(b"file.rs".to_vec()).unwrap());
        let (snapshot, revision) = self
            .driver
            .verified_bind(
                &self.access,
                self.driver.installed_snapshot().generation().clone(),
            )
            .unwrap();
        self.driver
            .validate_cut(&self.access, &snapshot, revision)
            .unwrap();
        snapshot
    }
}

#[test]
fn v3_cas_fence_rejects_retirement_after_validation_and_allows_a_live_driver() {
    let _serial = crate::blob_store::v3::serial_for_test();
    for retired in [true, false] {
        let fixture = Fixture::new();
        fixture.bootstrap();
        let previous = fixture.pointer();
        let fence = LeaseFence::live();
        fixture.driver.set_publish_fence(fence.clone());
        let snapshot = fixture.validated_change();
        if retired {
            fence.revoke();
        }
        let result = fixture
            .driver
            .build_own_generation(&fixture.access, &snapshot, false);
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        if retired {
            assert!(result.is_err());
            assert_eq!(fixture.pointer(), previous);
        } else {
            let generation = result.unwrap();
            assert_ne!(fixture.pointer(), previous);
            assert_eq!(fixture.pointer().as_deref(), Some(generation.name()));
        }
    }
}

#[test]
fn v3_final_metadata_validation_runs_inside_the_ownership_fence() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let fixture = Fixture::new();
    fixture.bootstrap();
    let previous = fixture.pointer();
    let fence = LeaseFence::live();
    fixture.driver.set_publish_fence(fence.clone());
    let snapshot = fixture.validated_change();
    *fence.policy_change.lock().unwrap() = Some(fixture.root.path().join(".gitattributes"));
    assert!(fixture
        .driver
        .build_own_generation(&fixture.access, &snapshot, false)
        .is_err());
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.pointer(), previous);
}

#[test]
fn refused_fresh_v3_publication_releases_the_completed_lexical_claim() {
    let _serial = crate::blob_store::v3::serial_for_test();
    let fixture = Fixture::new();
    let fence = LeaseFence::live();
    fence.revoke();
    fixture.driver.set_publish_fence(fence);
    assert!(fixture
        .driver
        .verified_bind(
            &fixture.access,
            fixture.driver.installed_snapshot().generation().clone()
        )
        .is_err());
    assert!(fixture.pointer().is_none());
    let registry =
        crate::blob_store::v3::LedgerReader::open_existing(fixture.storage.path(), "family")
            .unwrap();
    assert_eq!(
        registry
            .read(|c| Ok(
                c.query_row("SELECT COUNT(*) FROM base_builds", [], |row| row
                    .get::<_, u64>(0))?
            ))
            .unwrap(),
        0
    );
}
