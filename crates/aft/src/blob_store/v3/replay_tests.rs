//! Power-loss replay of the copy closure: v3 plane-row insert or touch,
//! `ClosureFlushed`, then the ledger transaction from `reserved` to
//! `committed`. Each case first runs the closure to count its mutating file
//! calls, then cuts power before each one in turn (and once after the end),
//! restores every file to its last synced state and reopens the stores.
//!
//! The invariant after every cut: the ledger has no row for the attempt, or
//! shows it `reserved` or `released` (claiming no rows), or shows it
//! `committed` with every copied row passing `get` with a matching digest.

#![cfg(unix)]

use super::replay;
use super::tests::serial;
use super::*;
use crate::views::registry::{FamilyRegistry, ViewRegistration};
use tempfile::{tempdir, TempDir};

const FAMILY: &str = "0123456789abcdef";
const HOLDER: &str = "holder";

struct Fixture {
    _root: TempDir,
    storage: PathBuf,
    _registration: ViewRegistration,
    key: FamilyKey,
    payload: Vec<u8>,
    /// The last reservation id written before the replayed attempt.
    seeded: i64,
    /// For a reuse fixture: the row's `ref_epoch` before the attempt and the
    /// plane's `gc_epoch`, which a durable touch must leave on the row.
    epochs: Option<(i64, i64)>,
}

/// The epoch a reuse fixture's plane store is advanced to before the replay,
/// as a sweep would, so the copy's touch has something to change.
const ADVANCED_GC_EPOCH: i64 = 5;

/// A v2 source row and a v3 family whose registry and plane store already
/// exist, all closed, so the replayed closure creates only WAL files.
/// `reuse` seeds v3 with the same row, making the closure a touch.
fn fixture(reuse: bool) -> Fixture {
    let root = tempdir().unwrap();
    let storage = root.path().to_path_buf();
    let registry = FamilyRegistry::open(&storage, FAMILY).unwrap();
    let checkout = storage.join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    let registration = registry
        .register_view_with_storage(&storage, HOLDER, &checkout)
        .unwrap();
    let payload = vec![0x5a; 64 * 1024];
    let key = FamilyKey::new(FamilyPlane::Semantic, *blake3::hash(&payload).as_bytes());
    registration
        .open_store(FamilyPlane::Semantic)
        .unwrap()
        .put_or_touch(&key, &payload)
        .unwrap();
    let (seed_key, seed_payload) = if reuse {
        (key, payload.clone())
    } else {
        (
            FamilyKey::new(FamilyPlane::Semantic, [1; 32]),
            b"seed".to_vec(),
        )
    };
    publish(&storage, FAMILY, 1 << 20, None, |p| {
        p.put(&seed_key, &seed_payload, 1)
    })
    .unwrap();
    let seeded = last_reservation(&storage);
    let epochs = reuse.then(|| {
        let path = plane_path(&storage, FAMILY, FamilyPlane::Semantic).unwrap();
        // The plane's owning connection; nothing else has it open.
        let database =
            connections::shared(&path, Kind::Plane(FamilyPlane::Semantic), false, false).unwrap();
        let c = database.connection().unwrap();
        c.execute(
            "UPDATE store_meta SET gc_epoch = ?1 WHERE singleton = 1",
            [ADVANCED_GC_EPOCH],
        )
        .unwrap();
        checkpoint(&c).unwrap();
        drop(c);
        drop(database);
        let old = ref_epoch(&storage, &key).expect("the reuse fixture holds the row");
        assert!(old < ADVANCED_GC_EPOCH);
        (old, ADVANCED_GC_EPOCH)
    });
    Fixture {
        _root: root,
        storage,
        _registration: registration,
        key,
        payload,
        seeded,
        epochs,
    }
}

fn ref_epoch(storage: &Path, key: &FamilyKey) -> Option<i64> {
    PlaneReader::open_existing(storage, FAMILY, key.plane())
        .unwrap()
        .database
        .connection()
        .unwrap()
        .query_row(
            "SELECT ref_epoch FROM blob_liveness JOIN blob_payloads ON blob_id=id WHERE full_key = ?1",
            [key.as_bytes().as_slice()],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

fn last_reservation(storage: &Path) -> i64 {
    LedgerReader::open_existing(storage, FAMILY)
        .unwrap()
        .database
        .connection()
        .unwrap()
        .query_row("SELECT COALESCE(MAX(id), 0) FROM reservations", [], |r| {
            r.get(0)
        })
        .unwrap()
}

fn copy(fixture: &Fixture) -> Result<CopyOutcome> {
    copy_on_first_use(
        &fixture.storage,
        CopySource::V2 {
            family: FAMILY,
            key: fixture.key,
        },
        HOLDER,
        None,
    )
}

fn replayed(fixture: &Fixture, crash_at: Option<u64>) -> replay::Report {
    replay::begin(crash_at);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy(fixture)));
    replay::finish()
}

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    /// No ledger row for the attempt.
    Absent,
    /// `reserved` or `released`: no copied row is claimed.
    Unclaimed,
    /// `committed`, and the copied row reads back with a matching digest.
    Committed,
    /// `committed` while a copied row is missing or fails its digest, or a
    /// reused row's `ref_epoch` does not match the ledger state.
    Torn,
}

fn observe(fixture: &Fixture) -> Seen {
    let seen = observe_rows(fixture);
    let Some((old, new)) = fixture.epochs else {
        return seen;
    };
    // The touch commits after the reservation and before the ledger commit:
    // with no reservation it cannot be durable, with a durable commit it must
    // be, and in between either value is whole.
    let epoch = ref_epoch(&fixture.storage, &fixture.key);
    let whole = match seen {
        Seen::Absent => epoch == Some(old),
        Seen::Unclaimed => epoch == Some(old) || epoch == Some(new),
        Seen::Committed => epoch == Some(new),
        Seen::Torn => false,
    };
    if whole {
        seen
    } else {
        eprintln!("reused row ref_epoch {epoch:?} after {seen:?} (old {old}, new {new})");
        Seen::Torn
    }
}

fn observe_rows(fixture: &Fixture) -> Seen {
    let state: Option<String> = LedgerReader::open_existing(&fixture.storage, FAMILY)
        .expect("the registry reopens after the power cut")
        .database
        .connection()
        .unwrap()
        .query_row(
            "SELECT state FROM reservations WHERE id > ?1",
            [fixture.seeded],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    match state.as_deref() {
        None => Seen::Absent,
        Some("reserved" | "released") => Seen::Unclaimed,
        Some("committed") => {
            let row = PlaneReader::open_existing(&fixture.storage, FAMILY, FamilyPlane::Semantic)
                .and_then(|plane| plane.get(&fixture.key));
            if matches!(row, Ok(Some(ref bytes)) if *bytes == fixture.payload) {
                Seen::Committed
            } else {
                Seen::Torn
            }
        }
        Some(other) => panic!("unknown reservation state {other}"),
    }
}

/// Runs `prepare` on a fresh fixture, then the replayed copy, for a run
/// with no cut and for a cut before each mutating call. Returns the uncut
/// run's report with its canonical storage root, and what each cut left.
fn enumerate(reuse: bool, prepare: impl Fn(&Fixture)) -> (replay::Report, PathBuf, Vec<Seen>) {
    let (whole, storage) = {
        let fixture = fixture(reuse);
        prepare(&fixture);
        let report = replayed(&fixture, None);
        assert!(!report.crashed);
        (report, fs::canonicalize(&fixture.storage).unwrap())
    };
    assert!(whole.ops > 0, "the closure made no file call to cut");
    let seen = (1..=whole.ops + 1)
        .map(|cut| {
            let fixture = fixture(reuse);
            prepare(&fixture);
            let report = replayed(&fixture, Some(cut));
            assert_eq!(
                report.crashed,
                cut <= whole.ops,
                "cut {cut}: this run made other file calls than the uncut run; {}",
                first_divergence(&whole.trace, &report.trace)
            );
            observe(&fixture)
        })
        .collect::<Vec<_>>();
    eprintln!(
        "replay reuse={reuse}: {} file calls, {} cuts, dir syncs {:?}, outcomes by cut {seen:?}",
        whole.ops,
        seen.len(),
        whole.dir_syncs
    );
    (whole, storage, seen)
}

/// Where a cut run's file calls first differ from the uncut run's, with a
/// few calls of context from each.
fn first_divergence(uncut: &[String], cut: &[String]) -> String {
    let at = uncut
        .iter()
        .zip(cut)
        .position(|(a, b)| a != b)
        .unwrap_or(uncut.len().min(cut.len()));
    let window = |trace: &[String]| {
        trace[at.saturating_sub(3).min(trace.len())..(at + 5).min(trace.len())].to_vec()
    };
    format!(
        "first difference at call {} (uncut {} calls, cut run {} calls before its cut): uncut {:?}, cut run {:?}",
        at + 1,
        uncut.len(),
        cut.len(),
        window(uncut),
        window(cut)
    )
}

fn assert_never_torn(seen: &[Seen]) {
    let torn: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, seen)| **seen == Seen::Torn)
        .map(|(cut, _)| cut + 1)
        .collect();
    assert!(torn.is_empty(), "torn closures at cuts {torn:?}: {seen:?}");
    assert!(seen.contains(&Seen::Committed), "{seen:?}");
    assert!(
        seen.iter()
            .any(|seen| matches!(seen, Seen::Absent | Seen::Unclaimed)),
        "{seen:?}"
    );
}

#[test]
fn replayed_copying_closure_is_whole_or_absent_after_any_power_cut() {
    let _guard = serial();
    let (_, _, seen) = enumerate(false, |_| {});
    assert_never_torn(&seen);
}

/// The reused row starts at an older `ref_epoch` than its plane's `gc_epoch`,
/// so every cut also checks the touch that keeps the row from collection:
/// the old epoch while the attempt is absent, the new one once its ledger
/// commit is durable.
#[test]
fn replayed_reuse_only_closure_is_whole_or_absent_after_any_power_cut() {
    let _guard = serial();
    let (_, _, seen) = enumerate(true, |_| {});
    assert_never_torn(&seen);
}

/// A first copy whose strict floor re-sync fails runs no closure. Its retry
/// finds the floor already covering v3, syncs the storage root again (a
/// `DirectorySync` event) and replays like any other copy.
#[test]
fn replayed_retry_after_a_failed_floor_directory_sync_is_whole_or_absent() {
    let _guard = serial();
    let fail_floor_sync = |fixture: &Fixture| {
        // The seed already synced this floor. Model a publisher without that
        // process-local proof so its first covered raise must sync again.
        crate::reader_floor::forget_durable_for_test(&fixture.storage);
        crate::durability::FAIL_DIRECTORY_SYNC
            .with(|slot| *slot.borrow_mut() = Some(fixture.storage.clone()));
        let error = copy(fixture).unwrap_err();
        crate::durability::FAIL_DIRECTORY_SYNC.with(|slot| slot.borrow_mut().take());
        assert!(matches!(error, Error::Io(_)), "{error}");
        assert_eq!(last_reservation(&fixture.storage), fixture.seeded);
        assert_eq!(
            PlaneReader::open_existing(&fixture.storage, FAMILY, FamilyPlane::Semantic)
                .unwrap()
                .get(&fixture.key)
                .unwrap(),
            None
        );
    };
    let (whole, storage, seen) = enumerate(false, fail_floor_sync);
    assert!(
        whole.dir_syncs.contains(&storage),
        "the covered retry recorded no storage-root DirectorySync: {:?}",
        whole.dir_syncs
    );
    assert_never_torn(&seen);
}

/// Test-only fault injection, which this test expects to fail the invariant:
/// with the plane commits unsynced and no closure flush before
/// `ClosureFlushed`, some power cut leaves the ledger committed while the
/// copied row is gone.
#[test]
fn red_control_without_the_closure_flush_a_power_cut_tears_the_copy() {
    let _guard = serial();
    replay::set_unflushed_closure(true);
    let result = std::panic::catch_unwind(|| enumerate(false, |_| {}));
    replay::set_unflushed_closure(false);
    let (_, _, seen) = result.unwrap();
    assert!(seen.contains(&Seen::Torn), "{seen:?}");
}

/// A plane WAL that an earlier read-only reader left behind (SQLite creates
/// it even for a reader and cannot remove it on a read-only close) must not
/// change how many calls the copy is cut at. Whether that file is still there
/// when a copy starts has differed between runs on a loaded CI runner, and
/// counting its creation then made a cut run disagree with the uncut run.
#[test]
fn replayed_copy_counts_the_same_calls_with_or_without_a_leftover_reader_wal() {
    let _guard = serial();
    let wal = |fixture: &Fixture| {
        plane_path(&fixture.storage, FAMILY, FamilyPlane::Semantic)
            .unwrap()
            .with_file_name("semantic.sqlite-wal")
    };
    let without = fixture(false);
    assert!(
        !wal(&without).exists(),
        "the fixture's writer removed its WAL"
    );
    let without = replayed(&without, None);
    let with = fixture(false);
    PlaneReader::open_existing(&with.storage, FAMILY, FamilyPlane::Semantic)
        .unwrap()
        .get(&with.key)
        .unwrap();
    assert!(
        wal(&with).exists(),
        "the read-only reader left its WAL behind"
    );
    let with = replayed(&with, None);
    assert!(!without.crashed && !with.crashed);
    assert_eq!(
        without.ops,
        with.ops,
        "{}",
        first_divergence(&without.trace, &with.trace)
    );
}
