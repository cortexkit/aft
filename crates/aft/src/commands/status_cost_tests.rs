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
