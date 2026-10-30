//! Pages and bytes one disk-limit sweep reads from a large family store,
//! split by the step that reads them.
//!
//! The probe counts SQLite page-cache misses per statement on the store's
//! own connection, after releasing that connection's cache, so every page a
//! statement needs is a page read from the store's files. Marking reads
//! other files (view manifests, `pins/`, `readers/`), so on Linux the probe
//! also reports the process's `rchar` (bytes returned by read system calls,
//! page cache or not) between the sweep's observer steps.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::blob_store::v2::read_tests::{
    btree_pages, bulk_insert, freelist_pages, has_totals, set_epoch, ReadProbe,
};
use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane, FamilyStore};
use crate::gc::family::{
    sweep_family_remeasured, FamilySweepPolicy, FamilySweepReport, SweepBounds, SweepObserver,
    SweepStep,
};
use crate::views::manifest_v2::{
    EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2,
};
use crate::views::readiness::PlaneState;
use crate::views::registry::{FamilyRegistry, ViewRegistration};
use crate::views::RelPath;

pub(crate) const FAMILY: &str = "reads";

/// A payload the size of a stored semantic chunk vector: a format byte, a
/// producer identity, the chunk text and a vector of 384 or more floats.
/// Sizes spread between 2,000 and 4,999 bytes, about 3,500 on average.
pub(crate) fn chunk_payload(index: u64) -> Vec<u8> {
    let len = 2_000 + (index.wrapping_mul(7_919) % 3_000) as usize;
    let mut state = index.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

pub(crate) fn chunk_key(index: u64) -> FamilyKey {
    FamilyKey::new(
        FamilyPlane::Semantic,
        *blake3::hash(&index.to_le_bytes()).as_bytes(),
    )
}

pub(crate) struct Fixture {
    pub registry: FamilyRegistry,
    pub view: ViewRegistration,
    pub store: FamilyStore,
}

/// One registered view whose current generation marks every row `marked`
/// selects, over a semantic store holding `rows` chunk-sized payloads.
pub(crate) fn build(storage: &Path, rows: u64, marked: impl Fn(u64) -> bool) -> Fixture {
    let registry = FamilyRegistry::open(storage, FAMILY).unwrap();
    let root = storage.join("root");
    fs::create_dir_all(&root).unwrap();
    let view = registry
        .register_view_with_storage(storage, "a", &root)
        .unwrap();
    // Registration may start a background disk-limit pass; let it finish so
    // it cannot sweep the store while a test measures its own pass.
    crate::views::eviction::wait_for_background_enforcement(&registry);
    let store = view.open_store(FamilyPlane::Semantic).unwrap();
    bulk_insert(
        &store,
        (0..rows).map(|index| (*chunk_key(index).as_bytes(), chunk_payload(index))),
    );
    publish_marks(&view, (0..rows).filter(|index| marked(*index)));
    Fixture {
        registry,
        view,
        store,
    }
}

/// Publishes a generation whose manifest lists the chunk keys of `indexes`
/// as ready semantic keys, so marking protects exactly those rows.
pub(crate) fn publish_marks(view: &ViewRegistration, indexes: impl Iterator<Item = u64>) {
    let mut manifest = ManifestV2::new(ManifestHeader {
        producers: Producers {
            trigram: "trigram-v1".to_string(),
            semantic: Some("model-a".to_string()),
            callgraph: "callgraph-v1".to_string(),
        },
        head_tree: None,
        ignore_fingerprint: None,
        segment: None,
    });
    for index in indexes {
        manifest
            .insert(
                RelPath::new(format!("src/f{index}.rs").into_bytes()).unwrap(),
                EntryV2::regular(
                    ContentHash::of(&index.to_le_bytes()),
                    8,
                    EntryPlanes {
                        trigram: None,
                        semantic: Some(PlaneState::ready(&chunk_key(index))),
                        callgraph: None,
                    },
                ),
            )
            .unwrap();
    }
    let store_view = view.view_store().unwrap();
    let base = store_view.current_generation().unwrap();
    let name = GenerationName::for_manifest(&manifest).unwrap();
    let prepared = store_view
        .prepare_v2(&name, base.as_deref(), &manifest, None)
        .unwrap();
    assert_eq!(
        store_view.commit_v2(prepared, None).unwrap(),
        PublishV2::Published
    );
}

/// Bytes this process has received from read system calls, page cache or
/// not, or `None` where `/proc/self/io` does not exist.
fn rchar() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/io").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("rchar:"))
        .and_then(|value| value.trim().parse().ok())
}

/// Records `rchar` at each step the sweep reports.
#[derive(Default)]
struct PhaseIo {
    samples: RefCell<Vec<(SweepStep, Option<u64>)>>,
}

impl SweepObserver for PhaseIo {
    fn reached(&self, step: SweepStep) {
        self.samples.borrow_mut().push((step, rchar()));
    }
}

/// The step of a disk-limit pass a statement on the store connection
/// belongs to.
pub(crate) fn step_of(sql: &str) -> &'static str {
    let sql = sql.trim_start();
    if sql.starts_with("SELECT COUNT(*), COALESCE(SUM(length(payload))") {
        "usage (exact payload sum over every row)"
    } else if sql.starts_with("SELECT p.full_key") {
        "rows_oldest (listing)"
    } else if sql.starts_with("DROP TRIGGER")
        || sql.starts_with("DROP TABLE IF EXISTS payload_totals")
        || sql.starts_with("CREATE TABLE payload_totals")
        || sql.starts_with("INSERT INTO payload_totals")
        || sql.starts_with("CREATE TRIGGER")
    {
        "payload totals setup"
    } else if sql.contains("sqlite_schema") || sql.contains("FROM payload_totals") {
        "payload totals read"
    } else if sql.starts_with("DELETE FROM blob_payloads") {
        "deletes"
    } else if sql.starts_with("PRAGMA page_count") || sql.starts_with("PRAGMA page_size") {
        "page_count/page_size (upper bound, reclaim)"
    } else if sql.starts_with("PRAGMA incremental_vacuum")
        || sql.starts_with("PRAGMA wal_checkpoint")
        || sql.starts_with("PRAGMA freelist_count")
        || sql.starts_with("PRAGMA auto_vacuum")
        || sql.starts_with("PRAGMA busy_timeout")
    {
        "reclaim_free_pages (incremental vacuum)"
    } else if sql.contains("store_meta") {
        "raise_epoch"
    } else if sql.contains("FROM segments") {
        "segments"
    } else {
        "other"
    }
}

/// What one probed sweep read.
#[derive(Debug)]
pub(crate) struct PassReads {
    pub report: FamilySweepReport,
    /// Pages read by the store connection, by step.
    pub pages: BTreeMap<&'static str, i64>,
    /// Pages read by the store connection in total.
    pub total_pages: i64,
    /// `rchar` bytes between the sweep's steps, when available.
    pub phases: Vec<(&'static str, Option<u64>)>,
}

/// Runs one family sweep as a disk-limit pass would, with a cold SQLite
/// cache on `store`'s connection, and counts what it reads from `store`.
pub(crate) fn probed_pass(
    registry: &FamilyRegistry,
    store: &FamilyStore,
    byte_budget: u64,
) -> PassReads {
    let probe = ReadProbe::start(store);
    let observer = PhaseIo::default();
    let started = rchar();
    let report = sweep_family_remeasured(
        registry,
        None,
        FamilySweepPolicy { byte_budget },
        SweepBounds::default(),
        Some(&observer),
    )
    .unwrap();
    let finished = rchar();
    let counts = probe.finish();
    let mut pages = BTreeMap::new();
    for (sql, (_, misses)) in &counts.statements {
        *pages.entry(step_of(sql)).or_default() += i64::from(*misses);
    }
    let samples = observer.samples.into_inner();
    let at = |wanted: SweepStep| {
        samples
            .iter()
            .find(|(step, _)| *step == wanted)
            .and_then(|(_, value)| *value)
    };
    let delta = |from: Option<u64>, to: Option<u64>| from.zip(to).map(|(a, b)| b - a);
    let phases = vec![
        (
            "open stores + raise epoch",
            delta(started, at(SweepStep::EpochRaised)),
        ),
        (
            "mark_member (all members)",
            delta(at(SweepStep::EpochRaised), at(SweepStep::Marked)),
        ),
        (
            "delete_unmarked (file walk, bound, usage, listing, deletes, reclaim)",
            delta(at(SweepStep::Marked), at(SweepStep::Deleted)),
        ),
        (
            "missing-root checks",
            delta(at(SweepStep::Deleted), finished),
        ),
        ("whole sweep", delta(started, finished)),
    ];
    PassReads {
        report,
        pages,
        total_pages: i64::from(counts.misses),
        phases,
    }
}

fn file_bytes(registry: &FamilyRegistry) -> u64 {
    crate::views::eviction::family_file_bytes(registry.storage(), registry.family(), None)
        .unwrap()
        .0
}

const PAGE: i64 = 4096;

fn print_pass(scenario: &str, pass: usize, reads: &PassReads, before: u64, after: u64) {
    let report = &reads.report;
    eprintln!(
        "DISK_PASS scenario={scenario} pass={pass} family_file_bytes={before}->{after} \
         marked_keys={} examined_blobs={} deleted_blobs={} deleted_bytes={} \
         reclaimed_pages={} reclaim_deferred={} store_pages_read={} store_bytes_read={}",
        report.marked_keys,
        report.examined_blobs,
        report.deleted_blobs,
        report.deleted_bytes,
        report.space_reclaim.reclaimed_pages,
        report.reclaim_deferred,
        reads.total_pages,
        reads.total_pages * PAGE,
    );
    for (step, pages) in &reads.pages {
        eprintln!(
            "DISK_PASS scenario={scenario} pass={pass}   step=\"{step}\" pages={pages} bytes={}",
            pages * PAGE
        );
    }
    for (phase, bytes) in &reads.phases {
        let shown = bytes.map_or_else(|| "unavailable".to_string(), |b| b.to_string());
        eprintln!(
            "DISK_PASS scenario={scenario} pass={pass}   rchar phase=\"{phase}\" bytes={shown}"
        );
    }
}

/// Measurement receipt: a semantic store of chunk-sized payloads, about
/// 1.96 GB of payload in a file of about 2.7 GB (`AFT_DISK_PASS_ROWS`
/// overrides the default 560,000 rows), nine rows in ten marked.
///
/// The receipt first measures the payload totals setup on its own (what
/// the first disk-limit pass over such a store does). Scenario `budget`:
/// the default 2 GiB payload budget, which the exact payload fits but the
/// page-count bound does not, so nothing is deleted. Scenario
/// `collect-all`: a budget of zero, as after an eviction or with the family
/// over its soft limit; the first pass deletes the unmarked tenth, the next
/// ones find every row marked but still list every row.
///
/// `cargo test -p agent-file-tools --lib -- --ignored --exact --nocapture
/// gc::family_read_tests::disk_limit_pass_reads_receipt` (an optimized test
/// build: `CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_TEST_OPT_LEVEL=3`).
#[test]
#[ignore = "builds a store of several gigabytes; run alone with --ignored --exact --nocapture"]
fn disk_limit_pass_reads_receipt() {
    let rows = std::env::var("AFT_DISK_PASS_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(560_000_u64);
    let storage = tempfile::tempdir().unwrap();
    let built = std::time::Instant::now();
    let fixture = build(storage.path(), rows, |index| index % 10 != 0);
    let payload = written_payload(rows);
    crate::blob_store::v2::read_tests::describe(&fixture.store);
    eprintln!(
        "DISK_PASS fixture rows={rows} payload_bytes={payload} family_file_bytes={} \
         page_bound={} built_in_s={:.1}",
        file_bytes(&fixture.registry),
        fixture.store.payload_bytes_upper_bound().unwrap(),
        built.elapsed().as_secs_f64()
    );

    let before = file_bytes(&fixture.registry);
    let probe = ReadProbe::start(&fixture.store);
    let rchar_before = rchar();
    let started = std::time::Instant::now();
    let outcome = fixture
        .store
        .ensure_payload_totals(started + std::time::Duration::from_secs(600))
        .unwrap();
    let elapsed = started.elapsed();
    let rchar_after = rchar();
    let counts = probe.finish();
    eprintln!(
        "DISK_PASS payload-totals setup outcome={outcome:?} write_lock_held_ms={} \
         store_pages_read={} store_bytes_read={} rchar_bytes={:?} table_pages={} \
         family_file_bytes={before}->{}",
        elapsed.as_millis(),
        counts.misses,
        i64::from(counts.misses) * PAGE,
        rchar_before.zip(rchar_after).map(|(a, b)| b - a),
        btree_pages(&fixture.store, "blob_payloads"),
        file_bytes(&fixture.registry),
    );
    assert!(has_totals(&fixture.store));

    for (scenario, budget) in [
        (
            "budget",
            crate::views::eviction::FAMILY_PAYLOAD_BUDGET_BYTES,
        ),
        ("collect-all", 0),
    ] {
        for pass in 1..=3 {
            let before = file_bytes(&fixture.registry);
            sqlite_memory_highwater(true);
            let started = std::time::Instant::now();
            let reads = probed_pass(&fixture.registry, &fixture.store, budget);
            let elapsed = started.elapsed();
            print_pass(
                scenario,
                pass,
                &reads,
                before,
                file_bytes(&fixture.registry),
            );
            eprintln!(
                "DISK_PASS scenario={scenario} pass={pass}   elapsed_ms={} freelist_pages={} \
                 sqlite_memory_highwater_bytes={} process_peak_rss={:?}",
                elapsed.as_millis(),
                freelist_pages(&fixture.store),
                sqlite_memory_highwater(false),
                fs::read_to_string("/proc/self/status")
                    .ok()
                    .and_then(|status| status
                        .lines()
                        .find(|line| line.starts_with("VmHWM:"))
                        .map(str::to_owned))
            );
            if scenario == "budget" {
                assert!(
                    reads.total_pages <= STEADY_PASS_PAGES,
                    "a pass over the measured store read {} pages: {:?}",
                    reads.total_pages,
                    reads.pages
                );
            }
        }
    }
    drop(fixture.view);
}

/// The most memory SQLite has held at once in this process since the last
/// reset, in bytes; `reset` starts a new measurement.
fn sqlite_memory_highwater(reset: bool) -> i64 {
    let (mut current, mut highwater) = (0_i64, 0_i64);
    // A process-wide counter, read through SQLite's own status interface.
    let result = unsafe {
        rusqlite::ffi::sqlite3_status64(
            rusqlite::ffi::SQLITE_STATUS_MEMORY_USED,
            &mut current,
            &mut highwater,
            i32::from(reset),
        )
    };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
    highwater
}

/// Bytes this process has passed to write system calls, or `None` where
/// `/proc/self/io` does not exist.
fn wchar() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/io").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("wchar:"))
        .and_then(|value| value.trim().parse().ok())
}

/// Write-path cost receipt: the time and bytes written for bulk inserts
/// (10,000 rows per transaction), per-key puts, per-key touches at a new
/// epoch and per-key GC deletes (one transaction each, as the code runs
/// them). Three setups: as before this change (no totals, temporary files on
/// disk), no totals with temporary storage in memory (now the default), and
/// totals triggers with temporary storage in memory (the shipped setup).
/// Three alternating rounds; compare medians. `AFT_TOTALS_COST_ROWS`
/// overrides the 50,000 bulk rows.
#[test]
#[ignore = "timing receipt; run alone with --ignored --exact --nocapture"]
fn payload_totals_write_cost_receipt() {
    use crate::blob_store::v2::StoreWriteAccess;
    let rows = std::env::var("AFT_TOTALS_COST_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50_000_u64);
    let per_key = 10_000_u64;
    for round in 0..3 {
        for (setup, with_totals, pragma) in [
            ("before", false, "PRAGMA temp_store = DEFAULT"),
            ("no-totals", false, ""),
            ("totals", true, ""),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = FamilyStore::open(
                &StoreWriteAccess::for_registered_view(dir.path(), "cost"),
                FamilyPlane::Semantic,
            )
            .unwrap();
            if !pragma.is_empty() {
                crate::blob_store::v2::read_tests::set_pragma(&store, pragma);
            }
            if with_totals {
                store
                    .ensure_payload_totals(
                        std::time::Instant::now() + std::time::Duration::from_secs(60),
                    )
                    .unwrap();
                assert!(has_totals(&store));
            }
            let measure = |work: &dyn Fn()| {
                let written = wchar();
                let started = std::time::Instant::now();
                work();
                (
                    started.elapsed().as_secs_f64() * 1000.0,
                    written.zip(wchar()).map(|(a, b)| b - a),
                )
            };
            let bulk = measure(&|| {
                bulk_insert(
                    &store,
                    (0..rows).map(|index| (*chunk_key(index).as_bytes(), chunk_payload(index))),
                )
            });
            let puts = measure(&|| {
                for index in rows..rows + per_key {
                    store
                        .put_or_touch(&chunk_key(index), &chunk_payload(index))
                        .unwrap();
                }
            });
            let epoch = store.raise_epoch(1).unwrap();
            let touches = measure(&|| {
                for index in rows..rows + per_key {
                    store.touch(&[chunk_key(index)]).unwrap();
                }
            });
            let epoch = store.raise_epoch(epoch + 1).unwrap();
            let deletes = measure(&|| {
                for index in 0..per_key {
                    assert!(store
                        .delete_if_unreferenced_since(&chunk_key(index), epoch)
                        .unwrap());
                }
            });
            if with_totals {
                assert!(has_totals(&store));
                let usage = store.usage().unwrap();
                assert_eq!(usage.rows, rows);
            }
            eprintln!(
                "TOTALS_COST round={round} setup={setup} \
                 bulk_insert_rows={rows} bulk_ms={:.0} bulk_wchar={:?} \
                 put_rows={per_key} put_ms={:.0} put_wchar={:?} \
                 touch_rows={per_key} touch_ms={:.0} touch_wchar={:?} \
                 delete_rows={per_key} delete_ms={:.0} delete_wchar={:?}",
                bulk.0, bulk.1, puts.0, puts.1, touches.0, touches.1, deletes.0, deletes.1
            );
        }
    }
}

/// The exact payload of a fixture of `rows`, from the payloads it wrote.
fn written_payload(rows: u64) -> u64 {
    (0..rows)
        .map(|index| chunk_payload(index).len() as u64)
        .sum()
}

/// Store pages a pass over a store with payload totals may read: page 1
/// for the page counts, the schema for the totals check, the totals row,
/// the metadata row for the epoch, and the pages the free-page check
/// touches. A sum or a listing over the fixture below reads hundreds.
const STEADY_PASS_PAGES: i64 = 16;

/// A disk-limit pass over a store whose payload totals are set up reads a
/// bounded number of the store's pages, starting from a connection opened
/// for the pass, with nothing about the store remembered in memory. The
/// payload fits the budget, which the page-count bound does not, so before
/// the totals every such pass summed every row and then listed every row.
#[test]
fn a_pass_over_a_store_with_payload_totals_reads_a_bounded_number_of_pages() {
    let storage = tempfile::tempdir().unwrap();
    let rows = 3_000;
    let Fixture {
        registry,
        view,
        store,
    } = build(storage.path(), rows, |index| index % 10 != 0);
    let budget = written_payload(rows);
    assert!(store.payload_bytes_upper_bound().unwrap() > budget);

    let first = probed_pass(&registry, &store, budget);
    assert_eq!(first.report.deleted_blobs, 0);
    assert!(
        has_totals(&store),
        "the disk-limit pass sets up the totals: {first:?}"
    );
    let table_pages = btree_pages(&store, "blob_payloads");
    assert!(table_pages > 20 * STEADY_PASS_PAGES, "{table_pages}");
    let path = store.path().to_path_buf();
    drop(store);

    for _ in 0..2 {
        // A new connection, as each disk-limit pass opens its stores anew.
        let store = view.open_store(FamilyPlane::Semantic).unwrap();
        assert_eq!(store.path(), path);
        let next = probed_pass(&registry, &store, budget);
        assert_eq!(next.report.deleted_blobs, 0);
        assert_eq!(next.report.examined_blobs, 0);
        assert!(
            next.total_pages <= STEADY_PASS_PAGES,
            "a pass read {} of the store's {table_pages}+ pages: {:?}",
            next.total_pages,
            next.pages
        );
    }
}

/// The set of keys a sweep deletes, for the same store state, matches the
/// rule the sweep has always applied: list every row oldest epoch first (cut
/// at the row bound), skip marked rows, and delete each unmarked row whose
/// epoch is older than the sweep's while the exact payload total is over the
/// budget. The rule is applied here to a listing with sizes taken before
/// each sweep, so it shares neither the sweep's totals table nor its
/// skipped listing. Each case runs three passes, so later passes compare
/// against a store the earlier ones changed.
#[test]
fn deleted_keys_match_the_listing_rule_for_the_same_store_state() {
    struct Case {
        name: &'static str,
        marked: fn(u64) -> bool,
        budget: fn(u64) -> u64,
        max_rows: usize,
    }
    let cases = [
        Case {
            name: "part of the unmarked rows over a payload budget",
            marked: |index| index % 3 != 0,
            budget: |payload| payload * 7 / 10,
            max_rows: 1_000_000,
        },
        Case {
            name: "every unmarked row",
            marked: |index| index % 2 == 0,
            budget: |_| 0,
            max_rows: 1_000_000,
        },
        Case {
            name: "every unmarked row within a row bound",
            marked: |index| index % 4 != 1,
            budget: |_| 0,
            max_rows: 100,
        },
        Case {
            name: "a budget the payload fits",
            marked: |_| false,
            budget: |payload| payload,
            max_rows: 1_000_000,
        },
    ];
    let rows = 600_u64;
    for case in cases {
        let storage = tempfile::tempdir().unwrap();
        let fixture = build(storage.path(), rows, case.marked);
        // Spread epochs so the oldest-first order matters, and give some
        // unmarked rows an epoch no sweep here reaches, which the
        // conditional delete must keep.
        for index in 0..rows {
            let epoch = if index % 7 == 3 { 1_000_000 } else { index % 5 };
            set_epoch(&fixture.store, &chunk_key(index), epoch);
        }
        let budget = (case.budget)(written_payload(rows));
        for pass in 1..=3 {
            let listing = fixture.store.rows().unwrap();
            let bound = fixture.store.payload_bytes_upper_bound().unwrap();
            let marked = (0..rows)
                .filter(|index| (case.marked)(*index))
                .map(chunk_key)
                .collect::<std::collections::BTreeSet<_>>();
            let report = sweep_family_remeasured(
                &fixture.registry,
                None,
                FamilySweepPolicy {
                    byte_budget: budget,
                },
                SweepBounds {
                    max_rows_per_store: case.max_rows,
                    ..SweepBounds::default()
                },
                None,
            )
            .unwrap();

            let mut expected = Vec::new();
            let mut expected_bytes = 0;
            // The fixture's files and page bound are larger than every
            // budget here, so only the exact total decides.
            assert!(bound > budget && file_bytes(&fixture.registry) > budget);
            let mut total = listing.iter().map(|row| row.payload_bytes).sum::<u64>();
            for row in listing.iter().take(case.max_rows) {
                if total <= budget {
                    break;
                }
                if marked.contains(&row.key) || row.ref_epoch >= report.epoch {
                    continue;
                }
                total -= row.payload_bytes;
                expected_bytes += row.payload_bytes;
                expected.push(row.key);
            }
            let mut deleted = listing
                .iter()
                .map(|row| row.key)
                .filter(|key| !fixture.store.contains(key).unwrap())
                .collect::<Vec<_>>();
            expected.sort();
            deleted.sort();
            assert_eq!(
                deleted, expected,
                "{} pass {pass}: deleted keys differ",
                case.name
            );
            assert_eq!(
                (report.deleted_blobs, report.deleted_bytes),
                (expected.len(), expected_bytes),
                "{} pass {pass}",
                case.name
            );
            if pass == 1 && case.name != "a budget the payload fits" {
                assert!(!expected.is_empty(), "{} must delete something", case.name);
            }
        }
    }
}

/// Bytes of the store's database pages, free or not, as SQLite counts them
/// on the store's own connection. It is what the main file shrinks to once
/// the write-ahead log is checkpointed.
fn database_bytes(store: &FamilyStore) -> u64 {
    store.payload_bytes_upper_bound().unwrap()
}

/// The length of the store's main database file on disk, from a stat of
/// its path; nothing is opened.
fn main_file_bytes(store: &FamilyStore) -> u64 {
    fs::metadata(store.path()).unwrap().len()
}

/// A family whose store is over its soft limit only because of free pages.
/// A third of the rows are unmarked; a sweep under a payload budget
/// deletes about half of those, which leaves their pages free inside the
/// file, far below the share and size at which every sweep returns free
/// pages, and the soft limit then sits between the family's live bytes
/// (file bytes minus the store's bytes that hold no data, here its free
/// pages) and its file bytes. Returns the fixture and the
/// rows still stored.
fn free_pages_only_family(storage: &Path, rows: u64) -> (Fixture, Vec<FamilyKey>) {
    let fixture = build(storage, rows, |index| index % 3 != 0);
    let unmarked = (0..rows)
        .filter(|index| index % 3 == 0)
        .map(|index| chunk_payload(index).len() as u64)
        .sum::<u64>();
    let freed = sweep_family_remeasured(
        &fixture.registry,
        None,
        FamilySweepPolicy {
            byte_budget: written_payload(rows) - unmarked / 2,
        },
        SweepBounds::default(),
        None,
    )
    .unwrap();
    assert!(freed.deleted_blobs > 0, "{freed:?}");
    assert_eq!(
        freed.space_reclaim.reclaimed_pages, 0,
        "the free pages must stay below the usual reclaim thresholds"
    );
    let stored = (0..rows)
        .map(chunk_key)
        .filter(|key| fixture.store.contains(key).unwrap())
        .collect::<Vec<_>>();
    let file = file_bytes(&fixture.registry);
    let free = freelist_pages(&fixture.store) * PAGE as u64;
    assert!(free > 64 * PAGE as u64, "{free}");
    fixture
        .registry
        .set_disk_budget(crate::views::eviction::DiskBudget {
            family_store_soft: file - free / 2,
            family_store_hard: u64::MAX,
            ..crate::views::eviction::DiskBudget::default()
        });
    (fixture, stored)
}

/// A family store over its soft limit only because of free pages holds no
/// row that has to go to meet that limit. One enforcement pass lists and
/// deletes no row and returns free pages to the filesystem. Before live
/// bytes decided the limit, every such pass collected every unmarked row,
/// listing every row to do so, and the free pages stayed in the file.
#[test]
fn a_store_over_its_soft_limit_only_by_free_pages_deletes_no_rows_and_shrinks() {
    let storage = tempfile::tempdir().unwrap();
    let (fixture, stored) = free_pages_only_family(storage.path(), 3_000);
    let database_before = database_bytes(&fixture.store);
    let main_file_before = main_file_bytes(&fixture.store);
    let free_before = freelist_pages(&fixture.store);

    let report = crate::views::eviction::enforce(
        &fixture.registry,
        crate::views::eviction::EnforceOptions::default(),
    )
    .unwrap();

    assert!(report.evicted.is_empty(), "{report:?}");
    let sweep = report.sweep.as_ref().expect("the family sweep ran");
    assert_eq!(
        (sweep.deleted_blobs, sweep.examined_blobs),
        (0, 0),
        "a pass over a store whose live bytes fit the soft limit deleted or listed rows: \
         {report:?}"
    );
    let missing = stored
        .iter()
        .filter(|key| !fixture.store.contains(key).unwrap())
        .count();
    assert_eq!(missing, 0, "rows deleted");
    let database_after = database_bytes(&fixture.store);
    let free_after = freelist_pages(&fixture.store);
    assert!(
        database_after < database_before && free_after < free_before,
        "database {database_before} -> {database_after} bytes, free pages {free_before} -> \
         {free_after}"
    );
    // The pages returned are free pages, at most one pass's cap of them;
    // pointer-map pages, one per few hundred pages, may go with them.
    let returned = &report.free_page_reclaim;
    let cap = crate::blob_store::reclaim::ALL_FREE_PAGES_MAX_PAGES;
    assert!(
        returned.reclaimed_pages > 0 && returned.reclaimed_pages <= cap,
        "{returned:?}"
    );
    assert_eq!(returned.reclaimed_pages, free_before - free_after);
    let shrunk_pages = (database_before - database_after) / PAGE as u64;
    assert!(
        shrunk_pages >= returned.reclaimed_pages
            && shrunk_pages <= returned.reclaimed_pages + returned.reclaimed_pages / 200 + 2,
        "{shrunk_pages} pages shrunk for {returned:?}"
    );
    // The space is back on disk within the pass: the main file, which the
    // store's connection keeps open, has been cut to the database's size,
    // not left at its old length until some later checkpoint.
    let main_file_after = main_file_bytes(&fixture.store);
    assert!(
        main_file_after == database_after && main_file_after < main_file_before,
        "main file {main_file_before} -> {main_file_after} bytes, database {database_after}"
    );
}

/// A family of `rows` marked rows whose store also holds about
/// `free_pages` free pages at the end of its main file, copied into that
/// file by a checkpoint, with the soft limit between the family's live
/// bytes and its file bytes.
fn tail_free_pages_family(storage: &Path, rows: u64, free_pages: u64) -> Fixture {
    let fixture = build(storage, rows, |_| true);
    crate::blob_store::v2::read_tests::set_pragma(
        &fixture.store,
        &format!(
            "CREATE TABLE free_fixture (data BLOB);
             INSERT INTO free_fixture VALUES (zeroblob({}));
             DROP TABLE free_fixture;
             PRAGMA wal_checkpoint(TRUNCATE);",
            free_pages * PAGE as u64
        ),
    );
    let free = freelist_pages(&fixture.store);
    assert!(free >= free_pages, "{free}");
    assert_eq!(
        main_file_bytes(&fixture.store),
        database_bytes(&fixture.store),
        "the fixture's free pages are in the main file"
    );
    let file = file_bytes(&fixture.registry);
    fixture
        .registry
        .set_disk_budget(crate::views::eviction::DiskBudget {
            family_store_soft: file - free * PAGE as u64 / 2,
            family_store_hard: u64::MAX,
            ..crate::views::eviction::DiskBudget::default()
        });
    fixture
}

/// A reclaim of 64 pages writes far fewer write-ahead-log frames than the
/// 1,000 at which SQLite checkpoints on its own, and the store is idle
/// after it. The main file is still cut within the same pass.
#[test]
fn a_small_reclaim_cuts_the_main_file_within_the_same_pass() {
    let storage = tempfile::tempdir().unwrap();
    let fixture = tail_free_pages_family(storage.path(), 300, 64);
    let main_file_before = main_file_bytes(&fixture.store);

    let report = crate::views::eviction::enforce(
        &fixture.registry,
        crate::views::eviction::EnforceOptions::default(),
    )
    .unwrap();

    let sweep = report.sweep.as_ref().expect("the family sweep ran");
    assert_eq!(sweep.deleted_blobs, 0, "{report:?}");
    assert!(!report.free_page_reclaim.checkpoint_blocked, "{report:?}");
    assert_eq!(freelist_pages(&fixture.store), 0, "{report:?}");
    let main_file_after = main_file_bytes(&fixture.store);
    assert!(
        main_file_after == database_bytes(&fixture.store)
            && main_file_before - main_file_after >= 64 * PAGE as u64,
        "main file {main_file_before} -> {main_file_after} bytes, database {}",
        database_bytes(&fixture.store)
    );
}

/// Another connection holding a read snapshot from before the reclaim
/// keeps the main file at its old length, since cutting it would remove
/// pages that snapshot may read. The pass does not wait for that reader:
/// it ends well within the store's 5 s busy timeout. Once the reader ends,
/// the next pass cuts the file. The reader stands for another process; it
/// is a test-only connection.
#[test]
fn a_reader_snapshot_defers_the_cut_to_the_next_pass_without_waiting() {
    let storage = tempfile::tempdir().unwrap();
    let fixture = tail_free_pages_family(storage.path(), 300, 64);
    let main_file_before = main_file_bytes(&fixture.store);
    let reader = rusqlite::Connection::open_with_flags(
        fixture.store.path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM store_meta", [], |row| row.get(0))
        .unwrap();

    let started = std::time::Instant::now();
    let first = crate::views::eviction::enforce(
        &fixture.registry,
        crate::views::eviction::EnforceOptions::default(),
    )
    .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(crate::blob_store::BUSY_TIMEOUT_MS),
        "the pass waited {elapsed:?} for the reader"
    );
    assert!(first.free_page_reclaim.reclaimed_pages >= 64, "{first:?}");
    assert_eq!(
        main_file_bytes(&fixture.store),
        main_file_before,
        "the main file was cut under a reader's snapshot"
    );
    assert!(
        first.free_page_reclaim.checkpoint_blocked,
        "the reader's snapshot blocked the checkpoint: {first:?}"
    );

    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    crate::views::eviction::enforce(
        &fixture.registry,
        crate::views::eviction::EnforceOptions::default(),
    )
    .unwrap();
    let main_file_after = main_file_bytes(&fixture.store);
    assert!(
        main_file_after == database_bytes(&fixture.store)
            && main_file_before - main_file_after >= 64 * PAGE as u64,
        "main file {main_file_before} -> {main_file_after} bytes, database {}",
        database_bytes(&fixture.store)
    );
}

/// Returning free pages regardless of the usual thresholds reads a number
/// of the store's pages bounded by the pages it returns, not by the store:
/// a pass capped at 64 pages over a store of thousands of pages, most of
/// them in use and the free ones scattered between them, reads at most a
/// few pages per page it moves, and stops at the cap with the rest left
/// for later.
#[test]
fn returning_free_pages_reads_a_bounded_number_of_pages() {
    let storage = tempfile::tempdir().unwrap();
    let (fixture, _) = free_pages_only_family(storage.path(), 3_000);
    let pages = database_bytes(&fixture.store) / PAGE as u64;
    let free_before = freelist_pages(&fixture.store);
    let cap = 64;
    assert!(
        free_before > cap && pages > 20 * cap,
        "{free_before} of {pages}"
    );

    let probe = ReadProbe::start(&fixture.store);
    // The pass's one-second budget is wall time: on a slow runner it can end
    // the pass after a few steps, which says nothing about the cap. With the
    // reclaim clock frozen only the cap can stop it.
    let clock = crate::blob_store::reclaim::freeze_clock();
    let reclaimed = fixture.store.reclaim_all_free_pages(None, cap).unwrap();
    drop(clock);
    let counts = probe.finish();

    assert_eq!(reclaimed.reclaimed_pages, cap, "{reclaimed:?}");
    assert!(reclaimed.deferred, "free pages remain: {reclaimed:?}");
    assert_eq!(freelist_pages(&fixture.store), free_before - cap);
    // Per moved page: the page itself, its pointer-map entry and the page
    // that points to it; plus the header and free-list trunk pages.
    let bound = 3 * cap as i32 + 16;
    eprintln!(
        "returning {cap} free pages read {} of the store's {pages} pages (bound {bound})",
        counts.misses
    );
    assert!(
        counts.misses <= bound,
        "returning {cap} free pages read {} of the store's {pages} pages: {:?}",
        counts.misses,
        counts.statements
    );
}

/// Measurement receipt for a family store over its soft limit only because
/// of free pages, on the fixture of [`disk_limit_pass_reads_receipt`]
/// (`AFT_DISK_PASS_ROWS` overrides its 560,000 rows; nine in ten marked):
/// a sweep under a payload budget deletes about half the unmarked rows,
/// which leaves their pages free, and the soft limit is set halfway through
/// those free pages. Three enforcement passes follow; each prints the store
/// pages it read, the rows it listed and deleted, and after it the
/// database's size by its page count, the main and write-ahead-log files'
/// lengths on disk (from a stat of each path) and the family's file bytes.
///
/// `cargo test -p agent-file-tools --lib -- --ignored --exact --nocapture
/// gc::family_read_tests::free_pages_only_pass_receipt` (an optimized test
/// build: `CARGO_PROFILE_DEV_OPT_LEVEL=3 CARGO_PROFILE_TEST_OPT_LEVEL=3`).
#[test]
#[ignore = "builds a store of several gigabytes; run alone with --ignored --exact --nocapture"]
fn free_pages_only_pass_receipt() {
    let rows = std::env::var("AFT_DISK_PASS_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(560_000_u64);
    let storage = tempfile::tempdir().unwrap();
    let built = std::time::Instant::now();
    let fixture = build(storage.path(), rows, |index| index % 10 != 0);
    // Set up the payload totals first, as the first disk-limit pass over
    // such a store does, so the passes below measure the steady state.
    fixture
        .store
        .ensure_payload_totals(std::time::Instant::now() + std::time::Duration::from_secs(600))
        .unwrap();
    let unmarked = (0..rows)
        .filter(|index| index % 10 == 0)
        .map(|index| chunk_payload(index).len() as u64)
        .sum::<u64>();
    let freed = sweep_family_remeasured(
        &fixture.registry,
        None,
        FamilySweepPolicy {
            byte_budget: written_payload(rows) - unmarked / 2,
        },
        SweepBounds::default(),
        None,
    )
    .unwrap();
    let file = file_bytes(&fixture.registry);
    let free = freelist_pages(&fixture.store) * PAGE as u64;
    let soft = file - free / 2;
    fixture
        .registry
        .set_disk_budget(crate::views::eviction::DiskBudget {
            family_store_soft: soft,
            ..crate::views::eviction::DiskBudget::default()
        });
    eprintln!(
        "FREE_PASS fixture rows={rows} built_in_s={:.1} setup_deleted_blobs={} \
         family_file_bytes={file} database_bytes={} main_file_bytes={} free_page_bytes={free} \
         live_bytes={} soft_limit={soft}",
        built.elapsed().as_secs_f64(),
        freed.deleted_blobs,
        database_bytes(&fixture.store),
        main_file_bytes(&fixture.store),
        file - free,
    );
    for pass in 1..=3 {
        let probe = ReadProbe::start(&fixture.store);
        let started = std::time::Instant::now();
        let report = crate::views::eviction::enforce(
            &fixture.registry,
            crate::views::eviction::EnforceOptions::default(),
        )
        .unwrap();
        let elapsed = started.elapsed();
        let counts = probe.finish();
        let mut pages = BTreeMap::<&str, i64>::new();
        for (sql, (_, misses)) in &counts.statements {
            *pages.entry(step_of(sql)).or_default() += i64::from(*misses);
        }
        let sweep = report.sweep.clone().unwrap_or_default();
        eprintln!(
            "FREE_PASS pass={pass} elapsed_ms={} store_pages_read={} store_bytes_read={} \
             examined_blobs={} deleted_blobs={} deleted_bytes={} sweep_error={:?} \
             family_file_bytes={} database_bytes={} main_file_bytes={} wal_file_bytes={} \
             freelist_pages={}",
            elapsed.as_millis(),
            counts.misses,
            i64::from(counts.misses) * PAGE,
            sweep.examined_blobs,
            sweep.deleted_blobs,
            sweep.deleted_bytes,
            report.sweep_error,
            file_bytes(&fixture.registry),
            database_bytes(&fixture.store),
            main_file_bytes(&fixture.store),
            fs::metadata(format!("{}-wal", fixture.store.path().display()))
                .map_or(0, |metadata| metadata.len()),
            freelist_pages(&fixture.store),
        );
        for (step, pages) in &pages {
            eprintln!(
                "FREE_PASS pass={pass}   step=\"{step}\" pages={pages} bytes={}",
                pages * PAGE
            );
        }
    }
    drop(fixture.view);
}

#[test]
fn under_budget_sweep_keeps_epoch_and_touches_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = build(dir.path(), 8, |_| true);
    let before = fixture.store.gc_epoch().unwrap();
    let probe = ReadProbe::start(&fixture.store);
    let report = sweep_family_remeasured(
        &fixture.registry,
        None,
        FamilySweepPolicy {
            byte_budget: u64::MAX,
        },
        SweepBounds::default(),
        None,
    )
    .unwrap();
    let reads = probe.finish();
    assert_eq!(
        fixture.store.gc_epoch().unwrap(),
        before,
        "a non-collecting sweep must not stale live rows"
    );
    assert_eq!(report.epoch, before);
    assert_eq!(report.marked_keys, 0);
    assert_eq!(report.deleted_blobs, 0);
    assert!(reads
        .statements
        .keys()
        .all(|sql| !sql.contains("UPDATE blob_payloads")));
}

#[test]
fn scheduled_sweeps_coalesce_but_force_collects_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = build(dir.path(), 8, |_| true);
    let first = super::family::sweep_family_scheduled(
        &fixture.registry,
        SweepBounds {
            max_rows_per_store: 1,
            ..Default::default()
        },
        false,
    )
    .unwrap();
    assert_eq!(first.skipped, Some("nothing to collect"));
    let epoch = fixture.store.gc_epoch().unwrap();
    let garbage = chunk_key(99);
    fixture.store.put_or_touch(&garbage, b"garbage").unwrap();
    let second =
        super::family::sweep_family_scheduled(&fixture.registry, SweepBounds::default(), false)
            .unwrap();
    assert_eq!(second.skipped, Some("interval"));
    assert_eq!(fixture.store.gc_epoch().unwrap(), epoch);
    let forced =
        super::family::sweep_family_scheduled(&fixture.registry, SweepBounds::default(), true)
            .unwrap();
    assert_eq!(forced.skipped, None);
    assert_eq!(forced.marked_keys, 8);
    assert_eq!(forced.deleted_blobs, 1);
    assert!(!fixture.store.contains(&garbage).unwrap());
    for index in 0..8 {
        assert!(fixture.store.contains(&chunk_key(index)).unwrap());
    }
}

#[test]
fn deleting_pass_marks_every_live_key_before_deletion() {
    struct Check<'a> {
        store: &'a FamilyStore,
    }
    impl SweepObserver for Check<'_> {
        fn reached(&self, step: SweepStep) {
            if step == SweepStep::Marked {
                for index in 0..9 {
                    assert!(self.store.contains(&chunk_key(index)).unwrap());
                }
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let fixture = build(dir.path(), 9, |index| index != 8);
    let report = sweep_family_remeasured(
        &fixture.registry,
        None,
        FamilySweepPolicy { byte_budget: 0 },
        SweepBounds::default(),
        Some(&Check {
            store: &fixture.store,
        }),
    )
    .unwrap();
    assert_eq!(report.marked_keys, 8);
    assert_eq!(report.deleted_blobs, 1);
    for index in 0..8 {
        assert!(fixture.store.contains(&chunk_key(index)).unwrap());
    }
    assert!(!fixture.store.contains(&chunk_key(8)).unwrap());
}

#[test]
fn all_live_preflight_reads_each_manifest_once_and_reclaims_dead_owners() {
    use crate::views::manifest_v2::ManifestReadProbe;
    let dir = tempfile::tempdir().unwrap();
    let fixture = build(dir.path(), 4, |_| true);
    let view_store = fixture.view.view_store().unwrap();
    let protected_generation = view_store.current_generation().unwrap().unwrap();
    // Reader protection must exist before replacement can make it obsolete.
    let live_reader =
        crate::root_cache::ReadMarker::create(fixture.view.view_dir(), &protected_generation)
            .unwrap();
    publish_marks(&fixture.view, 0..3);
    let current_generation = view_store.current_generation().unwrap().unwrap();
    assert_ne!(current_generation, protected_generation);
    crate::views::wait_for_generation_maintenance_for_test(fixture.view.view_dir());
    let live_pin =
        crate::pins::AssemblyPin::create_v2(&fixture.view, "live-assembly", &[chunk_key(0)])
            .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    let (dead_pin, dead_keys) = crate::pins::pin_paths(fixture.view.view_dir(), "dead-assembly");
    let metadata = crate::pins::PinMetadata {
        family: FAMILY.to_owned(),
        view: fixture.view.scope().to_owned(),
        generation: "dead-assembly".to_owned(),
        owner: crate::pins::PinOwner {
            pid: dead_pid,
            start_time: 0,
        },
        created_at: 0,
        renewed_at: 0,
    };
    fs::write(&dead_pin, serde_json::to_vec(&metadata).unwrap()).unwrap();
    fs::write(&dead_keys, b"").unwrap();
    let dead_marker =
        crate::root_cache::ReadMarker::create(fixture.view.view_dir(), "dead-reader").unwrap();
    let dead_marker_path = dead_marker.path().to_path_buf();
    let mut dead_metadata = dead_marker.metadata().clone();
    drop(dead_marker);
    dead_metadata.pid = dead_pid;
    fs::write(
        &dead_marker_path,
        serde_json::to_vec(&dead_metadata).unwrap(),
    )
    .unwrap();
    let before = fixture.store.gc_epoch().unwrap();
    let probe = ManifestReadProbe::start();
    let report = sweep_family_remeasured(
        &fixture.registry,
        None,
        FamilySweepPolicy { byte_budget: 0 },
        SweepBounds::default(),
        None,
    )
    .unwrap();
    let reads = probe.finish();
    assert_eq!(report.skipped, Some("nothing to collect"));
    assert_eq!(fixture.store.gc_epoch().unwrap(), before);
    assert_eq!(report.reclaimed_pins, 1);
    assert!(!dead_pin.exists() && !dead_keys.exists() && !dead_marker_path.exists());
    assert!(live_reader.path().exists());
    assert!(
        crate::pins::pin_paths(fixture.view.view_dir(), "live-assembly")
            .0
            .exists()
    );
    assert_eq!(
        reads.len(),
        2,
        "current and distinct reader-protected generations must be inspected"
    );
    for generation in [&current_generation, &protected_generation] {
        let path = view_store.manifest_path(generation).unwrap();
        assert_eq!(
            reads.get(&path),
            Some(&(1, fs::metadata(&path).unwrap().len())),
            "cleanup must not reload {generation}"
        );
    }
    drop(live_pin);
}
