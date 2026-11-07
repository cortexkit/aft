//! Several AFT processes (standalone OpenCode windows, Pi, the CLI) share one
//! `aft.db`. In each test here the test process holds a write transaction open
//! on that database through its own single connection, while a standalone
//! `aft` child runs its storage maintenance and serves requests (#375).
//!
//! The hold outlasts the steady five-second SQLite busy wait, which is how the
//! reported `database is locked` fold failures, per-minute retention warnings
//! and five-second `status` timeouts arose.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aft::db::TrackedConnection as Connection;
use serde_json::json;

use super::helpers::AftProcess;

/// Longer than the steady busy wait, so any maintenance still using it fails.
const HOLD: Duration = Duration::from_secs(7);
/// How long the child's maintenance keeps retrying a write that meets the
/// held write lock. The retry gate can hold this phase open independently of
/// the budget when a test needs to establish request/maintenance ordering.
const MAINTENANCE_BUSY_BUDGET_MS: &str = "2000";

struct Contended {
    _project: tempfile::TempDir,
    storage: tempfile::TempDir,
    aft: AftProcess,
    /// The only connection this test process opens on the shared database.
    conn: Connection,
}

impl Contended {
    fn start() -> Self {
        Self::start_with_retry_gate(None)
    }

    fn start_with_retry_gate(gate: Option<&std::path::Path>) -> Self {
        let project = crate::test_helpers::tempdir();
        let storage = crate::test_helpers::tempdir();
        let mut env = vec![
            ("AFT_CACHE_DIR", storage.path().as_os_str()),
            // Run maintenance every few hundred milliseconds instead of
            // every minute, so several runs fall inside the hold.
            (
                "AFT_TEST_MAINTENANCE_INTERVAL_MS",
                std::ffi::OsStr::new("300"),
            ),
            (
                "AFT_TEST_MAINTENANCE_BUSY_BUDGET_MS",
                std::ffi::OsStr::new(MAINTENANCE_BUSY_BUDGET_MS),
            ),
            ("RUST_LOG", std::ffi::OsStr::new("info,aft::db=debug")),
        ];
        if let Some(gate) = gate {
            env.push(("AFT_TEST_MAINTENANCE_RETRY_GATE", gate.as_os_str()));
        }
        let mut aft = AftProcess::spawn_with_env(&env);
        let response = aft.send(
            &json!({
                "id": "cfg",
                "command": "configure",
                "harness": "opencode",
                "project_root": project.path(),
                "storage_dir": storage.path(),
            })
            .to_string(),
        );
        assert_eq!(response["success"], true, "configure failed: {response:?}");
        let conn = aft::db::open(&storage.path().join("aft.db")).expect("open shared aft.db");
        let contended = Self {
            _project: project,
            storage,
            aft,
            conn,
        };
        // Start contending only once the child has committed a fold, so the
        // folds under test are the ones that meet the held write lock.
        contended.wait_for(Duration::from_secs(20), "first write-ledger fold", |c| {
            c.conn
                .query_row("SELECT COUNT(*) FROM write_ledger_meta", [], |row| {
                    row.get::<_, i64>(0)
                })
                .is_ok_and(|rows| rows > 0)
        });
        contended
    }

    fn log_path(&self) -> PathBuf {
        self.storage
            .path()
            .join("logs")
            .join(format!("aft-{}.log", self.aft.pid()))
    }

    fn log(&self) -> String {
        self.log_since(0)
    }

    /// The child's durable log from byte `start` on. The file only grows.
    fn log_since(&self, start: usize) -> String {
        let bytes = std::fs::read(self.log_path()).unwrap_or_default();
        String::from_utf8_lossy(bytes.get(start..).unwrap_or_default()).into_owned()
    }

    fn log_len(&self) -> usize {
        std::fs::metadata(self.log_path()).map_or(0, |meta| meta.len() as usize)
    }

    fn wait_for(&self, timeout: Duration, what: &str, ready: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + timeout;
        while !ready(self) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; child log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Hold the database write lock for [`HOLD`], calling `during` repeatedly
    /// meanwhile. Returns the log's length when the hold began.
    fn hold_write_lock(&mut self, mut during: impl FnMut(&mut AftProcess)) -> usize {
        let log_start = self.log_len();
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .expect("take the write lock");
        let held_since = Instant::now();
        while held_since.elapsed() < HOLD {
            during(&mut self.aft);
            std::thread::sleep(Duration::from_millis(100));
        }
        self.conn
            .execute_batch("COMMIT")
            .expect("release the write lock");
        log_start
    }

    /// Keep the lock held for the complete assertion instead of relying on a
    /// short sampling window that can end before a delayed status call runs.
    fn with_write_lock(&mut self, during: impl FnOnce(&mut AftProcess, &Connection)) {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .expect("take the write lock");
        during(&mut self.aft, &self.conn);
        self.conn
            .execute_batch("COMMIT")
            .expect("release the write lock");
    }

    /// Stop the child and return its complete durable log.
    fn finish(self) -> String {
        let log_path = self.log_path();
        let status = self.aft.shutdown();
        assert!(status.success(), "aft exited with {status}");
        std::fs::read_to_string(&log_path).unwrap_or_default()
    }
}

fn warn_lines<'a>(log: &'a str, needle: &str) -> Vec<&'a str> {
    log.lines()
        .filter(|line| line.contains(" WARN ") && line.contains(needle))
        .collect()
}

/// A fold that meets another process's write lock waits briefly, then defers
/// without warning; once the lock is released a later fold commits the data
/// the deferred one kept in memory.
#[test]
fn minute_fold_defers_under_a_foreign_write_lock_and_commits_after_release() {
    let mut contended = Contended::start();
    let log_start = contended.hold_write_lock(|_| {});
    let held_log = contended.log_since(log_start);
    let warnings = warn_lines(&held_log, "write ledger minute fold");
    assert!(
        warnings.is_empty(),
        "fold warned under contention: {warnings:#?}"
    );
    assert!(
        held_log.contains("write ledger minute fold deferred"),
        "no fold met the held write lock; child log:\n{held_log}"
    );
    let released_at = contended.log_len();
    contended.wait_for(
        Duration::from_secs(10),
        "a fold committed after the lock was released",
        |c| {
            c.log_since(released_at)
                .contains("write ledger minute fold committed")
        },
    );

    let log = contended.finish();
    let warnings = warn_lines(&log, "write ledger minute fold");
    assert!(
        warnings.is_empty(),
        "fold warned under contention: {warnings:#?}"
    );
}

struct MaintenanceRetryGate(tempfile::TempDir);

impl MaintenanceRetryGate {
    fn new() -> Self {
        Self(crate::test_helpers::tempdir())
    }

    fn is_paused(&self) -> bool {
        self.0.path().join("paused").exists() && !self.0.path().join("release").exists()
    }

    fn release(&self) {
        std::fs::write(self.0.path().join("release"), b"resume retry")
            .expect("release maintenance retry");
    }

    fn evidence(&self, name: &str) -> serde_json::Value {
        let path = self.0.path().join(name);
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|error| panic!("missing contention event {}: {error}", path.display()));
        serde_json::from_slice(&bytes).expect("contention event JSON")
    }

    fn clear_read_evidence(&self) {
        for name in ["read-requested", "read-waiting", "read-acquired"] {
            let _ = std::fs::remove_file(self.0.path().join(name));
        }
    }
}

impl Drop for MaintenanceRetryGate {
    fn drop(&mut self) {
        // Unblock the child even if an assertion unwinds before normal cleanup.
        let _ = std::fs::write(self.0.path().join("release"), b"resume retry");
    }
}

/// Status and a real database read must both finish while a maintenance busy
/// retry is paused. Waiting out its busy budget cannot satisfy this ordering.
#[test]
fn status_answers_while_another_process_holds_the_write_lock() {
    let gate = MaintenanceRetryGate::new();
    let mut contended = Contended::start_with_retry_gate(Some(gate.0.path()));
    let log_path = contended.log_path();
    let mut calls = 0;
    contended.with_write_lock(|aft, writer| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !gate.is_paused() {
            assert!(
                Instant::now() < deadline,
                "maintenance never encountered the held write lock: {}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        for _ in 0..5 {
            let response = aft.send_with_timeout(
                &json!({ "id": "status", "command": "status" }).to_string(),
                Duration::from_secs(30),
            );
            assert_eq!(response["success"], true, "status failed: {response:?}");
            // Status may serve its cached totals if the mutex is held. Probe
            // the actual connection too, so a maintenance critical section
            // that spans SQLite's busy retry cannot be hidden by that fallback.
            let read = aft.send_with_timeout(
                &json!({
                    "id": "read-during-retry", "command": "db_get_state",
                    "params": { "key": "contention-probe" },
                })
                .to_string(),
                Duration::from_secs(30),
            );
            assert_eq!(read["success"], true, "database read failed: {read:?}");
            assert!(
                gate.is_paused(),
                "requests must finish before maintenance resumes"
            );
            assert!(
                !writer.is_autocommit(),
                "the writer must remain locked until every status response has arrived"
            );
            calls += 1;
        }
    });
    gate.release();
    let log = contended.finish();
    assert!(
        log.contains("maintenance write busy retry paused"),
        "no busy retry: {log}"
    );

    assert_eq!(
        calls, 5,
        "every status and database read must run during a paused maintenance retry"
    );
}

/// A real SQLite BUSY result must release the shared mutex before maintenance
/// waits. Reads must acquire that same connection and return committed data
/// before either maintenance or the foreign writer is allowed to resume.
#[test]
fn database_reads_are_not_held_behind_maintenance_waiting_on_a_foreign_write_lock() {
    let gate = MaintenanceRetryGate::new();
    let mut contended = Contended::start_with_retry_gate(Some(gate.0.path()));
    aft::db::state::set_harness_state(
        &contended.conn,
        "opencode",
        "contention-probe",
        "committed before the foreign write lock",
        0,
    )
    .expect("seed committed read probe");
    let log_path = contended.log_path();
    contended.with_write_lock(|aft, writer| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !gate.is_paused() {
            assert!(
                Instant::now() < deadline,
                "maintenance never released the shared mutex during its foreign-lock retry; busy event: {}; log: {}",
                std::fs::read_to_string(gate.0.path().join("busy")).unwrap_or_default(),
                std::fs::read_to_string(&log_path).unwrap_or_default(),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let maintenance = gate.evidence("paused");
        assert_eq!(maintenance["sqlite_busy"], true, "{maintenance}");
        assert_eq!(
            maintenance["mutex_available"], true,
            "maintenance waited on the foreign lock while holding the shared mutex: {maintenance}"
        );
        assert_eq!(
            maintenance["observed_connection_id"], maintenance["connection_id"],
            "the hook must try_lock the actual maintenance connection: {maintenance}"
        );
        for i in 0..5 {
            gate.clear_read_evidence();
            let response = aft.send_with_timeout(
                &json!({
                    "id": format!("read-{i}"),
                    "command": "db_get_state",
                    "params": { "key": "contention-probe" },
                })
                .to_string(),
                Duration::from_secs(30),
            );
            let read = gate.evidence("read-acquired");
            assert_eq!(
                response["success"], true,
                "read failed: {response:?}; {read}"
            );
            assert_eq!(response["value"], "committed before the foreign write lock");
            assert_eq!(read["mutex_id"], maintenance["mutex_id"], "{read}");
            assert_eq!(
                read["connection_id"], maintenance["connection_id"],
                "{read}"
            );
            assert_eq!(read["maintenance_paused"], true, "{read}");
            assert!(
                gate.is_paused(),
                "read finished after maintenance resumed: {read}"
            );
            assert!(
                !writer.is_autocommit(),
                "foreign writer released before read"
            );
            eprintln!("maintenance={maintenance}; read={read}");
        }
    });
    gate.release();
    contended.finish();
}

/// Retention runs under the same contention without a warning per attempt:
/// losing the connection mutex or the write lock is routine for a
/// maintenance task and is logged at debug level.
#[test]
fn retention_does_not_warn_per_attempt_while_another_process_holds_the_write_lock() {
    let mut contended = Contended::start();
    let log_start = contended.hold_write_lock(|_| {});
    let held_log = contended.log_since(log_start);
    assert!(
        held_log.contains("bash task retention"),
        "retention never ran during the hold; child log:\n{held_log}"
    );

    let log = contended.finish();
    let warnings = warn_lines(&log, "retention");
    assert!(
        warnings.is_empty(),
        "retention warned under contention: {warnings:#?}"
    );
}
