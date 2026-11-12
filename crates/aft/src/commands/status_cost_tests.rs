//! Filled-store probe for the producer-triggered status path.

use super::*;
use crate::data_layer::runtime::{DataCheckout, Fresh};
use crate::data_layer::vectors::{FillBudget, Producer, VectorPlane};
use crate::run_tool_call::ThreadCpuSample;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

thread_local! {
    static COSTS: RefCell<Option<BTreeMap<&'static str, Duration>>> = const { RefCell::new(None) };
    static SQL: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn trace_sql(sql: &str) {
    SQL.with(|statements| statements.borrow_mut().push(sql.to_string()));
}

fn db_page_reads(ctx: &AppContext) -> (i32, i32) {
    let db = ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let read = |operation| {
        let (mut current, mut high) = (0, 0);
        // The connection mutex keeps the SQLite handle live and exclusively held.
        let code = unsafe {
            rusqlite::ffi::sqlite3_db_status(conn.handle(), operation, &mut current, &mut high, 1)
        };
        assert_eq!(code, rusqlite::ffi::SQLITE_OK);
        current
    };
    (
        read(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT),
        read(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS),
    )
}

pub(super) struct PushCost(Option<ThreadCpuSample>);

impl PushCost {
    pub(super) fn start() -> Self {
        Self(COSTS.with(|costs| costs.borrow().as_ref().and_then(|_| ThreadCpuSample::now())))
    }

    pub(super) fn mark(&mut self, phase: &'static str) {
        if self.0.is_none() {
            return;
        }
        let end = ThreadCpuSample::now();
        if let Some(elapsed) = ThreadCpuSample::elapsed(self.0, end) {
            COSTS.with(|costs| {
                *costs
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .entry(phase)
                    .or_default() += elapsed;
            });
        }
        self.0 = end;
    }
}

struct Fixture {
    ctx: Arc<AppContext>,
    checkout: Arc<DataCheckout>,
    plane: VectorPlane,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(files: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"status-probe\"\n",
        )
        .unwrap();
        for file in 0..files {
            let source = (0..16)
                .map(|function| {
                    format!("pub fn item_{file}_{function}() -> usize {{ {file} + {function} }}\n")
                })
                .collect::<String>();
            std::fs::write(root.join(format!("file_{file}.rs")), source).unwrap();
        }
        let root = std::fs::canonicalize(root).unwrap();
        let storage = dir.path().join("storage");
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(storage.clone());
        let semantic = config.semantic.clone();
        let ctx = Arc::new(AppContext::from_app(
            crate::context::App::default_shared(),
            config,
        ));
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx.set_canonical_cache_root(root.clone());
        std::fs::create_dir_all(&storage).unwrap();
        let mut db = crate::db::TrackedConnection::open(
            &storage.join("aft.db"),
            crate::db::SqliteStore::AftDb,
        )
        .unwrap();
        crate::db::run_migrations(&mut db).unwrap();
        db.execute_batch("BEGIN").unwrap();
        let project_key = crate::path_identity::project_scope_key(&root);
        let harness = ctx.harness().storage_segment();
        for event in 0..10_000 {
            crate::db::compression_events::insert_compression_event(
                &db,
                &crate::db::compression_events::CompressionEventRow {
                    harness: &harness,
                    session_id: Some(DEFAULT_SESSION_ID),
                    project_key: &project_key,
                    tool: "bash",
                    task_id: Some(&format!("task-{event}")),
                    compressor: "probe",
                    original_bytes: 1000,
                    compressed_bytes: 100,
                    original_tokens: 250,
                    compressed_tokens: 25,
                    created_at: 1_700_000_000_000,
                },
            )
            .unwrap();
        }
        db.execute_batch("COMMIT").unwrap();
        db.trace(Some(trace_sql));
        ctx.set_db(Arc::new(std::sync::Mutex::new(db)));
        let checkout = DataCheckout::open_planes(
            &root,
            &storage,
            None,
            crate::data_layer::checkout::Planes::ALL,
        )
        .unwrap();
        assert!(matches!(
            checkout.fresh_view(Duration::from_secs(120)),
            Fresh::Ready(_)
        ));
        let fingerprint =
            crate::semantic_index::SemanticIndexFingerprint::for_config_dimension(&semantic, 2);
        let plane = VectorPlane::new(
            checkout.checkout_arc_for_test(),
            Producer::from_fingerprint(&fingerprint),
            files + 10,
        );
        let filled = plane
            .fill(
                &mut |texts| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
                FillBudget::default(),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(filled.files_left, 0);
        checkout.read_meaning_only(&semantic, &fingerprint).unwrap();
        ctx.pin_data_checkout(Arc::clone(&checkout));
        *ctx.checkpoint().lock() =
            crate::checkpoint::CheckpointStore::unbound_in_for_test(&storage);
        for checkpoint in 0..20 {
            ctx.checkpoint()
                .lock()
                .create_for_files(
                    DEFAULT_SESSION_ID,
                    &format!("checkpoint_{checkpoint}"),
                    vec![root.join("file_0.rs")],
                )
                .unwrap();
        }
        Self {
            ctx,
            checkout,
            plane,
            _dir: dir,
        }
    }

    fn edit(&self, edit: usize) {
        let path = self.checkout.root().join("file_0.rs");
        std::fs::write(&path, format!("pub fn edited_{edit}() {{}}\n")).unwrap();
        self.checkout.note_paths(&[path]);
        assert!(matches!(
            self.checkout.fresh_view(Duration::from_secs(60)),
            Fresh::Ready(_)
        ));
    }
}

#[test]
fn status_push_matches_legacy_after_fill_refresh_checkpoint_create_and_delete_without_store_reads()
{
    let fixture = Fixture::new(32);
    fixture.ctx.bind_status_context();
    let assert_parity = || {
        // Explicit status retains the old hydration/report/disk-walk function.
        // Disk bytes on pushes are now a timestamped sample, so take a fresh
        // sample while the fixture is idle before comparing that measurement.
        fixture.checkout.disk_measurement().expire_for_test();
        let start = Instant::now();
        loop {
            let push = fixture.ctx.try_build_status_push_snapshot().unwrap();
            if push["disk"]["data_store_bytes_stale"] == false
                && push["compression"].get("stale").is_none()
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "status producers did not settle: {push}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        crate::checkpoint::status_read_counters_for_test();
        crate::data_layer::vectors::status_member_builds_for_test();
        crate::data_layer::surfaces::status_disk_walks_for_test();
        SQL.with(|statements| statements.borrow_mut().clear());
        let mut push = fixture.ctx.try_build_status_push_snapshot().unwrap();
        assert_eq!(crate::checkpoint::status_read_counters_for_test().0, 0);
        assert_eq!(
            crate::data_layer::vectors::status_member_builds_for_test(),
            0
        );
        assert_eq!(crate::data_layer::surfaces::status_disk_walks_for_test(), 0);
        let sql = SQL.with(|statements| std::mem::take(&mut *statements.borrow_mut()));
        assert!(
            sql.iter()
                .all(|sql| !sql.contains("SUM(") && !sql.contains("COUNT(")),
            "push scanned SQLite: {sql:?}"
        );
        assert_eq!(push["disk"]["data_store_bytes_stale"], false);
        push["disk"]
            .as_object_mut()
            .unwrap()
            .remove("data_store_bytes_stale");
        let old = crate::status_push::project(
            &fixture.ctx,
            fixture
                .ctx
                .build_status_snapshot_for_session(DEFAULT_SESSION_ID),
        );
        assert_eq!(push, old);
    };
    assert_parity();
    fixture.edit(1000);
    fixture
        .plane
        .fill(
            &mut |texts| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
            FillBudget::default(),
            &mut |_| {},
        )
        .unwrap();
    fixture.checkout.refresh_meaning_for_test();
    assert_parity();
    fixture
        .ctx
        .checkpoint()
        .lock()
        .create_for_files(
            DEFAULT_SESSION_ID,
            "new_checkpoint",
            vec![fixture.checkout.root().join("file_0.rs")],
        )
        .unwrap();
    assert_parity();
    assert!(fixture
        .ctx
        .checkpoint()
        .lock()
        .delete(DEFAULT_SESSION_ID, "new_checkpoint"));
    assert_parity();
    assert!(fixture
        .ctx
        .checkpoint()
        .lock()
        .delete(DEFAULT_SESSION_ID, "checkpoint_1"));
    assert_eq!(
        fixture.ctx.try_build_status_push_snapshot().unwrap()["checkpoints_total"],
        18
    );
    let mut external = crate::checkpoint::CheckpointStore::unbound_in_for_test(
        &fixture._dir.path().join("storage"),
    );
    external
        .create_for_files(
            DEFAULT_SESSION_ID,
            "external",
            vec![fixture.checkout.root().join("file_0.rs")],
        )
        .unwrap();
    let push = fixture.ctx.try_build_status_push_snapshot().unwrap();
    assert_eq!(push["session"]["checkpoints"], 19);
    assert_eq!(push["checkpoints_total"], 19);
    assert_eq!(
        fixture
            .ctx
            .checkpoint()
            .lock()
            .resident_session_count(DEFAULT_SESSION_ID),
        18,
        "push must not hydrate"
    );

    // Record a new text view without running its producer. The previous
    // coverage is no longer a certificate for this view: a push must report
    // unknown counts rather than rebuild or borrow the previous coverage.
    fixture.checkout.pause_meaning_summary_for_test();
    std::fs::write(
        fixture.checkout.root().join("file_0.rs"),
        "pub fn pending_view() {}\n",
    )
    .unwrap();
    fixture
        .checkout
        .checkout()
        .record_paths(&[b"file_0.rs".to_vec()])
        .unwrap();
    crate::data_layer::vectors::status_member_builds_for_test();
    crate::checkpoint::status_read_counters_for_test();
    crate::data_layer::surfaces::status_disk_walks_for_test();
    SQL.with(|statements| statements.borrow_mut().clear());
    let pending = fixture.ctx.try_build_status_push_snapshot().unwrap();
    assert_eq!(
        crate::data_layer::vectors::status_member_builds_for_test(),
        0
    );
    assert_eq!(crate::checkpoint::status_read_counters_for_test().0, 0);
    assert_eq!(crate::data_layer::surfaces::status_disk_walks_for_test(), 0);
    assert!(pending["semantic_index"]["files"].is_null(), "{pending}");
    assert!(SQL.with(|statements| statements
        .borrow()
        .iter()
        .all(|sql| !sql.contains("SUM(") && !sql.contains("COUNT("))));
}

#[test]
#[ignore = "manual per-phase CPU and edit-loop push frequency probe"]
fn status_push_filled_store_cost_probe() {
    let fixture = Fixture::new(256);
    for edited in [false, true] {
        db_page_reads(&fixture.ctx);
        SQL.with(|statements| statements.borrow_mut().clear());
        COSTS.with(|costs| *costs.borrow_mut() = Some(BTreeMap::new()));
        let mut cpu = Duration::ZERO;
        for sample in 0..50 {
            if edited {
                fixture.edit(sample);
            }
            let start = ThreadCpuSample::now();
            std::hint::black_box(fixture.ctx.try_build_status_push_snapshot().unwrap());
            cpu += ThreadCpuSample::elapsed(start, ThreadCpuSample::now()).unwrap();
        }
        let phases = COSTS.with(|costs| costs.borrow_mut().take().unwrap());
        eprintln!(
            "status cost: edited={edited}, files=256, checkpoints=20, pushes=50, CPU/push={:?}",
            cpu / 50
        );
        for (phase, elapsed) in phases {
            eprintln!("  {phase}: {:?}/push", elapsed / 50);
        }
        let statements = SQL.with(|statements| std::mem::take(&mut *statements.borrow_mut()));
        let mut grouped = BTreeMap::new();
        for sql in statements {
            *grouped.entry(sql).or_insert(0usize) += 1;
        }
        eprintln!("SQL across 50 pushes: {grouped:?}");
        eprintln!(
            "SQLite page-cache hits/misses across 50 pushes: {:?}",
            db_page_reads(&fixture.ctx)
        );
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    fixture
        .ctx
        .set_progress_sender(Some(Arc::new(Box::new(move |frame| {
            let _ = sender.send(frame);
        }))));
    fixture.ctx.bind_status_context();
    let start = Instant::now();
    let before = fixture
        .ctx
        .status_emitter()
        .snapshot_builds
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut edits = 0;
    while start.elapsed() < Duration::from_secs(60) {
        fixture.edit(edits);
        fixture.ctx.signal_status_changed();
        edits += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    receiver.recv_timeout(Duration::from_secs(60)).unwrap();
    let after = fixture
        .ctx
        .status_emitter()
        .snapshot_builds
        .load(std::sync::atomic::Ordering::Relaxed);
    eprintln!(
        "edit loop: {} edits in {:?}, {} snapshot builds, {:.1} builds/root/min",
        edits,
        start.elapsed(),
        after - before,
        (after - before) as f64 * 60.0 / start.elapsed().as_secs_f64()
    );
}

#[test]
#[ignore = "manual deferred backup-inventory cost probe"]
fn status_push_backup_inventory_cost_probe() {
    let fixture = Fixture::new(256);
    let mut backup = fixture.ctx.backup().lock();
    backup.set_storage_dir_for_harness(
        fixture._dir.path().join("storage"),
        crate::harness::Harness::Opencode,
        0,
    );
    for file in 0..256 {
        assert!(backup
            .snapshot(
                DEFAULT_SESSION_ID,
                &fixture.checkout.root().join(format!("file_{file}.rs")),
                "status inventory probe"
            )
            .unwrap()
            .is_some());
    }
    for resident in [true, false] {
        if !resident {
            backup.release_session_memory(DEFAULT_SESSION_ID);
        }
        let before = backup.status_discovery_entries_for_test();
        let start = ThreadCpuSample::now();
        for _ in 0..50 {
            assert_eq!(
                std::hint::black_box(backup.tracked_files(DEFAULT_SESSION_ID)).len(),
                256
            );
        }
        let cpu = ThreadCpuSample::elapsed(start, ThreadCpuSample::now()).unwrap();
        eprintln!("backup inventory (unchanged push call): paths=256, resident={resident}, calls=50, CPU/call={:?}, directory entries/call={}", cpu/50, (backup.status_discovery_entries_for_test() - before)/50);
    }
}

/// Compares the unchanged `tracked_files` call with listing-only alternatives
/// over the same 256 durable stacks. The listing variants never open a
/// `meta.json`: they read directory names (path-hash names are 16 lowercase hex
/// digits) and union them with the hashed resident keys.
#[test]
#[ignore = "manual backup-inventory listing cost probe"]
fn status_push_backup_listing_cost_probe() {
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;
    fn path_hash(key: &std::path::Path) -> String {
        Sha256::digest(key.to_string_lossy().as_bytes())[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    fn is_path_hash(name: &str) -> bool {
        name.len() == 16 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }
    fn find_session_dir(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        if dir.join("session.json").is_file() {
            return Some(dir.to_path_buf());
        }
        std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .find_map(|entry| find_session_dir(&entry.path()))
    }

    let fixture = Fixture::new(256);
    let mut backup = fixture.ctx.backup().lock();
    let storage = fixture._dir.path().join("storage");
    backup.set_storage_dir_for_harness(storage.clone(), crate::harness::Harness::Opencode, 0);
    for file in 0..256 {
        assert!(backup
            .snapshot(
                DEFAULT_SESSION_ID,
                &fixture.checkout.root().join(format!("file_{file}.rs")),
                "status inventory probe"
            )
            .unwrap()
            .is_some());
    }
    let session_dir = find_session_dir(&storage).unwrap();
    let mut others = std::fs::read_dir(&session_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !is_path_hash(name))
        .collect::<Vec<_>>();
    others.sort();
    let resident_keys = backup.tracked_files(DEFAULT_SESSION_ID);
    eprintln!("session dir non-path-hash entries: {others:?}");

    let names_only = |resident: &[std::path::PathBuf]| -> usize {
        let mut names: HashSet<String> = resident.iter().map(|key| path_hash(key)).collect();
        if !session_dir.exists() {
            return names.len();
        }
        for entry in std::fs::read_dir(&session_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_path_hash(name) && entry.file_type().unwrap().is_dir() {
                names.insert(name.to_string());
            }
        }
        names.len()
    };
    let names_and_meta_stat = |resident: &[std::path::PathBuf]| -> usize {
        let mut names: HashSet<String> = resident.iter().map(|key| path_hash(key)).collect();
        if !session_dir.exists() {
            return names.len();
        }
        for entry in std::fs::read_dir(&session_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_path_hash(name) || !entry.file_type().unwrap().is_dir() {
                continue;
            }
            // Length and modification time are what a stat-keyed parse cache
            // would compare; reading them costs one stat per stack.
            if let Ok(meta) = std::fs::symlink_metadata(entry.path().join("meta.json")) {
                std::hint::black_box((meta.len(), meta.modified().ok()));
                names.insert(name.to_string());
            }
        }
        names.len()
    };

    for resident in [true, false] {
        if !resident {
            backup.release_session_memory(DEFAULT_SESSION_ID);
        }
        let resident_now: Vec<std::path::PathBuf> = if resident {
            resident_keys.clone()
        } else {
            Vec::new()
        };
        let measure = |label: &str, f: &dyn Fn() -> usize| {
            let start = ThreadCpuSample::now();
            for _ in 0..50 {
                assert_eq!(std::hint::black_box(f()), 256);
            }
            let cpu = ThreadCpuSample::elapsed(start, ThreadCpuSample::now()).unwrap();
            eprintln!("{label}: resident={resident}, calls=50, CPU/call={:?}", cpu / 50);
        };
        measure("tracked_files (unchanged)", &|| backup.tracked_files(DEFAULT_SESSION_ID).len());
        measure("names only + hashed resident keys", &|| names_only(&resident_now));
        measure("names + meta.json stat + hashed resident keys", &|| {
            names_and_meta_stat(&resident_now)
        });
    }
}
