use rusqlite::{Connection, OpenFlags, TransactionBehavior};

pub mod lifecycle;
pub use lifecycle::{
    connection_snapshot, SqliteConnectionSnapshot, SqliteStore, SqliteStoreCount, TrackedConnection,
};
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

pub mod backups;
pub mod bash_tasks;
pub mod bash_watches;
pub mod compression_events;
pub mod github_read_cache;
pub mod removal;
pub mod standing_roots;
pub mod state;

pub const CURRENT_SCHEMA_VERSION: u32 = 8;

const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
  version INTEGER NOT NULL PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS bash_tasks (
  harness      TEXT NOT NULL,
  session_id   TEXT NOT NULL,
  task_id      TEXT NOT NULL,
  project_key  TEXT NOT NULL,
  command      TEXT NOT NULL,
  cwd          TEXT NOT NULL,
  status       TEXT NOT NULL,
  exit_code    INTEGER,
  pid          INTEGER,
  pgid         INTEGER,
  started_at   INTEGER NOT NULL,
  completed_at INTEGER,
  stdout_path  TEXT,
  stderr_path  TEXT,
  compressed   INTEGER NOT NULL DEFAULT 1,
  timeout_ms   INTEGER,
  completion_delivered INTEGER NOT NULL DEFAULT 0,
  output_bytes INTEGER,
  metadata     TEXT,
  PRIMARY KEY (harness, session_id, task_id)
);
CREATE INDEX IF NOT EXISTS idx_bash_tasks_project_key ON bash_tasks(project_key);
CREATE INDEX IF NOT EXISTS idx_bash_tasks_status      ON bash_tasks(status);
CREATE INDEX IF NOT EXISTS idx_bash_tasks_session_status ON bash_tasks(harness, session_id, status);

CREATE TABLE IF NOT EXISTS compression_events (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  harness           TEXT NOT NULL,
  session_id        TEXT,
  project_key       TEXT NOT NULL,
  tool              TEXT NOT NULL,
  task_id           TEXT,
  command           TEXT,
  compressor        TEXT NOT NULL,
  original_bytes    INTEGER NOT NULL,
  compressed_bytes  INTEGER NOT NULL,
  original_tokens   INTEGER NOT NULL,
  compressed_tokens INTEGER NOT NULL,
  created_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_compression_session         ON compression_events(harness, session_id);
CREATE INDEX IF NOT EXISTS idx_compression_session_created ON compression_events(harness, session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_compression_project_key     ON compression_events(project_key);

CREATE TABLE IF NOT EXISTS backups (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  backup_id     TEXT,
  harness       TEXT NOT NULL,
  session_id    TEXT NOT NULL,
  project_key   TEXT NOT NULL,
  op_id         TEXT,
  order_blob    BLOB NOT NULL,
  file_path     TEXT NOT NULL,
  path_hash     TEXT NOT NULL,
  backup_path   TEXT,
  kind          TEXT NOT NULL,
  description   TEXT,
  created_at    INTEGER NOT NULL,
  is_tombstone  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_backups_session_path  ON backups(harness, session_id, path_hash);
CREATE INDEX IF NOT EXISTS idx_backups_session_op    ON backups(harness, session_id, op_id) WHERE op_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_backups_session_order ON backups(harness, session_id, order_blob DESC);
CREATE INDEX IF NOT EXISTS idx_backups_session_path_order ON backups(harness, session_id, path_hash, order_blob DESC);

CREATE TABLE IF NOT EXISTS harness_state (
  harness    TEXT NOT NULL,
  key        TEXT NOT NULL,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (harness, key)
);

CREATE TABLE IF NOT EXISTS host_state (
  key        TEXT NOT NULL PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

const MIGRATION_V2: &str = r#"
DELETE FROM compression_events
WHERE id NOT IN (
  SELECT MIN(id)
  FROM compression_events
  GROUP BY
    harness,
    COALESCE(session_id, char(0)),
    project_key,
    tool,
    COALESCE(task_id, char(0))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_compression_event_identity
ON compression_events (
  harness,
  COALESCE(session_id, char(0)),
  project_key,
  tool,
  COALESCE(task_id, char(0))
);
"#;

const MIGRATION_V3: &str = r#"
CREATE INDEX IF NOT EXISTS idx_bash_tasks_project_lookup
ON bash_tasks (harness, project_key, task_id, started_at DESC);
"#;

// V4 adds the restore_meta column to backups (Unix mode / created_dirs /
// link_target for DB-fallback restores when the meta.json sidecar is gone).
const MIGRATION_V4: &str = r#"
ALTER TABLE backups ADD COLUMN restore_meta TEXT;
"#;

// V5 persists async bash_notify / bash_watch pattern registrations so a
// bridge/daemon restart can re-arm watches and deliver gap matches.
const MIGRATION_V5: &str = r#"
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
CREATE INDEX IF NOT EXISTS idx_bash_pattern_watches_session
  ON bash_pattern_watches (harness, session_id);
CREATE INDEX IF NOT EXISTS idx_bash_pattern_watches_task
  ON bash_pattern_watches (harness, session_id, task_id);
"#;

// Removal-time health reads the existing task and backup tables. These indexes
// keep its seven-day aggregation and non-terminal task lookup off full history.
const MIGRATION_V6: &str = r#"
CREATE INDEX IF NOT EXISTS idx_bash_tasks_started_activity
  ON bash_tasks (started_at, project_key, harness, session_id);
CREATE INDEX IF NOT EXISTS idx_bash_tasks_non_terminal_pid
  ON bash_tasks (pid)
  WHERE status NOT IN ('completed', 'failed', 'killed', 'timed_out');
CREATE INDEX IF NOT EXISTS idx_backups_created_activity
  ON backups (created_at, project_key, harness, session_id);
"#;

// Standing roots are deliberately machine-scoped. Do not add harness, session,
// or daemon columns: daemon and daemonless CLI share one durable path pin.
const MIGRATION_V7: &str = r#"
CREATE TABLE IF NOT EXISTS standing_roots (
  literal_path           TEXT NOT NULL PRIMARY KEY,
  resolved_target        TEXT NOT NULL,
  resolved_git_toplevel  TEXT,
  scoped_relative_path   TEXT
);

CREATE TABLE IF NOT EXISTS standing_root_freshness (
  literal_path          TEXT NOT NULL,
  index_kind            TEXT NOT NULL CHECK (index_kind IN ('search', 'semantic', 'callgraph')),
  needs_strict_verify   INTEGER NOT NULL CHECK (needs_strict_verify IN (0, 1)),
  strict_verified_at    INTEGER,
  PRIMARY KEY (literal_path, index_kind),
  FOREIGN KEY (literal_path) REFERENCES standing_roots(literal_path) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_standing_root_freshness_needs_verify
  ON standing_root_freshness (needs_strict_verify, literal_path);
"#;

// Fate-unknown tasks are terminal but deliberately distinct from command failure:
// the daemon cannot reconstruct an exit result after finding the recorded process dead.
const MIGRATION_V8: &str = r#"
DROP INDEX IF EXISTS idx_bash_tasks_non_terminal_pid;
CREATE INDEX idx_bash_tasks_non_terminal_pid
  ON bash_tasks (pid)
  WHERE status NOT IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown');
"#;

#[derive(Debug)]
pub enum OpenError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    DowngradeRefused {
        db_version: u32,
        supported: u32,
    },
    MigrationFailed {
        from: u32,
        to: u32,
        error: rusqlite::Error,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Io(error) => write!(f, "database I/O error: {error}"),
            OpenError::Sqlite(error) => write!(f, "sqlite error: {error}"),
            OpenError::DowngradeRefused {
                db_version,
                supported,
            } => write!(
                f,
                "database schema version {db_version} is newer than supported version {supported}"
            ),
            OpenError::MigrationFailed { from, to, error } => {
                write!(f, "database migration {from}->{to} failed: {error}")
            }
        }
    }
}

impl std::error::Error for OpenError {}

impl From<std::io::Error> for OpenError {
    fn from(error: std::io::Error) -> Self {
        OpenError::Io(error)
    }
}

impl From<rusqlite::Error> for OpenError {
    fn from(error: rusqlite::Error) -> Self {
        OpenError::Sqlite(error)
    }
}

/// Open or create the AFT SQLite database at the given path.
///
/// Applies per-connection PRAGMAs, runs schema migrations from the DB's
/// current schema version up to [`CURRENT_SCHEMA_VERSION`], and returns the
/// configured connection.
pub fn open(path: &Path) -> Result<TrackedConnection, OpenError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut conn = TrackedConnection::open(path, SqliteStore::AftDb)?;
    apply_pragmas(&conn)?;
    run_migrations(&mut conn)?;
    Ok(conn)
}

/// Open an existing AFT database without creating, migrating, or mutating it.
///
/// Doctor uses this path for removal-time reporting, so checking state cannot
/// itself create an AFT database or race a running bridge's write transaction.
pub fn open_readonly(path: &Path) -> Result<TrackedConnection, OpenError> {
    let conn = TrackedConnection::open_path_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
        SqliteStore::AftDb,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// Apply the per-connection PRAGMAs required for every AFT SQLite connection.
pub fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Set the wait policy before WAL can acquire its journal lock. Otherwise a
    // concurrently opening daemon may fail immediately instead of honoring it.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Run forward-only migrations up to [`CURRENT_SCHEMA_VERSION`].
///
/// Returns the post-migration schema version. Refuses to open databases created
/// by newer AFT versions.
pub fn run_migrations(conn: &mut Connection) -> Result<u32, OpenError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL PRIMARY KEY);",
    )?;

    let db_version = current_schema_version(conn)?;
    if db_version > CURRENT_SCHEMA_VERSION {
        return Err(OpenError::DowngradeRefused {
            db_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }

    for version in (db_version + 1)..=CURRENT_SCHEMA_VERSION {
        apply_migration(conn, version)?;
    }

    Ok(current_schema_version(conn)?)
}

fn current_schema_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get::<_, u32>(0),
    )
}

fn apply_migration(conn: &mut Connection, version: u32) -> Result<(), OpenError> {
    let from = version - 1;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| OpenError::MigrationFailed {
            from,
            to: version,
            error,
        })?;

    let result = match version {
        1 => tx.execute_batch(MIGRATION_V1),
        2 => tx.execute_batch(MIGRATION_V2),
        3 => tx.execute_batch(MIGRATION_V3),
        4 => apply_migration_v4(&tx),
        5 => tx.execute_batch(MIGRATION_V5),
        6 => tx.execute_batch(MIGRATION_V6),
        7 => tx.execute_batch(MIGRATION_V7),
        8 => tx.execute_batch(MIGRATION_V8),
        _ => Ok(()),
    }
    .and_then(|()| {
        tx.execute("DELETE FROM schema_version", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_version (version) VALUES (?1)",
            [version],
        )?;
        tx.commit()
    });

    result.map_err(|error| OpenError::MigrationFailed {
        from,
        to: version,
        error,
    })
}

fn apply_migration_v4(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(backups)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if !columns.iter().any(|column| column == "restore_meta") {
        conn.execute_batch(MIGRATION_V4)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    const EXPECTED_TABLES: &[&str] = &[
        "schema_version",
        "bash_tasks",
        "bash_pattern_watches",
        "compression_events",
        "backups",
        "harness_state",
        "host_state",
        "standing_roots",
        "standing_root_freshness",
    ];

    const EXPECTED_INDEXES: &[&str] = &[
        "idx_bash_tasks_project_key",
        "idx_bash_tasks_status",
        "idx_bash_tasks_session_status",
        "idx_bash_tasks_project_lookup",
        "idx_bash_pattern_watches_session",
        "idx_bash_pattern_watches_task",
        "idx_bash_tasks_started_activity",
        "idx_bash_tasks_non_terminal_pid",
        "idx_backups_created_activity",
        "idx_compression_session",
        "idx_compression_session_created",
        "idx_compression_project_key",
        "idx_compression_event_identity",
        "idx_backups_session_path",
        "idx_backups_session_op",
        "idx_backups_session_order",
        "idx_backups_session_path_order",
        "idx_standing_root_freshness_needs_verify",
    ];

    #[test]
    fn open_fresh_db_creates_all_tables() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        let tables = sqlite_names(&conn, "table");
        for table in EXPECTED_TABLES {
            assert!(tables.contains(&table.to_string()), "missing table {table}");
        }
    }

    #[test]
    fn open_fresh_db_creates_all_indexes() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        let indexes = sqlite_names(&conn, "index");
        for index in EXPECTED_INDEXES {
            assert!(
                indexes.contains(&index.to_string()),
                "missing index {index}"
            );
        }
    }

    #[test]
    fn open_existing_db_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");

        let conn = open(&path).unwrap();
        let first_version = schema_version(&conn);
        drop(conn);

        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn), first_version);
    }

    #[test]
    fn pragmas_applied_correctly() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        let synchronous: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();

        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 5000);
        assert_eq!(synchronous, 1);
    }

    #[test]
    fn downgrade_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = open(&path).unwrap();
        conn.execute("INSERT OR REPLACE INTO schema_version VALUES (999)", [])
            .unwrap();
        drop(conn);

        match open(&path).unwrap_err() {
            OpenError::DowngradeRefused {
                db_version,
                supported,
            } => {
                assert_eq!(db_version, 999);
                assert_eq!(supported, CURRENT_SCHEMA_VERSION);
            }
            error => panic!("expected downgrade refusal, got {error:?}"),
        }
    }

    #[test]
    fn migration_runner_advances_version() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migration_v2_deduplicates_compression_events_and_adds_unique_index() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");

        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        insert_compression_event(
            &conn,
            1,
            "opencode",
            Some("session-1"),
            "project-key",
            "bash",
            Some("task-1"),
        )
        .unwrap();
        insert_compression_event(
            &conn,
            2,
            "opencode",
            Some("session-1"),
            "project-key",
            "bash",
            Some("task-1"),
        )
        .unwrap();
        insert_compression_event(&conn, 3, "opencode", None, "project-key", "bash", None).unwrap();
        drop(conn);

        let conn = open(&path).unwrap();

        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        let ids = compression_event_ids(&conn);
        assert_eq!(ids, vec![1, 3]);
        let indexes = sqlite_names(&conn, "index");
        assert!(
            indexes.contains(&"idx_compression_event_identity".to_string()),
            "missing v2 unique compression event identity index"
        );
        assert_unique_constraint(insert_compression_event(
            &conn,
            4,
            "opencode",
            Some("session-1"),
            "project-key",
            "bash",
            Some("task-1"),
        ));
    }

    #[test]
    fn migration_v3_upgrades_existing_v2_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (2)", [])
            .unwrap();
        drop(conn);

        let conn = open(&path).unwrap();

        // A v2 database migrates all the way to the current version; V3 creates
        // the bash-task lookup index on the way.
        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        assert!(sqlite_names(&conn, "index").contains(&"idx_bash_tasks_project_lookup".to_string()));
    }

    #[test]
    fn bash_task_project_lookup_uses_composite_filter_and_order_index() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();
        let mut statement = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT harness, session_id, task_id, project_key, command, cwd, status,
                        exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                        compressed, timeout_ms, completion_delivered, output_bytes, metadata
                 FROM bash_tasks
                 WHERE harness = ?1 AND project_key = ?2 AND task_id = ?3
                 ORDER BY started_at DESC
                 LIMIT 1",
            )
            .unwrap();
        let plan = statement
            .query_map(params!["opencode", "project-key", "bash-task"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            plan.iter()
                .any(|detail| detail.contains("idx_bash_tasks_project_lookup")),
            "lookup plan did not use the composite index: {plan:?}"
        );
        assert!(
            plan.iter()
                .all(|detail| !detail.contains("USE TEMP B-TREE FOR ORDER BY")),
            "lookup plan still sorts into a temporary B-tree: {plan:?}"
        );
    }

    #[test]
    fn migration_v4_adds_restore_metadata_to_v2_and_v3_databases() {
        for initial_version in [2, 3] {
            let dir = tempdir().unwrap();
            let path = dir.path().join(format!("aft-v{initial_version}.db"));
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATION_V1).unwrap();
            conn.execute_batch(MIGRATION_V2).unwrap();
            conn.execute("DELETE FROM schema_version", []).unwrap();
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [initial_version],
            )
            .unwrap();
            insert_backup(&conn, "legacy", &order_blob(1)).unwrap();
            drop(conn);

            let conn = open(&path).unwrap();

            assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
            assert!(table_columns(&conn, "backups").contains(&"restore_meta".to_string()));
            let restore_meta: Option<String> = conn
                .query_row(
                    "SELECT restore_meta FROM backups WHERE backup_id = 'legacy'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(restore_meta, None, "legacy rows stay nullable");
        }
    }

    #[test]
    fn migration_v4_is_idempotent_when_column_already_exists() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch(MIGRATION_V4).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (3)", [])
            .unwrap();
        drop(conn);

        let conn = open(&path).unwrap();

        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(
            table_columns(&conn, "backups")
                .iter()
                .filter(|column| column.as_str() == "restore_meta")
                .count(),
            1
        );
    }

    #[test]
    fn migration_v5_adds_bash_pattern_watches_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch(MIGRATION_V3).unwrap();
        conn.execute_batch(MIGRATION_V4).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (4)", [])
            .unwrap();
        drop(conn);

        let conn = open(&path).unwrap();

        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        assert!(sqlite_names(&conn, "table").contains(&"bash_pattern_watches".to_string()));
        assert!(sqlite_names(&conn, "index").contains(&"idx_bash_pattern_watches_task".to_string()));
    }

    #[test]
    fn migration_v7_adds_machine_scoped_standing_root_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch(MIGRATION_V3).unwrap();
        conn.execute_batch(MIGRATION_V4).unwrap();
        conn.execute_batch(MIGRATION_V5).unwrap();
        conn.execute_batch(MIGRATION_V6).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (6)", [])
            .unwrap();
        drop(conn);

        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        assert!(sqlite_names(&conn, "table").contains(&"standing_roots".to_string()));
        assert!(sqlite_names(&conn, "table").contains(&"standing_root_freshness".to_string()));
        assert!(sqlite_names(&conn, "index")
            .contains(&"idx_standing_root_freshness_needs_verify".to_string()));
    }

    #[test]
    fn migration_v6_adds_removal_health_indexes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch(MIGRATION_V3).unwrap();
        conn.execute_batch(MIGRATION_V4).unwrap();
        conn.execute_batch(MIGRATION_V5).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (5)", [])
            .unwrap();
        drop(conn);

        let conn = open(&path).unwrap();

        let indexes = sqlite_names(&conn, "index");
        for index in [
            "idx_bash_tasks_started_activity",
            "idx_bash_tasks_non_terminal_pid",
            "idx_backups_created_activity",
        ] {
            assert!(
                indexes.contains(&index.to_string()),
                "missing v6 index {index}"
            );
        }
    }

    #[test]
    fn open_readonly_does_not_create_a_missing_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing-aft.db");

        assert!(open_readonly(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn migration_runner_no_op_when_current() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");

        let conn = open(&path).unwrap();
        assert_eq!(schema_version_row_count(&conn), 1);
        drop(conn);

        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(schema_version_row_count(&conn), 1);
    }

    #[test]
    fn harness_state_compound_pk_works() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        conn.execute(
            "INSERT INTO harness_state (harness, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params!["opencode", "warned_tools", "{}", 1_i64],
        )
        .unwrap();
        let duplicate = conn.execute(
            "INSERT INTO harness_state (harness, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params!["opencode", "warned_tools", "{}", 2_i64],
        );
        assert_unique_constraint(duplicate);

        conn.execute(
            "INSERT INTO harness_state (harness, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params!["pi", "warned_tools", "{}", 3_i64],
        )
        .unwrap();
    }

    #[test]
    fn host_state_simple_pk_works() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        conn.execute(
            "INSERT INTO host_state (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params!["trusted_filter_projects", "[]", 1_i64],
        )
        .unwrap();
        let duplicate = conn.execute(
            "INSERT INTO host_state (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params!["trusted_filter_projects", "[]", 2_i64],
        );
        assert_unique_constraint(duplicate);
    }

    #[test]
    fn bash_tasks_compound_pk_works() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        insert_bash_task(&conn, "opencode", "session-1", "bash-12345678").unwrap();
        let duplicate = insert_bash_task(&conn, "opencode", "session-1", "bash-12345678");
        assert_unique_constraint(duplicate);

        insert_bash_task(&conn, "pi", "session-1", "bash-12345678").unwrap();
    }

    #[test]
    fn backups_order_blob_sort() {
        let dir = tempdir().unwrap();
        let conn = open(&dir.path().join("aft.db")).unwrap();

        let one = order_blob(1);
        let two = order_blob(2);
        let max = [0xFF; 16];

        insert_backup(&conn, "one", &one).unwrap();
        insert_backup(&conn, "two", &two).unwrap();
        insert_backup(&conn, "max", &max).unwrap();

        assert_eq!(backup_ids_ordered(&conn, "ASC"), vec!["one", "two", "max"]);
        assert_eq!(backup_ids_ordered(&conn, "DESC"), vec!["max", "two", "one"]);
    }

    fn sqlite_names(conn: &Connection, kind: &str) -> Vec<String> {
        let sql = match kind {
            "table" => "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            "index" => "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            _ => panic!("unsupported sqlite_master kind: {kind}"),
        };
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn schema_version(conn: &Connection) -> u32 {
        conn.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap()
    }

    fn schema_version_row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap()
    }

    fn assert_unique_constraint(result: rusqlite::Result<usize>) {
        let error = result.expect_err("expected a unique constraint violation");
        assert!(
            error.to_string().contains("UNIQUE constraint failed"),
            "expected UNIQUE constraint failure, got {error}"
        );
    }

    fn insert_bash_task(
        conn: &Connection,
        harness: &str,
        session_id: &str,
        task_id: &str,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO bash_tasks (
                harness, session_id, task_id, project_key, command, cwd, status, started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                harness,
                session_id,
                task_id,
                "project-key",
                "echo ok",
                "/tmp",
                "running",
                1_i64
            ],
        )
    }

    fn insert_compression_event(
        conn: &Connection,
        id: i64,
        harness: &str,
        session_id: Option<&str>,
        project_key: &str,
        tool: &str,
        task_id: Option<&str>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO compression_events (
                id, harness, session_id, project_key, tool, task_id, command, compressor,
                original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                harness,
                session_id,
                project_key,
                tool,
                task_id,
                "echo ok",
                "test-compressor",
                100_i64,
                50_i64,
                20_i64,
                10_i64,
                id
            ],
        )
    }

    fn compression_event_ids(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT id FROM compression_events ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn insert_backup(
        conn: &Connection,
        backup_id: &str,
        order_blob: &[u8],
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO backups (
                backup_id, harness, session_id, project_key, order_blob, file_path,
                path_hash, kind, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                backup_id,
                "opencode",
                "session-1",
                "project-key",
                order_blob,
                "/tmp/file.txt",
                "path-hash",
                "content",
                1_i64
            ],
        )
    }

    fn order_blob(value: u128) -> [u8; 16] {
        value.to_be_bytes()
    }

    fn backup_ids_ordered(conn: &Connection, direction: &str) -> Vec<String> {
        let sql = match direction {
            "ASC" => "SELECT backup_id FROM backups ORDER BY order_blob ASC",
            "DESC" => "SELECT backup_id FROM backups ORDER BY order_blob DESC",
            _ => panic!("unsupported order direction: {direction}"),
        };
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }
}
