use std::cell::Cell;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, LazyLock, Mutex};
use std::thread;

use rusqlite::Connection;

const REVERT_V9_TO_V8: &str = r#"
DROP TABLE IF EXISTS compression_event_rollups;
DROP TABLE IF EXISTS compression_retention_cursor;
DROP INDEX IF EXISTS idx_compression_created;

DROP INDEX IF EXISTS idx_bash_pattern_watches_session;
DROP INDEX IF EXISTS idx_bash_pattern_watches_task;
ALTER TABLE bash_pattern_watches RENAME TO bash_pattern_watches_with_task_fk;
CREATE TABLE IF NOT EXISTS bash_pattern_watches (
  harness        TEXT NOT NULL,
  session_id     TEXT NOT NULL,
  task_id        TEXT NOT NULL,
  watch_id       TEXT NOT NULL,
  pattern_kind   TEXT NOT NULL,
  pattern        TEXT NOT NULL,
  once           INTEGER NOT NULL DEFAULT 1,
  created_at     INTEGER NOT NULL,
  stdout_offset  INTEGER NOT NULL DEFAULT 0,
  stderr_offset  INTEGER NOT NULL DEFAULT 0,
  pty_offset     INTEGER NOT NULL DEFAULT 0,
  scanning       INTEGER NOT NULL DEFAULT 1,
  pending_match  INTEGER NOT NULL DEFAULT 0,
  match_text     TEXT,
  match_offset   INTEGER,
  match_context  TEXT,
  PRIMARY KEY (harness, session_id, task_id, watch_id)
);
INSERT INTO bash_pattern_watches (
  harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
  created_at, stdout_offset, stderr_offset, pty_offset, scanning,
  pending_match, match_text, match_offset, match_context
)
SELECT
  harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
  created_at, stdout_offset, stderr_offset, pty_offset, scanning,
  pending_match, match_text, match_offset, match_context
FROM bash_pattern_watches_with_task_fk;
DROP TABLE bash_pattern_watches_with_task_fk;
CREATE INDEX IF NOT EXISTS idx_bash_pattern_watches_session
  ON bash_pattern_watches (harness, session_id);
CREATE INDEX IF NOT EXISTS idx_bash_pattern_watches_task
  ON bash_pattern_watches (harness, session_id, task_id);

DELETE FROM schema_version;
INSERT INTO schema_version (version) VALUES (8);
"#;

const REPLAY_V9_AND_RECORD_9: &str = r#"
DROP INDEX IF EXISTS idx_bash_pattern_watches_session;
DROP INDEX IF EXISTS idx_bash_pattern_watches_task;
ALTER TABLE bash_pattern_watches RENAME TO bash_pattern_watches_without_task_fk;
CREATE TABLE bash_pattern_watches (
  harness        TEXT NOT NULL,
  session_id     TEXT NOT NULL,
  task_id        TEXT NOT NULL,
  watch_id       TEXT NOT NULL,
  pattern_kind   TEXT NOT NULL,
  pattern        TEXT NOT NULL,
  once           INTEGER NOT NULL DEFAULT 1,
  created_at     INTEGER NOT NULL,
  stdout_offset  INTEGER NOT NULL DEFAULT 0,
  stderr_offset  INTEGER NOT NULL DEFAULT 0,
  pty_offset     INTEGER NOT NULL DEFAULT 0,
  scanning       INTEGER NOT NULL DEFAULT 1,
  pending_match  INTEGER NOT NULL DEFAULT 0,
  match_text     TEXT,
  match_offset   INTEGER,
  match_context  TEXT,
  PRIMARY KEY (harness, session_id, task_id, watch_id),
  FOREIGN KEY (harness, session_id, task_id)
    REFERENCES bash_tasks (harness, session_id, task_id) ON DELETE CASCADE
);
INSERT INTO bash_pattern_watches (
  harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
  created_at, stdout_offset, stderr_offset, pty_offset, scanning,
  pending_match, match_text, match_offset, match_context
)
SELECT
  watch.harness, watch.session_id, watch.task_id, watch.watch_id,
  watch.pattern_kind, watch.pattern, watch.once, watch.created_at,
  watch.stdout_offset, watch.stderr_offset, watch.pty_offset, watch.scanning,
  watch.pending_match, watch.match_text, watch.match_offset, watch.match_context
FROM bash_pattern_watches_without_task_fk AS watch
WHERE EXISTS (
  SELECT 1
  FROM bash_tasks AS task
  WHERE task.harness = watch.harness
    AND task.session_id = watch.session_id
    AND task.task_id = watch.task_id
);
DROP TABLE bash_pattern_watches_without_task_fk;
CREATE INDEX idx_bash_pattern_watches_session
  ON bash_pattern_watches (harness, session_id);
CREATE INDEX idx_bash_pattern_watches_task
  ON bash_pattern_watches (harness, session_id, task_id);
DELETE FROM schema_version;
INSERT INTO schema_version (version) VALUES (9);
"#;

static MIGRATION_WRITE_TRANSACTIONS: AtomicUsize = AtomicUsize::new(0);
static STALE_PLAN_BARRIER: LazyLock<Mutex<Option<Arc<Barrier>>>> =
    LazyLock::new(|| Mutex::new(None));
static CONCURRENT_PLAN_BARRIER: LazyLock<Mutex<Option<Arc<Barrier>>>> =
    LazyLock::new(|| Mutex::new(None));

thread_local! {
    static STALE_PLAN_PAUSED: Cell<bool> = const { Cell::new(false) };
    static CONCURRENT_PLAN_PAUSED: Cell<bool> = const { Cell::new(false) };
}

fn count_migration_write_transactions(sql: &str) {
    if sql.starts_with("BEGIN IMMEDIATE") {
        MIGRATION_WRITE_TRANSACTIONS.fetch_add(1, Ordering::SeqCst);
    }
}

fn pause_stale_plan_before_first_write(sql: &str) {
    if !sql.starts_with("BEGIN IMMEDIATE") {
        return;
    }
    STALE_PLAN_PAUSED.with(|paused| {
        if paused.replace(true) {
            return;
        }
        let barrier = STALE_PLAN_BARRIER
            .lock()
            .expect("stale-plan barrier lock")
            .clone()
            .expect("stale-plan barrier installed");
        barrier.wait();
        barrier.wait();
    });
}

fn pause_concurrent_plans_before_first_write(sql: &str) {
    if !sql.starts_with("BEGIN IMMEDIATE") {
        return;
    }
    CONCURRENT_PLAN_PAUSED.with(|paused| {
        if paused.replace(true) {
            return;
        }
        let barrier = CONCURRENT_PLAN_BARRIER
            .lock()
            .expect("concurrent-plan barrier lock")
            .clone()
            .expect("concurrent-plan barrier installed");
        barrier.wait();
    });
}

fn create_v8_database(path: &Path) {
    let conn = aft::db::open(path).expect("create current database");
    conn.execute_batch(REVERT_V9_TO_V8)
        .expect("restore the schema to v8");
    assert_eq!(schema_version(&conn), 8);
}

fn schema_version(conn: &Connection) -> u32 {
    conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
        row.get(0)
    })
    .expect("read schema version")
}

fn v10_objects(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE name IN (
           'compression_event_rollups',
           'compression_retention_cursor',
           'idx_compression_created'
         )
         ORDER BY name",
    )
    .expect("prepare v10 object query")
    .query_map([], |row| row.get(0))
    .expect("query v10 objects")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("collect v10 objects")
}

fn schema_sql(conn: &Connection) -> Vec<Option<String>> {
    conn.prepare("SELECT sql FROM sqlite_master ORDER BY name")
        .expect("prepare schema query")
        .query_map([], |row| row.get(0))
        .expect("query schema")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect schema")
}

#[test]
fn migration_fast_path_avoids_writes_and_v8_runs_only_two_steps() {
    let current_storage = tempfile::tempdir().expect("current temporary storage");
    let current_database = current_storage.path().join("aft.db");
    drop(aft::db::open(&current_database).expect("create current database"));
    let mut current = Connection::open(&current_database).expect("open current connection");
    aft::db::apply_pragmas(&current).expect("apply current connection pragmas");
    MIGRATION_WRITE_TRANSACTIONS.store(0, Ordering::SeqCst);
    current.trace(Some(count_migration_write_transactions));

    aft::db::run_migrations(&mut current).expect("open current schema");

    assert_eq!(MIGRATION_WRITE_TRANSACTIONS.load(Ordering::SeqCst), 0);
    current.trace(None);

    let v8_storage = tempfile::tempdir().expect("v8 temporary storage");
    let v8_database = v8_storage.path().join("aft.db");
    create_v8_database(&v8_database);
    let mut v8 = Connection::open(&v8_database).expect("open v8 connection");
    aft::db::apply_pragmas(&v8).expect("apply v8 connection pragmas");
    MIGRATION_WRITE_TRANSACTIONS.store(0, Ordering::SeqCst);
    v8.trace(Some(count_migration_write_transactions));

    aft::db::run_migrations(&mut v8).expect("migrate v8 schema");

    assert_eq!(MIGRATION_WRITE_TRANSACTIONS.load(Ordering::SeqCst), 2);
}

#[test]
fn stale_planned_migrations_cannot_regress_the_schema_or_wedge_open() {
    let storage = tempfile::tempdir().expect("temporary storage");
    let database = storage.path().join("aft.db");
    create_v8_database(&database);

    let barrier = Arc::new(Barrier::new(2));
    *STALE_PLAN_BARRIER.lock().expect("stale-plan barrier lock") = Some(Arc::clone(&barrier));
    let database_for_stale_opener = database.clone();
    let stale_opener = thread::spawn(move || {
        let mut conn = Connection::open(database_for_stale_opener).expect("open stale connection");
        aft::db::apply_pragmas(&conn).expect("apply stale connection pragmas");
        conn.trace(Some(pause_stale_plan_before_first_write));
        aft::db::run_migrations(&mut conn).map_err(|error| error.to_string())
    });

    barrier.wait();
    aft::db::open(&database).expect("opener A migrates v8 to v10");
    barrier.wait();
    let stale_result = stale_opener.join().expect("stale opener did not panic");
    *STALE_PLAN_BARRIER.lock().expect("stale-plan barrier lock") = None;

    let observed = Connection::open(&database).expect("inspect migration outcome");
    let observed_version = schema_version(&observed);
    let observed_objects = v10_objects(&observed);
    drop(observed);
    let fresh_open = aft::db::open(&database)
        .map(|_| ())
        .map_err(|error| error.to_string());

    if let Err(stale_error) = stale_result {
        assert_eq!(
            observed_version, 9,
            "stale V9 must expose the reported regression"
        );
        assert_eq!(
            observed_objects,
            vec![
                "compression_event_rollups",
                "compression_retention_cursor",
                "idx_compression_created",
            ],
            "the failed stale V10 must leave every V10 object present"
        );
        let fresh_error =
            fresh_open.expect_err("the reported wedged database must refuse a fresh open");
        panic!(
            "stale migration plan wedged aft.db: stale opener: {stale_error}; fresh opener: {fresh_error}"
        );
    }

    assert_eq!(observed_version, aft::db::CURRENT_SCHEMA_VERSION);
    assert!(fresh_open.is_ok(), "fresh open must succeed after the race");
}

#[test]
fn open_repairs_reported_version_nine_wedge_with_v10_objects_intact() {
    let storage = tempfile::tempdir().expect("temporary storage");
    let database = storage.path().join("aft.db");
    let conn = aft::db::open(&database).expect("create v10 database");
    conn.execute_batch(REPLAY_V9_AND_RECORD_9)
        .expect("reproduce stale V9 commit after V10");
    assert_eq!(schema_version(&conn), 9);
    assert_eq!(v10_objects(&conn).len(), 3);
    drop(conn);

    let repaired = aft::db::open(&database).expect("self-heal reported wedge");

    assert_eq!(schema_version(&repaired), aft::db::CURRENT_SCHEMA_VERSION);
    assert_eq!(v10_objects(&repaired).len(), 3);
}

#[test]
fn eight_concurrent_v8_openers_converge_on_the_fresh_v10_schema() {
    let storage = tempfile::tempdir().expect("temporary storage");
    let database = storage.path().join("aft.db");
    create_v8_database(&database);

    let barrier = Arc::new(Barrier::new(8));
    *CONCURRENT_PLAN_BARRIER
        .lock()
        .expect("concurrent-plan barrier lock") = Some(Arc::clone(&barrier));
    let handles = (0..8)
        .map(|_| {
            let database = database.clone();
            thread::spawn(move || {
                let mut conn = Connection::open(database).expect("open concurrent connection");
                aft::db::apply_pragmas(&conn).expect("apply concurrent connection pragmas");
                conn.trace(Some(pause_concurrent_plans_before_first_write));
                aft::db::run_migrations(&mut conn).map_err(|error| error.to_string())
            })
        })
        .collect::<Vec<_>>();

    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().expect("concurrent opener did not panic"))
        .collect::<Vec<_>>();
    *CONCURRENT_PLAN_BARRIER
        .lock()
        .expect("concurrent-plan barrier lock") = None;
    let failures = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().err())
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "concurrent migration failures: {failures:?}"
    );

    let migrated = Connection::open(&database).expect("open concurrently migrated database");
    assert_eq!(schema_version(&migrated), aft::db::CURRENT_SCHEMA_VERSION);

    let fresh_storage = tempfile::tempdir().expect("fresh comparison storage");
    let fresh = aft::db::open(&fresh_storage.path().join("aft.db"))
        .expect("create fresh v10 comparison database");
    assert_eq!(schema_sql(&migrated), schema_sql(&fresh));
}
