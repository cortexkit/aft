//! Copy-on-first-use. Every source row here is referenced by no manifest, so
//! only a pin can keep it through a sweep.

use super::copy::{SKIP_LEASE_CHECK, SKIP_PIN};
use super::tests::serial;
use super::*;
use crate::blob_store::{BlobPlane, BlobStore, FullKey};
use crate::gc::family::{sweep_family, FamilySweepPolicy, SweepObserver, SweepStep};
use crate::pins::AssemblyPin;
use crate::views::registry::{FamilyRegistry, ViewRegistration};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;
use tempfile::tempdir;

const FAMILY: &str = "0123456789abcdef";
const HOLDER: &str = "holder";
const COLLECT_ALL: FamilySweepPolicy = FamilySweepPolicy { byte_budget: 0 };
const WAIT: Duration = Duration::from_secs(120);

struct V2Source {
    registry: FamilyRegistry,
    registration: ViewRegistration,
    store: crate::blob_store::v2::FamilyStore,
    key: FamilyKey,
}

fn v2_source(storage: &Path, payload: &[u8]) -> V2Source {
    let registry = FamilyRegistry::open(storage, FAMILY).unwrap();
    let root = storage.join("checkout");
    fs::create_dir_all(&root).unwrap();
    let registration = registry
        .register_view_with_storage(storage, HOLDER, &root)
        .unwrap();
    fs::create_dir_all(registration.view_dir().join("pins")).unwrap();
    let store = registration.open_store(FamilyPlane::Semantic).unwrap();
    let key = FamilyKey::new(FamilyPlane::Semantic, *blake3::hash(payload).as_bytes());
    store.put_or_touch(&key, payload).unwrap();
    V2Source {
        registry,
        registration,
        store,
        key,
    }
}

fn v1_holder(storage: &Path) -> PathBuf {
    let dir = storage.join("views").join(HOLDER);
    fs::create_dir_all(dir.join("pins")).unwrap();
    dir
}

fn v1_source(storage: &Path, payload: &[u8]) -> (BlobStore, [u8; 32]) {
    let mut store = BlobStore::open(storage, FAMILY, BlobPlane::Semantic).unwrap();
    let key = *blake3::hash(payload).as_bytes();
    store
        .put(
            &FullKey {
                bytes: key,
                plane: BlobPlane::Semantic,
            },
            payload,
        )
        .unwrap();
    v1_holder(storage);
    (store, key)
}

fn v1_contains(store: &BlobStore, key: [u8; 32]) -> bool {
    store
        .contains(&FullKey {
            bytes: key,
            plane: BlobPlane::Semantic,
        })
        .unwrap()
}

fn v1_sweep(storage: &Path) -> crate::gc::SweepReport {
    crate::gc::sweep(crate::gc::SweepRequest {
        durable_storage: storage,
        storage,
        family: FAMILY,
        view_dir: &storage.join("views").join(HOLDER),
        byte_budget: 0,
        now_ms: crate::pins::now_ms() + crate::gc::BLOB_AGE_FLOOR_MS + 1,
        references: Default::default(),
    })
    .unwrap()
}

fn v3_get(storage: &Path, key: &FamilyKey) -> Option<Vec<u8>> {
    PlaneReader::open_existing(storage, FAMILY, key.plane())
        .unwrap()
        .get(key)
        .unwrap()
}

fn copy_pins(pins: &Path) -> Vec<String> {
    fs::read_dir(pins)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("v3-copy-") && name.ends_with(".json"))
        .collect()
}

/// Parks the copier at `V3FloorRaised`: its pin exists, it has not read the
/// source payload yet, and nothing under `blobs/v3/` changed.
struct Park {
    parked: Sender<()>,
    go: Mutex<Receiver<()>>,
}
impl DurabilityObserver for Park {
    fn reached(&self, step: DurabilityStep) {
        if step == DurabilityStep::V3FloorRaised {
            self.parked.send(()).unwrap();
            self.go
                .lock()
                .unwrap()
                .recv_timeout(WAIT)
                .expect("the test never released the parked copier");
        }
    }
}

struct Copier {
    parked: Receiver<()>,
    go: Sender<()>,
    thread: std::thread::JoinHandle<Result<CopyOutcome>>,
}
impl Copier {
    fn spawn(
        storage: &Path,
        source: CopySource<'static>,
        skip_pin: bool,
        skip_lease_check: bool,
    ) -> Self {
        let storage = storage.to_path_buf();
        let (parked_tx, parked) = mpsc::channel();
        let (go, go_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            SKIP_PIN.with(|flag| flag.set(skip_pin));
            SKIP_LEASE_CHECK.with(|flag| flag.set(skip_lease_check));
            let observer = Park {
                parked: parked_tx,
                go: Mutex::new(go_rx),
            };
            copy_on_first_use(&storage, source, HOLDER, Some(&observer))
        });
        Self { parked, go, thread }
    }
    fn wait_parked(&self) {
        self.parked
            .recv_timeout(WAIT)
            .expect("the copier never reached V3FloorRaised");
    }
    fn release(self) -> Result<CopyOutcome> {
        let _ = self.go.send(());
        self.thread.join().unwrap()
    }
}

/// Pauses a sweep thread at one step until released.
struct PauseAt {
    step: SweepStep,
    reached: Sender<()>,
    go: Mutex<Receiver<()>>,
}
impl SweepObserver for PauseAt {
    fn reached(&self, step: SweepStep) {
        if step == self.step {
            self.reached.send(()).unwrap();
            self.go
                .lock()
                .unwrap()
                .recv_timeout(WAIT)
                .expect("the test never released the paused sweep");
        }
    }
}

struct PausedSweep {
    go: Sender<()>,
    thread: std::thread::JoinHandle<crate::gc::family::FamilySweepReport>,
}
impl PausedSweep {
    fn after_marking(registry: &FamilyRegistry) -> Self {
        let registry = registry.clone();
        let (reached_tx, reached) = mpsc::channel();
        let (go, go_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let observer = PauseAt {
                step: SweepStep::Marked,
                reached: reached_tx,
                go: Mutex::new(go_rx),
            };
            sweep_family(&registry, None, COLLECT_ALL, Some(&observer)).unwrap()
        });
        reached
            .recv_timeout(WAIT)
            .expect("the sweep never finished marking");
        Self { go, thread }
    }
    fn finish(self) -> crate::gc::family::FamilySweepReport {
        self.go.send(()).unwrap();
        self.thread.join().unwrap()
    }
}

/// While a sweep holds its lease, the copier must make no pin and must not
/// reach its publication. Watches for a bounded interval.
fn assert_no_pin_while_lease_is_live(pins: &Path, copier: &Copier) {
    let until = Instant::now() + Duration::from_millis(400);
    while Instant::now() < until {
        assert!(
            copy_pins(pins).is_empty(),
            "the copier pinned while a sweep held the lease"
        );
        assert!(copier.parked.try_recv().is_err());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn copy_red_control_switches_are_off_by_default() {
    std::thread::spawn(|| {
        assert!(!SKIP_PIN.with(std::cell::Cell::get));
        assert!(!SKIP_LEASE_CHECK.with(std::cell::Cell::get));
        assert!(!super::replay::unflushed_closure());
    })
    .join()
    .unwrap();
    assert!(!SKIP_PIN.with(std::cell::Cell::get));
    assert!(!SKIP_LEASE_CHECK.with(std::cell::Cell::get));
    assert!(!super::replay::unflushed_closure());
}

#[test]
fn a_copied_row_reads_byte_identical_after_its_source_is_deleted() {
    let _guard = serial();
    // v2 source, deleted through the store's own connection.
    let root = tempdir().unwrap();
    let payload = b"v2 payload copied on first use".to_vec();
    let source = v2_source(root.path(), &payload);
    let copy = CopySource::V2 {
        family: FAMILY,
        key: source.key,
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap(),
        CopyOutcome::Copied
    );
    assert!(source
        .store
        .delete_if_unreferenced_since(&source.key, i64::MAX as u64)
        .unwrap());
    assert_eq!(source.store.get(&source.key).unwrap(), None);
    assert_eq!(v3_get(root.path(), &source.key), Some(payload));
    assert!(copy_pins(&source.registration.view_dir().join("pins")).is_empty());

    // v1 source with an unusual payload_schema, deleted through the owning
    // BlobStore connection. The copy keeps the stored schema.
    let root = tempdir().unwrap();
    let payload = b"v1 payload copied on first use".to_vec();
    let store = BlobStore::open(root.path(), FAMILY, BlobPlane::Callgraph).unwrap();
    let key = *blake3::hash(&payload).as_bytes();
    store
        .connection
        .execute(
            "INSERT INTO blob_payloads (full_key, payload, payload_digest, payload_schema, created_at_ms)
             VALUES (?1, ?2, ?3, 9, 0)",
            params![key.as_slice(), payload, blake3::hash(&payload).as_bytes().as_slice()],
        )
        .unwrap();
    let pins = v1_holder(root.path()).join("pins");
    let copy = CopySource::V1 {
        artifact_key: FAMILY,
        plane: BlobPlane::Callgraph,
        key,
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap(),
        CopyOutcome::Copied
    );
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap(),
        CopyOutcome::Reused
    );
    store
        .connection
        .execute(
            "DELETE FROM blob_payloads WHERE full_key = ?1",
            [key.as_slice()],
        )
        .unwrap();
    let v3_key = FamilyKey::new(FamilyPlane::Callgraph, key);
    assert_eq!(v3_get(root.path(), &v3_key), Some(payload));
    let reader = PlaneReader::open_existing(root.path(), FAMILY, FamilyPlane::Callgraph).unwrap();
    let schema: i64 = reader
        .database
        .connection()
        .unwrap()
        .query_row(
            "SELECT payload_schema FROM blob_payloads WHERE full_key = ?1",
            [key.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema, 9);
    assert!(copy_pins(&pins).is_empty());
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap(),
        CopyOutcome::SourceMissing
    );
}

#[test]
fn a_source_whose_digest_does_not_match_is_not_copied() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let store = BlobStore::open(root.path(), FAMILY, BlobPlane::Semantic).unwrap();
    let key = [4; 32];
    store
        .connection
        .execute(
            "INSERT INTO blob_payloads (full_key, payload, payload_digest, payload_schema, created_at_ms)
             VALUES (?1, X'0102', ?2, 1, 0)",
            params![key.as_slice(), [0u8; 32].as_slice()],
        )
        .unwrap();
    v1_holder(root.path());
    let copy = CopySource::V1 {
        artifact_key: FAMILY,
        plane: BlobPlane::Semantic,
        key,
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None)
            .unwrap_err()
            .to_string(),
        "payload_digest"
    );
    // The refusal comes before the copy's first plane write, so no v3 plane
    // store holds the row.
    assert!(!plane_path(root.path(), FAMILY, FamilyPlane::Semantic)
        .unwrap()
        .exists());
}

#[test]
fn a_copy_needs_a_registered_holder_and_writes_nothing_without_one() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let source = v2_source(root.path(), b"v2 payload");
    let error = copy_on_first_use(
        root.path(),
        CopySource::V2 {
            family: FAMILY,
            key: source.key,
        },
        "not-a-member",
        None,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "copy_holder_not_registered");
    let (_store, key) = v1_source(root.path(), b"v1 payload");
    let error = copy_on_first_use(
        root.path(),
        CopySource::V1 {
            artifact_key: FAMILY,
            plane: BlobPlane::Semantic,
            key,
        },
        "not-a-v1-scope",
        None,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "copy_holder_not_registered");
    assert!(!root.path().join("views/not-a-v1-scope").exists());
    assert!(!root.path().join("blobs/v3").exists());
}

type Rows = Vec<(Vec<u8>, Vec<u8>, i64)>;
fn sqlite_state(path: &Path, rows_sql: &str, meta_sql: &str) -> (Rows, Vec<String>, i64, String) {
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let rows = c
        .prepare(rows_sql)
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let meta = c
        .prepare(meta_sql)
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let schema_version = c
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    let journal = c
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    (rows, meta, schema_version, journal)
}

/// Every entry under `dir` with its length, so a pin left behind or a
/// pointer database rewrite shows up.
fn tree(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            entries.push((path.clone(), 0));
            entries.extend(tree(&path));
        } else {
            entries.push((path, meta.len()));
        }
    }
    entries.sort();
    entries
}

#[test]
fn a_copy_leaves_v1_and_v2_rows_metadata_schema_and_view_state_unchanged() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let source = v2_source(root.path(), b"v2 payload left unchanged");
    let other = FamilyKey::new(FamilyPlane::Semantic, [8; 32]);
    let v2_path =
        crate::blob_store::v2::plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap();
    let v2_state = || {
        sqlite_state(
            &v2_path,
            "SELECT full_key, payload_digest, ref_epoch FROM blob_payloads ORDER BY full_key",
            "SELECT schema_version || ':' || plane || ':' || gc_epoch FROM store_meta",
        )
    };
    let (_v1_store, v1_key) = v1_source(root.path(), b"v1 payload left unchanged");
    let v1_path = root
        .path()
        .join("blobs")
        .join(FAMILY)
        .join("semantic.sqlite");
    let v1_state = || {
        sqlite_state(
            &v1_path,
            "SELECT full_key, payload_digest, payload_schema FROM blob_payloads ORDER BY full_key",
            "SELECT hex(full_key) || ':' || created_at_ms FROM blob_payloads ORDER BY full_key",
        )
    };
    let v2_view = source.registration.view_dir().to_path_buf();
    let v1_view = root.path().join("views").join(HOLDER);
    let pointer = source
        .registration
        .view_store()
        .unwrap()
        .current_generation()
        .unwrap();
    let (v2_before, v1_before) = (v2_state(), v1_state());
    let (v2_tree, v1_tree) = (tree(&v2_view), tree(&v1_view));

    for copy in [
        CopySource::V2 {
            family: FAMILY,
            key: source.key,
        },
        CopySource::V1 {
            artifact_key: FAMILY,
            plane: BlobPlane::Semantic,
            key: v1_key,
        },
    ] {
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap();
    }

    assert_eq!(v2_state(), v2_before);
    assert_eq!(v1_state(), v1_before);
    assert_eq!(v1_before.3, "wal");
    assert_eq!(tree(&v2_view), v2_tree);
    assert_eq!(tree(&v1_view), v1_tree);
    assert_eq!(
        source
            .registration
            .view_store()
            .unwrap()
            .current_generation()
            .unwrap(),
        pointer
    );
    // A v2 write after the copy still finds the schema it always had.
    let schema = || {
        Connection::open_with_flags(&v2_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY name")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let before = schema();
    source
        .store
        .put_or_touch(&other, b"later v2 write")
        .unwrap();
    assert_eq!(schema(), before);
}

/// The copier's pin is the only thing keeping an unreferenced source while
/// the copier is parked between pinning and reading it.
fn parked_copier_keeps_source(skip_pin: bool) -> (bool, bool, Result<CopyOutcome>) {
    let root = tempdir().unwrap();
    let source = v2_source(root.path(), b"v2 source kept by the copier pin");
    let copier = Copier::spawn(
        root.path(),
        CopySource::V2 {
            family: FAMILY,
            key: source.key,
        },
        skip_pin,
        false,
    );
    copier.wait_parked();
    sweep_family(&source.registry, None, COLLECT_ALL, None).unwrap();
    let v2_kept = source.store.contains(&source.key).unwrap();
    let v2 = copier.release();
    assert_eq!(v2.is_ok_and(|o| o == CopyOutcome::Copied), v2_kept);

    let (store, key) = v1_source(root.path(), b"v1 source kept by the copier pin");
    let copier = Copier::spawn(
        root.path(),
        CopySource::V1 {
            artifact_key: FAMILY,
            plane: BlobPlane::Semantic,
            key,
        },
        skip_pin,
        false,
    );
    copier.wait_parked();
    v1_sweep(root.path());
    let v1_kept = v1_contains(&store, key);
    let v1 = copier.release();
    (v2_kept, v1_kept, v1)
}

#[test]
fn a_parked_copier_keeps_its_source_through_v1_and_v2_sweeps() {
    let _guard = serial();
    let (v2_kept, v1_kept, v1) = parked_copier_keeps_source(false);
    assert!(v2_kept, "a COLLECT_ALL v2 sweep deleted a pinned source");
    assert!(v1_kept, "a v1 sweep deleted a pinned source");
    assert_eq!(v1.unwrap(), CopyOutcome::Copied);
}

#[test]
fn red_control_without_the_pin_a_parked_copier_loses_its_source() {
    let _guard = serial();
    let (v2_kept, v1_kept, v1) = parked_copier_keeps_source(true);
    assert!(
        !v2_kept,
        "the v2 fixture source survived without protection"
    );
    assert!(
        !v1_kept,
        "the v1 fixture source survived without protection"
    );
    assert_eq!(v1.unwrap(), CopyOutcome::SourceMissing);
}

/// A sweep's marked set is fixed before `SweepStep::Marked`, so a copy can
/// succeed across a sweep only for a source that was protected when the
/// sweep marked; here a fixture pin stands in for the live view reference
/// that protects a real source. An unprotected source is lost to that sweep
/// whatever the copier does, and the copier then reports `SourceMissing`.
#[test]
fn a_copier_waits_out_a_live_sweep_lease_before_pinning() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let source = v2_source(root.path(), b"v2 source referenced while the sweep marks");
    let pins = source.registration.view_dir().join("pins");
    let reference =
        AssemblyPin::create_v2(&source.registration, "fixture-reference", &[source.key]).unwrap();
    let garbage = v2_source(root.path(), b"unreferenced sweep candidate");
    let sweep = PausedSweep::after_marking(&source.registry);
    drop(reference);
    let copier = Copier::spawn(
        root.path(),
        CopySource::V2 {
            family: FAMILY,
            key: source.key,
        },
        false,
        false,
    );
    assert_no_pin_while_lease_is_live(&pins, &copier);
    sweep.finish();
    assert!(!garbage.store.contains(&garbage.key).unwrap());
    assert!(source.store.contains(&source.key).unwrap());
    copier.wait_parked();
    assert_eq!(copy_pins(&pins).len(), 1);
    // The next sweep sees the copier's pin.
    sweep_family(&source.registry, None, COLLECT_ALL, None).unwrap();
    assert!(source.store.contains(&source.key).unwrap());
    assert_eq!(copier.release().unwrap(), CopyOutcome::Copied);
    assert!(source.store.contains(&source.key).unwrap());
    assert!(copy_pins(&pins).is_empty());

    // Unprotected source: the correct copier still pins only after the
    // lease, and honestly reports the source the sweep took.
    let root = tempdir().unwrap();
    let source = v2_source(root.path(), b"v2 source nothing references");
    let pins = source.registration.view_dir().join("pins");
    let sweep = PausedSweep::after_marking(&source.registry);
    let copier = Copier::spawn(
        root.path(),
        CopySource::V2 {
            family: FAMILY,
            key: source.key,
        },
        false,
        false,
    );
    assert_no_pin_while_lease_is_live(&pins, &copier);
    sweep.finish();
    assert!(!source.store.contains(&source.key).unwrap());
    assert_eq!(copier.release().unwrap(), CopyOutcome::SourceMissing);
    assert!(copy_pins(&pins).is_empty());
}

#[test]
fn red_control_a_copier_that_ignores_the_lease_loses_its_source() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let source = v2_source(root.path(), b"v2 source pinned during a sweep");
    let pins = source.registration.view_dir().join("pins");
    let sweep = PausedSweep::after_marking(&source.registry);
    let copier = Copier::spawn(
        root.path(),
        CopySource::V2 {
            family: FAMILY,
            key: source.key,
        },
        false,
        true,
    );
    copier.wait_parked();
    assert_eq!(
        copy_pins(&pins).len(),
        1,
        "the red copier pins while the lease is live"
    );
    sweep.finish();
    assert!(
        !source.store.contains(&source.key).unwrap(),
        "a pin made after marking must not have protected the source"
    );
    assert_eq!(copier.release().unwrap(), CopyOutcome::SourceMissing);
}

#[cfg(unix)]
#[test]
fn deleting_copied_rows_lowers_usage_only_at_the_next_covering_measurement() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let payload = vec![3u8; 8 * 1024 * 1024];
    let (_store, key) = v1_source(root.path(), &payload);
    assert_eq!(
        copy_on_first_use(
            root.path(),
            CopySource::V1 {
                artifact_key: FAMILY,
                plane: BlobPlane::Semantic,
                key,
            },
            HOLDER,
            None,
        )
        .unwrap(),
        CopyOutcome::Copied
    );
    let lock = ReservationLock::acquire(root.path()).unwrap();
    let mut registry = RegistryWriter::open_existing(root.path(), FAMILY, &lock).unwrap();
    let before = registry.accounting().unwrap().usage();
    let path = plane_path(root.path(), FAMILY, FamilyPlane::Semantic).unwrap();
    {
        // The plane store's owning connection: no other v3 handle is open.
        let database =
            connections::shared(&path, Kind::Plane(FamilyPlane::Semantic), false, false).unwrap();
        let c = database.connection().unwrap();
        assert_eq!(
            c.execute(
                "DELETE FROM blob_payloads WHERE full_key = ?1",
                [key.as_slice()]
            )
            .unwrap(),
            1
        );
        checkpoint(&c).unwrap();
        // Bounded passes of the shared reclaim, as reclaim_space_bounded runs
        // for v2 stores, until no free page is left to return.
        while super::super::reclaim::reclaim_all_free_pages(
            &c,
            None,
            super::super::reclaim::ALL_FREE_PAGES_MAX_PAGES,
        )
        .unwrap()
        .reclaimed_pages
            > 0
        {}
        checkpoint(&c).unwrap();
    }
    let fits_only_after_freeing = |freed: u64| NAMESPACE_BUDGET - before + freed;
    assert_eq!(registry.accounting().unwrap().usage(), before);
    assert_eq!(
        registry
            .reserve(fits_only_after_freeing(1024 * 1024))
            .err()
            .unwrap()
            .to_string(),
        "v3_namespace_budget"
    );
    registry.covering_measurement().unwrap();
    let after = registry.accounting().unwrap().usage();
    assert_eq!(after, super::tests::inventory(&registry.dir));
    let freed = before - after;
    assert!(
        freed >= payload.len() as u64,
        "freed {freed} bytes of an {} byte row",
        payload.len()
    );
    let reservation = registry.reserve(fits_only_after_freeing(freed)).unwrap();
    registry.release(reservation).unwrap();
}

/// Records, at each `ClosureFlushed`, the ledger's outstanding reserved bytes
/// and whether v3 holds the copied row. At its first `V3FloorRaised` it can
/// delete that row through the plane store's owning connection, to model a
/// row that disappears between the reuse check and the touch.
struct LedgerProbe<'a> {
    storage: &'a Path,
    key: FamilyKey,
    delete_at_first_floor: bool,
    floors: std::cell::Cell<usize>,
    flushed: std::cell::RefCell<Vec<(u64, bool)>>,
}
impl DurabilityObserver for LedgerProbe<'_> {
    fn reached(&self, step: DurabilityStep) {
        match step {
            DurabilityStep::V3FloorRaised => {
                self.floors.set(self.floors.get() + 1);
                if self.delete_at_first_floor && self.floors.get() == 1 {
                    let path = plane_path(self.storage, FAMILY, self.key.plane()).unwrap();
                    let database =
                        connections::shared(&path, Kind::Plane(self.key.plane()), false, false)
                            .unwrap();
                    let c = database.connection().unwrap();
                    c.execute(
                        "DELETE FROM blob_payloads WHERE full_key = ?1",
                        [self.key.as_bytes().as_slice()],
                    )
                    .unwrap();
                    checkpoint(&c).unwrap();
                }
            }
            DurabilityStep::ClosureFlushed => {
                let reserved = LedgerReader::open_existing(self.storage, FAMILY)
                    .unwrap()
                    .accounting()
                    .unwrap()
                    .reserved_bytes;
                let present = v3_get(self.storage, &self.key).is_some();
                self.flushed.borrow_mut().push((reserved, present));
            }
            _ => {}
        }
    }
}

/// Creates the v3 registry and holds a live reservation that leaves room for
/// one full copy request of `payload_len` bytes but not for two. Returns the
/// held bytes.
fn leave_room_for_one_copy(storage: &Path, payload_len: u64) -> u64 {
    publish(storage, FAMILY, 0, None, |_| Ok(())).unwrap();
    let full_request = 2 * payload_len + (1 << 20);
    let lock = ReservationLock::acquire(storage).unwrap();
    let mut writer = RegistryWriter::open_existing(storage, FAMILY, &lock).unwrap();
    let usage = writer.accounting().unwrap().usage();
    let held = NAMESPACE_BUDGET - usage - full_request - full_request / 6;
    // The reservation stays outstanding: its owner (this process) is alive.
    writer.reserve(held).unwrap();
    held
}

#[test]
fn a_repeated_copy_reserves_only_a_touch_and_reuses_the_committed_row() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let payload = vec![6u8; 4 * 1024 * 1024];
    let (_store, key) = v1_source(root.path(), &payload);
    let held = leave_room_for_one_copy(root.path(), payload.len() as u64);
    let copy = CopySource::V1 {
        artifact_key: FAMILY,
        plane: BlobPlane::Semantic,
        key,
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap(),
        CopyOutcome::Copied
    );
    // A second full-size request no longer fits the namespace budget.
    let probe = LedgerProbe {
        storage: root.path(),
        key: FamilyKey::new(FamilyPlane::Semantic, key),
        delete_at_first_floor: false,
        floors: std::cell::Cell::new(0),
        flushed: std::cell::RefCell::new(Vec::new()),
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, Some(&probe)).unwrap(),
        CopyOutcome::Reused
    );
    let flushed = probe.flushed.into_inner();
    assert_eq!(flushed.len(), 1, "{flushed:?}");
    let (reserved, present) = flushed[0];
    assert!(present);
    // Liveness is a separate row, so reservation size no longer scales with
    // payload length. A disappearing row still requires a full copy attempt.
    let touch = reserved - held;
    assert_eq!(touch, copy::TOUCH_RESERVATION);
    assert_eq!(
        LedgerReader::open_existing(root.path(), FAMILY)
            .unwrap()
            .accounting()
            .unwrap()
            .reserved_bytes,
        held
    );
    assert_eq!(
        v3_get(root.path(), &FamilyKey::new(FamilyPlane::Semantic, key)),
        Some(payload)
    );
}

#[test]
fn a_row_that_vanishes_before_its_touch_is_copied_under_a_full_reservation() {
    let _guard = serial();
    let root = tempdir().unwrap();
    let payload = vec![7u8; 4 * 1024 * 1024];
    let (_store, key) = v1_source(root.path(), &payload);
    let copy = CopySource::V1 {
        artifact_key: FAMILY,
        plane: BlobPlane::Semantic,
        key,
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, None).unwrap(),
        CopyOutcome::Copied
    );
    let probe = LedgerProbe {
        storage: root.path(),
        key: FamilyKey::new(FamilyPlane::Semantic, key),
        delete_at_first_floor: true,
        floors: std::cell::Cell::new(0),
        flushed: std::cell::RefCell::new(Vec::new()),
    };
    assert_eq!(
        copy_on_first_use(root.path(), copy, HOLDER, Some(&probe)).unwrap(),
        CopyOutcome::Copied
    );
    let flushed = probe.flushed.into_inner();
    assert_eq!(flushed.len(), 2, "{flushed:?}");
    // The touch-sized attempt wrote no row; the full attempt did.
    let (touch_reserved, after_touch) = flushed[0];
    assert!(
        !after_touch,
        "a row was inserted under the touch-sized reservation"
    );
    assert!(touch_reserved == copy::TOUCH_RESERVATION, "{flushed:?}");
    let (full_reserved, after_copy) = flushed[1];
    assert!(after_copy);
    assert!(full_reserved >= 2 * payload.len() as u64, "{flushed:?}");
    assert_eq!(
        v3_get(root.path(), &FamilyKey::new(FamilyPlane::Semantic, key)),
        Some(payload)
    );
}
#[test]
fn no_production_path_calls_copy_on_first_use() {
    fn scan(dir: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && fs::read_to_string(&path)
                    .unwrap()
                    .contains("copy_on_first_use")
            {
                found.push(
                    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut found = Vec::new();
    scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    found.sort();
    assert_eq!(
        found,
        [
            "src/blob_store/v3.rs",
            "src/blob_store/v3/copy.rs",
            "src/blob_store/v3/copy_tests.rs",
            "src/blob_store/v3/layout_gate.rs",
            "src/blob_store/v3/layout_tests.rs",
            "src/blob_store/v3/replay_tests.rs",
        ]
    );
}
