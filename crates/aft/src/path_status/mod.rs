//! Per-view status annotations for paths that could not join a complete generation.
//!
//! The path table contains only pending or failed annotations. Removing an
//! annotation means assembly no longer reports a problem for that path; manifest
//! membership remains the authority for whether the path is in a generation.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

/// Maximum paths included in a refresh-status response; counts include paths beyond this limit.
pub const VISIBLE_PATH_CAP: usize = 20;

const PATH_STATUS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS path_status (
    rel_path BLOB NOT NULL PRIMARY KEY,
    state TEXT NOT NULL CHECK(state IN ('pending', 'failed')),
    reason TEXT NOT NULL,
    since_generation INTEGER NOT NULL CHECK(since_generation >= 0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS maintenance_outcomes (
    operation TEXT NOT NULL PRIMARY KEY,
    outcome TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0)
) WITHOUT ROWID;
"#;

/// The two path problem states exposed by view status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathState {
    Pending,
    Failed,
}

impl PathState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One durable annotation from the per-view `path_status` derived table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathStatus {
    pub rel_path: Vec<u8>,
    pub state: PathState,
    pub reason: String,
    pub since_generation: u64,
}

/// Summary data for refresh-status responses: total counts plus a bounded,
/// bytewise-ordered path list. This is internal response data, not a tool schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathStatusSummary {
    pub pending_count: usize,
    pub failed_count: usize,
    pub paths: Vec<PathStatus>,
}

#[derive(Debug)]
pub enum PathStatusError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    InvalidState(String),
    InvalidGeneration(i64),
    GenerationOutOfRange(u64),
}

impl fmt::Display for PathStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "path-status I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "path-status SQLite error: {error}"),
            Self::InvalidState(state) => write!(f, "invalid path-status state `{state}`"),
            Self::InvalidGeneration(generation) => {
                write!(f, "invalid negative path-status generation {generation}")
            }
            Self::GenerationOutOfRange(generation) => {
                write!(
                    f,
                    "path-status generation {generation} exceeds SQLite INTEGER"
                )
            }
        }
    }
}

impl Error for PathStatusError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::InvalidState(_) | Self::InvalidGeneration(_) | Self::GenerationOutOfRange(_) => {
                None
            }
        }
    }
}

impl From<std::io::Error> for PathStatusError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for PathStatusError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Owns the path-status table in one view's derived SQLite database.
#[derive(Debug)]
pub struct PathStatusStore {
    path: PathBuf,
    connection: Connection,
}

impl PathStatusStore {
    /// Opens the conventional derived-state database for one view.
    pub fn open(view_dir: &Path) -> Result<Self, PathStatusError> {
        Self::open_at(&view_dir.join("derived.sqlite"))
    }

    /// Opens a view-derived database at an explicit path. This allows the view
    /// assembler to share its already-created derived database with this table.
    pub fn open_at(path: &Path) -> Result<Self, PathStatusError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(PATH_STATUS_SCHEMA)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Marks a path pending. Repeated pending reports preserve the generation
    /// where the annotation began, so status consumers can identify its age.
    pub fn mark_pending(
        &mut self,
        rel_path: &[u8],
        reason: impl Into<String>,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        self.upsert(
            rel_path,
            PathState::Pending,
            reason.into(),
            since_generation,
        )
    }

    /// Marks a path failed. A state transition starts a new annotation age.
    pub fn mark_failed(
        &mut self,
        rel_path: &[u8],
        reason: impl Into<String>,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        self.upsert(rel_path, PathState::Failed, reason.into(), since_generation)
    }

    /// Removes an annotation after the path joined a complete generation.
    pub fn clear(&mut self, rel_path: &[u8]) -> Result<(), PathStatusError> {
        self.connection.execute(
            "DELETE FROM path_status WHERE rel_path = ?1",
            params![rel_path],
        )?;
        Ok(())
    }

    pub fn status_for(&self, rel_path: &[u8]) -> Result<Option<PathStatus>, PathStatusError> {
        let row = self
            .connection
            .query_row(
                "SELECT rel_path, state, reason, since_generation
                 FROM path_status WHERE rel_path = ?1",
                params![rel_path],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(Self::decode_row).transpose()
    }

    /// Returns counts for both states and at most twenty bytewise-ordered paths.
    pub fn record_maintenance_outcome(
        &mut self,
        operation: &str,
        outcome: &str,
        generation: u64,
    ) -> Result<(), PathStatusError> {
        self.connection.execute(
            "INSERT INTO maintenance_outcomes(operation, outcome, generation)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(operation) DO NOTHING",
            params![operation, outcome, generation],
        )?;
        Ok(())
    }

    pub fn maintenance_outcome(
        &self,
        operation: &str,
    ) -> Result<Option<(String, u64)>, PathStatusError> {
        self.connection
            .query_row(
                "SELECT outcome, generation FROM maintenance_outcomes WHERE operation = ?1",
                [operation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(PathStatusError::from)
    }

    pub fn summary(&self) -> Result<PathStatusSummary, PathStatusError> {
        let (pending_count, failed_count) = self.connection.query_row(
            "SELECT
                 COALESCE(SUM(CASE WHEN state = 'pending' THEN 1 ELSE 0 END), 0),
                 COALESCE(SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END), 0)
             FROM path_status",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let pending_count = usize::try_from(pending_count)
            .map_err(|_| PathStatusError::InvalidGeneration(pending_count))?;
        let failed_count = usize::try_from(failed_count)
            .map_err(|_| PathStatusError::InvalidGeneration(failed_count))?;

        let mut statement = self.connection.prepare(
            "SELECT rel_path, state, reason, since_generation
             FROM path_status
             ORDER BY rel_path
             LIMIT ?1",
        )?;
        let paths = statement
            .query_map(params![VISIBLE_PATH_CAP as i64], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(Self::decode_row)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PathStatusSummary {
            pending_count,
            failed_count,
            paths,
        })
    }

    fn upsert(
        &mut self,
        rel_path: &[u8],
        state: PathState,
        reason: String,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        let generation = i64::try_from(since_generation)
            .map_err(|_| PathStatusError::GenerationOutOfRange(since_generation))?;
        self.connection.execute(
            "INSERT INTO path_status (rel_path, state, reason, since_generation)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(rel_path) DO UPDATE SET
                 state = excluded.state,
                 reason = excluded.reason,
                 since_generation = CASE
                     WHEN path_status.state = excluded.state THEN path_status.since_generation
                     ELSE excluded.since_generation
                 END",
            params![rel_path, state.as_str(), reason, generation],
        )?;
        Ok(())
    }

    fn decode_row(
        (rel_path, state, reason, since_generation): (Vec<u8>, String, String, i64),
    ) -> Result<PathStatus, PathStatusError> {
        Ok(PathStatus {
            rel_path,
            state: PathState::parse(&state).ok_or(PathStatusError::InvalidState(state))?,
            reason,
            since_generation: u64::try_from(since_generation)
                .map_err(|_| PathStatusError::InvalidGeneration(since_generation))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_all_rows_but_caps_and_orders_visible_paths_by_bytes() {
        let dir = tempfile::tempdir().expect("create view dir");
        let mut store = PathStatusStore::open(dir.path()).expect("open path-status store");
        assert_eq!(
            store.summary().expect("summarize empty statuses"),
            PathStatusSummary {
                pending_count: 0,
                failed_count: 0,
                paths: Vec::new(),
            }
        );
        for index in 0..25 {
            store
                .mark_pending(format!("z{index:02}").as_bytes(), "read changed", 7)
                .expect("mark pending");
        }
        store
            .mark_failed(b"\x80binary", "quarantined", 8)
            .expect("mark failed");
        store
            .mark_failed(b"a/path", "quota", 9)
            .expect("mark failed");

        let summary = store.summary().expect("summarize statuses");
        assert_eq!(summary.pending_count, 25);
        assert_eq!(summary.failed_count, 2);
        assert_eq!(summary.paths.len(), VISIBLE_PATH_CAP);
        assert_eq!(summary.paths[0].rel_path, b"a/path");
        assert_eq!(summary.paths[1].rel_path, b"z00");
        assert!(summary
            .paths
            .windows(2)
            .all(|paths| paths[0].rel_path <= paths[1].rel_path));
    }

    #[test]
    fn repeated_state_preserves_annotation_age_and_success_clears_it() {
        let dir = tempfile::tempdir().expect("create view dir");
        let mut store = PathStatusStore::open(dir.path()).expect("open path-status store");

        store
            .mark_pending(b"src/lib.rs", "read changed", 3)
            .expect("mark pending");
        store
            .mark_pending(b"src/lib.rs", "still changing", 9)
            .expect("repeat pending");
        assert_eq!(
            store
                .status_for(b"src/lib.rs")
                .expect("read status")
                .expect("pending row")
                .since_generation,
            3
        );

        store
            .mark_failed(b"src/lib.rs", "quarantined", 10)
            .expect("mark failed");
        assert_eq!(
            store
                .status_for(b"src/lib.rs")
                .expect("read status")
                .expect("failed row")
                .since_generation,
            10
        );
        store.clear(b"src/lib.rs").expect("clear completed path");
        assert!(store
            .status_for(b"src/lib.rs")
            .expect("read status")
            .is_none());
    }
}
