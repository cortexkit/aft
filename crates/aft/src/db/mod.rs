use rusqlite::{Connection, TransactionBehavior};
use std::fmt;
use std::fs;
use std::path::Path;

pub mod backups;
pub mod bash_tasks;
pub mod compression_events;
pub mod state;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

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
pub fn open(path: &Path) -> Result<Connection, OpenError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    run_migrations(&mut conn)?;
    Ok(conn)
}

/// Apply the per-connection PRAGMAs required for every AFT SQLite connection.
pub fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    const EXPECTED_TABLES: &[&str] = &[
        "schema_version",
        "bash_tasks",
        "compression_events",
        "backups",
        "harness_state",
        "host_state",
    ];

    const EXPECTED_INDEXES: &[&str] = &[
        "idx_bash_tasks_project_key",
        "idx_bash_tasks_status",
        "idx_bash_tasks_session_status",
        "idx_compression_session",
        "idx_compression_session_created",
        "idx_compression_project_key",
        "idx_backups_session_path",
        "idx_backups_session_op",
        "idx_backups_session_order",
        "idx_backups_session_path_order",
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
