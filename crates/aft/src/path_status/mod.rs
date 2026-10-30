//! Per-view status annotations for paths that could not join a complete generation.
//!
//! The path table contains only pending or failed annotations. Removing an
//! annotation means assembly no longer reports a problem for that path; manifest
//! membership remains the authority for whether the path is in a generation.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, TransactionBehavior};

pub(crate) mod health;

#[cfg(test)]
thread_local! {
    pub(crate) static HEALTH_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static STORE_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static HEALTH_SUMMARY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Maximum paths included in a refresh-status response; counts include paths beyond this limit.
pub const VISIBLE_PATH_CAP: usize = 20;

/// Maximum skipped paths named in a status summary; the per-reason counts
/// include paths beyond this limit.
pub const SKIPPED_PATH_SAMPLE_CAP: usize = 5;

/// Why a path ended without one of its derived planes, or outside the
/// generation, for a reason no retry would change.
///
/// A skip is a terminal state, not a problem state: the view is complete, and
/// no later work will change the outcome until the path's content or Git mode
/// changes. It is recorded so status can say which paths were left out
/// instead of hiding them. Skips live in their own table and never count as
/// pending or failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SkipReason {
    /// The file is in a call-graph language but is not valid UTF-8, so the
    /// parser cannot read it. It is published with an empty call graph; the
    /// call graph built without views also leaves such files out. The
    /// semantic index cannot read it either, so no semantic vectors wait for
    /// it.
    NonUtf8Source,
    /// HEAD lists the path with a Git mode that is neither a regular file, a
    /// symlink nor a submodule, which view assembly does not know how to
    /// publish. The path is left out of the generation.
    UnsupportedGitMode,
    /// The file is larger than the semantic index reads
    /// (`semantic_index::MAX_SEMANTIC_FILE_BYTES`), so it never has semantic
    /// vectors. It is published without them.
    SemanticFileTooLarge,
    /// The project walk the semantic index uses leaves the file out (a
    /// skipped directory such as `dist/` or `node_modules/`, or a
    /// `.gitignore`/`.aftignore` rule), although Git tracks it. It is
    /// published without semantic vectors.
    ExcludedFromSemanticIndex,
}

impl SkipReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NonUtf8Source => "non_utf8_source",
            Self::UnsupportedGitMode => "unsupported_git_mode",
            Self::SemanticFileTooLarge => "semantic_file_too_large",
            Self::ExcludedFromSemanticIndex => "excluded_from_semantic_index",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::NonUtf8Source,
            Self::UnsupportedGitMode,
            Self::SemanticFileTooLarge,
            Self::ExcludedFromSemanticIndex,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == value)
    }

    /// Whether a member skipped for this reason never gets semantic vectors,
    /// so it is not waiting for them.
    pub const fn excludes_semantic(self) -> bool {
        matches!(
            self,
            Self::NonUtf8Source | Self::SemanticFileTooLarge | Self::ExcludedFromSemanticIndex
        )
    }
}

/// One skipped path named in a status summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSkip {
    pub rel_path: Vec<u8>,
    pub reason: String,
}

/// What a complete view publication says the status ledger should hold once
/// its generation is the one served.
#[derive(Clone, Debug, Default)]
pub(crate) struct StatusIntent {
    /// The generation the intent describes; new skip rows start at it.
    pub generation: u64,
    /// Every path the generation accounts for: its members, and HEAD paths
    /// it leaves out with a recorded skip (an unsupported Git mode). Skips of
    /// any other path are dropped.
    pub members: std::collections::HashSet<Vec<u8>>,
    /// Paths whose semantic work is still outstanding; they stay pending.
    pub keep_pending: std::collections::BTreeSet<Vec<u8>>,
    /// The ledger's mark epoch (`PathStatusStore::mark_epoch`) when the
    /// publication's preparation started reading the checkout. An annotation
    /// marked later belongs to work this publication does not cover, such as
    /// a newer preparation that marked pending while this settlement waited
    /// to be applied, so the settlement leaves it alone.
    pub marked_through: u64,
    /// Paths to mark failed, with the failure: members that cannot be read
    /// for their semantic vectors, and paths left out of the generation
    /// because a missing payload could not be rebuilt.
    pub failed: std::collections::BTreeMap<Vec<u8>, String>,
    /// Skip outcomes for members read afresh: `Some` records a skip, `None`
    /// removes one. Members reused unread keep their rows, since their
    /// content, and so their outcome, did not change.
    pub fresh_skips: std::collections::BTreeMap<Vec<u8>, Option<SkipReason>>,
}

/// The ledger rows a settlement writes: annotations to delete (with the state
/// they were read in), annotations to mark failed, and skips to record
/// (`Some`) or delete (`None`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct StatusChanges {
    pub clear: Vec<(Vec<u8>, PathState)>,
    pub fail: Vec<(Vec<u8>, String)>,
    pub skips: Vec<(Vec<u8>, Option<SkipReason>)>,
}

impl StatusChanges {
    pub(crate) fn is_empty(&self) -> bool {
        self.clear.is_empty() && self.fail.is_empty() && self.skips.is_empty()
    }
}

/// Compares the ledger with `intent`. It reads every annotation and skip
/// row, which a settled view keeps few of, rather than probing each member.
fn plan_settlement(
    connection: &rusqlite::Connection,
    intent: &StatusIntent,
) -> Result<StatusChanges, PathStatusError> {
    let mut changes = StatusChanges::default();
    // An annotation written before marks were stamped has no mark; it is
    // older than any settlement that reads it.
    let mut annotations = connection
        .prepare_cached("SELECT rel_path, state, reason, COALESCE(mark, 0) FROM path_status")?;
    let mut failed = intent.failed.clone();
    for row in annotations.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })? {
        let (path, state, reason, mark) = row?;
        let state = PathState::parse(&state).ok_or(PathStatusError::InvalidState(state))?;
        if u64::try_from(mark).unwrap_or(0) > intent.marked_through {
            // Marked by later work, which owns the row now: neither clear
            // it nor mark it failed.
            failed.remove(&path);
            continue;
        }
        if let Some(failure) = failed.get(&path) {
            // Already failed for the same reason: nothing to write.
            if state == PathState::Failed && *failure == reason {
                failed.remove(&path);
            }
            continue;
        }
        let joined = !intent.keep_pending.contains(&path);
        let settled = match state {
            PathState::Pending => joined,
            PathState::Failed => joined && intent.members.contains(&path),
        };
        if settled {
            changes.clear.push((path, state));
        }
    }
    changes.fail = failed.into_iter().collect();
    let mut recorded = std::collections::BTreeMap::new();
    let mut skips = connection.prepare_cached("SELECT rel_path, reason FROM path_skips")?;
    for row in skips.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    })? {
        let (path, reason) = row?;
        recorded.insert(path, reason);
    }
    for (path, reason) in &recorded {
        let outcome = intent.fresh_skips.get(path);
        if !intent.members.contains(path) || outcome == Some(&None) {
            changes.skips.push((path.clone(), None));
        } else if let Some(Some(fresh)) = outcome {
            if fresh.as_str() != reason {
                changes.skips.push((path.clone(), Some(*fresh)));
            }
        }
    }
    for (path, outcome) in &intent.fresh_skips {
        if let Some(reason) = outcome {
            if !recorded.contains_key(path) && intent.members.contains(path) {
                changes.skips.push((path.clone(), Some(*reason)));
            }
        }
    }
    Ok(changes)
}

const PATH_STATUS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS path_status (
    rel_path BLOB NOT NULL PRIMARY KEY,
    state TEXT NOT NULL CHECK(state IN ('pending', 'failed')),
    reason TEXT NOT NULL,
    since_generation INTEGER NOT NULL CHECK(since_generation >= 0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS path_skips (
    rel_path BLOB NOT NULL PRIMARY KEY,
    reason TEXT NOT NULL,
    since_generation INTEGER NOT NULL CHECK(since_generation >= 0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS path_status_marks (
    id INTEGER NOT NULL PRIMARY KEY CHECK(id = 0),
    epoch INTEGER NOT NULL CHECK(epoch >= 0)
);
CREATE TABLE IF NOT EXISTS maintenance_outcomes (
    operation TEXT NOT NULL PRIMARY KEY,
    outcome TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0)
) WITHOUT ROWID;
"#;

/// Stamps each annotation with the mark epoch of the write that last marked
/// it (`?5`), so a settlement can tell its own work from later marks.
const UPSERT_PATH_STATUS: &str =
    "INSERT INTO path_status (rel_path, state, reason, since_generation, mark)
    VALUES (?1, ?2, ?3, ?4, ?5)
    ON CONFLICT(rel_path) DO UPDATE SET
        state = excluded.state,
        reason = excluded.reason,
        since_generation = CASE
            WHEN path_status.state = excluded.state THEN path_status.since_generation
            ELSE excluded.since_generation
        END,
        mark = excluded.mark";

/// Advances the ledger's mark epoch inside `transaction` and returns the new
/// value. Every write that marks annotations takes one, so marks only grow.
fn next_mark(transaction: &rusqlite::Transaction<'_>) -> Result<i64, PathStatusError> {
    transaction.execute(
        "INSERT INTO path_status_marks (id, epoch) VALUES (0, 1)
         ON CONFLICT(id) DO UPDATE SET epoch = epoch + 1",
        [],
    )?;
    Ok(transaction.query_row(
        "SELECT epoch FROM path_status_marks WHERE id = 0",
        [],
        |row| row.get(0),
    )?)
}

/// Adds the `mark` column to a ledger created before annotations were
/// stamped. Its existing rows keep no mark, which reads as older than any
/// settlement.
fn add_mark_column(connection: &mut rusqlite::Connection) -> Result<(), PathStatusError> {
    // Check without a write lock first: every open runs this, including the
    // status reader, and an up-to-date ledger must not wait behind a large
    // marking or settlement write. Only a ledger that still needs the column
    // takes the write lock, and it checks again under it.
    if mark_column_exists(connection)? {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !mark_column_exists(&transaction)? {
        transaction.execute_batch("ALTER TABLE path_status ADD COLUMN mark INTEGER")?;
    }
    transaction.commit()?;
    Ok(())
}

fn mark_column_exists(connection: &rusqlite::Connection) -> Result<bool, PathStatusError> {
    Ok(connection
        .prepare("SELECT 1 FROM pragma_table_info('path_status') WHERE name = 'mark'")?
        .exists([])?)
}

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
    /// Skipped paths by reason. Skips are not counted as pending or failed.
    pub skipped_by_reason: std::collections::BTreeMap<String, usize>,
    /// At most `SKIPPED_PATH_SAMPLE_CAP` skipped paths, bytewise ordered.
    pub skipped_sample: Vec<PathSkip>,
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
    connection: crate::db::file_identity::IdentityConnection,
    health: std::sync::Weak<health::Health>,
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
            crate::private_storage::open_dir(parent, parent)?;
        }
        let connection = crate::db::file_identity::IdentityConnection::open(
            path,
            "path_status::PathStatusStore::open_at",
        )?;
        #[cfg(test)]
        STORE_OPENS.with(|opens| opens.set(opens.get() + 1));
        // A last-close checkpoint changes the health signature without a
        // ledger write. Leave checkpointing to the normal WAL threshold so
        // cached local projections never need to guess who changed the files.
        connection.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        connection.busy_timeout(crate::db::STEADY_BUSY_TIMEOUT)?;
        // This is a long-lived annotation store. A publication can update one
        // row per source file, so rollback journaling creates and unlinks a
        // journal for every annotation instead of appending to one WAL.
        crate::blob_store::retry_while_busy(crate::db::STEADY_BUSY_TIMEOUT, || {
            connection.pragma_update(None, "journal_mode", "WAL")
        })?;
        connection.pragma_update(
            None,
            "journal_size_limit",
            crate::db::lifecycle::WAL_JOURNAL_SIZE_LIMIT,
        )?;
        connection.execute_batch(PATH_STATUS_SCHEMA)?;
        let mut store = Self {
            path: path.to_path_buf(),
            connection,
            health: std::sync::Arc::downgrade(&health::Health::for_path(path)),
        };
        add_mark_column(&mut store.connection)?;
        Ok(store)
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

    /// Marks a pending set in one IMMEDIATE transaction with the same age rule
    /// as `mark_pending`. Readers see all rows or none; an error or unwinding
    /// cancellation rolls back the entire set, without changing durability.
    /// Reasons may differ per path, and repeated paths are applied in order.
    pub fn mark_pending_many<'a>(
        &mut self,
        rows: impl IntoIterator<Item = (&'a [u8], &'a str)>,
        since_generation: u64,
    ) -> Result<(), PathStatusError> {
        let generation = i64::try_from(since_generation)
            .map_err(|_| PathStatusError::GenerationOutOfRange(since_generation))?;
        let mut rows = rows.into_iter().peekable();
        if rows.peek().is_none() {
            return Ok(());
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mark = next_mark(&transaction)?;
        {
            let mut upsert = transaction.prepare_cached(UPSERT_PATH_STATUS)?;
            for (path, reason) in rows {
                upsert.execute(params![
                    path,
                    PathState::Pending.as_str(),
                    reason,
                    generation,
                    mark
                ])?;
            }
        }
        transaction.commit()?;
        self.invalidate_health();
        Ok(())
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
        let changed = self.connection.execute(
            "DELETE FROM path_status WHERE rel_path = ?1",
            params![rel_path],
        )?;
        if changed > 0 {
            self.invalidate_health();
        }
        Ok(())
    }

    /// Deletes the given paths' annotations in one transaction. Publications
    /// use `settle`; tests use this to clear rows directly.
    #[cfg(test)]
    pub(crate) fn clear_many<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<(), PathStatusError> {
        let transaction = self.connection.transaction()?;
        let mut changed = 0;
        {
            let mut delete =
                transaction.prepare_cached("DELETE FROM path_status WHERE rel_path = ?1")?;
            for path in paths {
                changed += delete.execute(params![path])?;
            }
        }
        transaction.commit()?;
        if changed > 0 {
            self.invalidate_health();
        }
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
        #[cfg(test)]
        HEALTH_SUMMARY_READS.with(|reads| reads.set(reads.get() + 1));
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

        let mut by_reason = self
            .connection
            .prepare("SELECT reason, COUNT(*) FROM path_skips GROUP BY reason ORDER BY reason")?;
        let skipped_by_reason = by_reason
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .map(|row| {
                let (reason, count) = row?;
                let count = usize::try_from(count)
                    .map_err(|_| PathStatusError::InvalidGeneration(count))?;
                Ok((reason, count))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, PathStatusError>>()?;
        let mut sample = self
            .connection
            .prepare("SELECT rel_path, reason FROM path_skips ORDER BY rel_path LIMIT ?1")?;
        let skipped_sample = sample
            .query_map(params![SKIPPED_PATH_SAMPLE_CAP as i64], |row| {
                Ok(PathSkip {
                    rel_path: row.get(0)?,
                    reason: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PathStatusSummary {
            pending_count,
            failed_count,
            paths,
            skipped_by_reason,
            skipped_sample,
        })
    }

    /// All health fields describe one SQLite read snapshot, even if another
    /// process commits between the summary's individual queries.
    fn health_summary(&self) -> Result<PathStatusSummary, PathStatusError> {
        let transaction = self.connection.unchecked_transaction()?;
        let summary = self.summary()?;
        transaction.commit()?;
        Ok(summary)
    }

    /// The rows `intent` would change, read without writing. Empty when the
    /// ledger already says what `intent` says.
    pub(crate) fn plan_settlement(
        &self,
        intent: &StatusIntent,
    ) -> Result<StatusChanges, PathStatusError> {
        plan_settlement(&self.connection, intent)
    }

    /// Brings the ledger in line with `intent` in one IMMEDIATE transaction,
    /// writing only the rows that differ, and returns how many it wrote.
    ///
    /// Pending rows clear unless their path still has outstanding semantic
    /// work: a member has joined the generation, and a path that is not a
    /// member (deleted from the checkout, excluded by the project content
    /// rules, no longer in HEAD) is never revisited by a later publication,
    /// so a row left behind for it would stay pending for the life of the
    /// view. Failed rows clear only for members that joined. Skips follow the
    /// fresh outcomes, and skips of paths that are no longer members go.
    pub(crate) fn settle(&mut self, intent: &StatusIntent) -> Result<usize, PathStatusError> {
        let generation = i64::try_from(intent.generation)
            .map_err(|_| PathStatusError::GenerationOutOfRange(intent.generation))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changes = plan_settlement(&transaction, intent)?;
        {
            let mut clear = transaction
                .prepare_cached("DELETE FROM path_status WHERE rel_path = ?1 AND state = ?2")?;
            for (path, state) in &changes.clear {
                clear.execute(params![path, state.as_str()])?;
            }
            // A failure describes the work this settlement covers, so it is
            // stamped with the settlement's own cutoff: a settlement of later
            // work can clear it, and it never looks newer than that work.
            let mark = i64::try_from(intent.marked_through).unwrap_or(i64::MAX);
            let mut fail = transaction.prepare_cached(UPSERT_PATH_STATUS)?;
            for (path, reason) in &changes.fail {
                fail.execute(params![
                    path,
                    PathState::Failed.as_str(),
                    reason,
                    generation,
                    mark
                ])?;
            }
            let mut upsert = transaction.prepare_cached(
                "INSERT INTO path_skips (rel_path, reason, since_generation)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(rel_path) DO UPDATE SET
                     reason = excluded.reason,
                     since_generation = excluded.since_generation",
            )?;
            let mut unskip =
                transaction.prepare_cached("DELETE FROM path_skips WHERE rel_path = ?1")?;
            for (path, reason) in &changes.skips {
                match reason {
                    Some(reason) => upsert.execute(params![path, reason.as_str(), generation])?,
                    None => unskip.execute(params![path])?,
                };
            }
        }
        transaction.commit()?;
        if !changes.is_empty() {
            self.invalidate_health();
        }
        Ok(changes.clear.len() + changes.fail.len() + changes.skips.len())
    }

    /// Every recorded skip, by path.
    pub(crate) fn skip_reasons(
        &self,
    ) -> Result<std::collections::BTreeMap<Vec<u8>, String>, PathStatusError> {
        let mut skips = self
            .connection
            .prepare("SELECT rel_path, reason FROM path_skips")?;
        let rows = skips
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
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
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mark = next_mark(&transaction)?;
        transaction.execute(
            UPSERT_PATH_STATUS,
            params![rel_path, state.as_str(), reason, generation, mark],
        )?;
        transaction.commit()?;
        self.invalidate_health();
        Ok(())
    }

    fn invalidate_health(&self) {
        // Writers must not keep a retired runtime's health reader alive.
        // Without an observer, the next runtime builds its projection lazily.
        if let Some(health) = self
            .health
            .upgrade()
            .or_else(|| health::Health::existing(&self.path))
        {
            health.committed();
        }
    }

    /// The ledger's current mark epoch. Annotations written since marks were
    /// introduced are stamped at or below it; older rows have no mark and
    /// count as epoch 0. A publication reads it before its preparation reads
    /// the checkout, and its settlement clears nothing marked later.
    pub(crate) fn mark_epoch(&self) -> Result<u64, PathStatusError> {
        let epoch: Option<i64> = self
            .connection
            .query_row(
                "SELECT epoch FROM path_status_marks WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(epoch.map_or(0, |epoch| u64::try_from(epoch).unwrap_or(0)))
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
pub(crate) mod test_support {
    use super::PathStatusStore;
    use std::cell::Cell;

    thread_local! {
        static IN_TRANSACTION: Cell<bool> = const { Cell::new(false) };
        static WRITE_TRANSACTIONS: Cell<usize> = const { Cell::new(0) };
    }

    // Observe SQLite statements, including implicit transactions, rather than
    // counting calls to the batch API: a caller can accidentally bypass that API.
    pub(crate) fn count_write_transactions(
        store: &mut PathStatusStore,
        operation: impl FnOnce(&mut PathStatusStore),
    ) -> usize {
        IN_TRANSACTION.set(false);
        WRITE_TRANSACTIONS.set(0);
        store.connection.trace(Some(|sql| {
            let sql = sql.trim_start();
            if sql.starts_with("BEGIN") {
                IN_TRANSACTION.set(true);
            } else if sql.starts_with("COMMIT") {
                WRITE_TRANSACTIONS.set(WRITE_TRANSACTIONS.get() + 1);
                IN_TRANSACTION.set(false);
            } else if sql.starts_with("ROLLBACK") {
                IN_TRANSACTION.set(false);
            } else if !IN_TRANSACTION.get()
                && (sql.starts_with("INSERT") || sql.starts_with("DELETE"))
            {
                WRITE_TRANSACTIONS.set(WRITE_TRANSACTIONS.get() + 1);
            }
        }));
        operation(store);
        store.connection.trace(None);
        WRITE_TRANSACTIONS.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_batch_matches_per_row_states_reasons_and_ages() {
        let dir = tempfile::tempdir().unwrap();
        let mut per_row = PathStatusStore::open(&dir.path().join("per-row")).unwrap();
        let mut batched = PathStatusStore::open(&dir.path().join("batched")).unwrap();
        for store in [&mut per_row, &mut batched] {
            store.mark_pending(b"pending", "old pending", 2).unwrap();
            store.mark_failed(b"failed", "old failure", 3).unwrap();
            store.mark_pending(b"cleared", "old cleared", 4).unwrap();
            store.clear(b"cleared").unwrap();
            store.mark_failed(b"untouched", "keep failed", 5).unwrap();
        }
        let rows = [
            (b"new".as_slice(), "new path"),
            (b"pending", "still pending"),
            (b"failed", "retry failure"),
            (b"cleared", "requeued"),
            (b"\x80binary", "byte path"),
            (b"new", "last reason wins"),
        ];
        for (path, reason) in rows {
            per_row.mark_pending(path, reason, 9).unwrap();
        }
        batched.mark_pending_many(rows, 9).unwrap();
        assert_eq!(batched.summary().unwrap(), per_row.summary().unwrap());
        for (path, age, reason) in [
            (b"new".as_slice(), 9, "last reason wins"),
            (b"pending", 2, "still pending"),
            (b"failed", 9, "retry failure"),
            (b"cleared", 9, "requeued"),
        ] {
            let row = batched.status_for(path).unwrap().unwrap();
            assert_eq!(row.state, PathState::Pending);
            assert_eq!(row.since_generation, age);
            assert_eq!(row.reason, reason);
        }
        assert_eq!(
            batched.status_for(b"untouched").unwrap().unwrap().state,
            PathState::Failed
        );
    }

    #[test]
    fn pending_batch_sql_error_and_cancellation_roll_back_all_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        store.mark_failed(b"existing", "failure", 2).unwrap();
        let before = store.summary().unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_b BEFORE INSERT ON path_status WHEN new.rel_path = x'62'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        assert!(store
            .mark_pending_many(
                [
                    (b"existing".as_slice(), "retry"),
                    (b"a", "new"),
                    (b"b", "new")
                ],
                9,
            )
            .is_err());
        assert_eq!(store.summary().unwrap(), before);
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_b")
            .unwrap();
        let paths = (0..5000).map(|n| format!("path-{n}")).collect::<Vec<_>>();
        let cancelled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store
                .mark_pending_many(
                    paths.iter().enumerate().map(|(index, path)| {
                        assert_ne!(index, 1000, "injected cancellation after 1000 rows");
                        (path.as_bytes(), "pending")
                    }),
                    9,
                )
                .unwrap();
        }));
        assert!(cancelled.is_err());
        drop(store);
        let store = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(store.summary().unwrap(), before);
        let integrity: String = store
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    #[test]
    fn pending_batch_process_exit_before_commit_leaves_no_partial_rows() {
        const CHILD_DIR: &str = "AFT_PENDING_BATCH_CRASH_DIR";
        const INTERRUPTED_EXIT: i32 = 86;
        if let Some(dir) = std::env::var_os(CHILD_DIR) {
            let mut store = PathStatusStore::open(Path::new(&dir)).unwrap();
            let paths = (0..5000).map(|n| format!("path-{n}")).collect::<Vec<_>>();
            store
                .mark_pending_many(
                    paths.iter().enumerate().map(|(index, path)| {
                        if index == 1000 {
                            // Exit without destructors, leaving SQLite to recover the
                            // uncommitted transaction when the parent opens the store.
                            std::process::exit(INTERRUPTED_EXIT);
                        }
                        (path.as_bytes(), "pending")
                    }),
                    9,
                )
                .unwrap();
            panic!("crash injection did not execute");
        }
        let dir = tempfile::tempdir().unwrap();
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        store.mark_failed(b"existing", "failure", 2).unwrap();
        let before = store.summary().unwrap();
        drop(store);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "path_status::tests::pending_batch_process_exit_before_commit_leaves_no_partial_rows", "--nocapture"])
            .env(CHILD_DIR, dir.path())
            .status().unwrap();
        assert_eq!(status.code(), Some(INTERRUPTED_EXIT));
        let store = PathStatusStore::open(dir.path()).unwrap();
        assert_eq!(store.summary().unwrap(), before);
    }

    #[test]
    fn pending_batch_5000_rows_use_one_immediate_transaction_and_empty_uses_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        let paths = (0..5000).map(|n| format!("path-{n}")).collect::<Vec<_>>();
        let started = std::time::Instant::now();
        let count = test_support::count_write_transactions(&mut store, |store| {
            store
                .mark_pending_many(paths.iter().map(|path| (path.as_bytes(), "queued")), 1)
                .unwrap();
        });
        eprintln!("5000-row pending transaction: {:?}", started.elapsed());
        assert_eq!(count, 1);
        assert_eq!(store.summary().unwrap().pending_count, 5000);
        let count = test_support::count_write_transactions(&mut store, |store| {
            store.mark_pending_many(std::iter::empty(), 1).unwrap();
        });
        assert_eq!(count, 0);
        assert!(matches!(
            store.mark_pending_many([(b"bad".as_slice(), "bad")], u64::MAX),
            Err(PathStatusError::GenerationOutOfRange(_))
        ));
        assert!(store.status_for(b"bad").unwrap().is_none());
        let synchronous: i64 = store
            .connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 2, "batching must not weaken FULL durability");
    }

    #[test]
    fn repeated_status_writes_use_wal_without_rollback_journals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.sqlite");
        // Older views already have a DELETE-mode status database.
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch(PATH_STATUS_SCHEMA).unwrap();
        drop(old);
        let mut store = PathStatusStore::open(dir.path()).unwrap();
        let mode: String = store
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        for index in 0..32 {
            store
                .mark_pending(format!("file_{index}.ts").as_bytes(), "waiting", 1)
                .unwrap();
        }
        assert_eq!(store.summary().unwrap().pending_count, 32);
        assert!(dir.path().join("derived.sqlite-wal").is_file());
        assert!(!dir.path().join("derived.sqlite-journal").exists());
        drop(store);
        assert_eq!(
            PathStatusStore::open(dir.path())
                .unwrap()
                .summary()
                .unwrap()
                .pending_count,
            32
        );
    }

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
                skipped_by_reason: Default::default(),
                skipped_sample: Vec::new(),
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
    #[test]
    fn completed_path_batch_is_atomic_and_preserves_pending_paths() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = super::PathStatusStore::open(directory.path()).unwrap();
        for path in [b"a".as_slice(), b"b", b"pending"] {
            store.mark_pending(path, "queued", 1).unwrap();
        }
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse_b BEFORE DELETE ON path_status WHEN old.rel_path = x'62'
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
            )
            .unwrap();
        assert!(store.clear_many([b"a".as_slice(), b"b"]).is_err());
        assert!(store.status_for(b"a").unwrap().is_some());
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_b")
            .unwrap();
        store.clear_many([b"a".as_slice(), b"b"]).unwrap();
        assert!(store.status_for(b"a").unwrap().is_none());
        assert!(store.status_for(b"b").unwrap().is_none());
        assert!(store.status_for(b"pending").unwrap().is_some());
    }

    fn intent(
        generation: u64,
        members: &[&[u8]],
        keep_pending: &[&[u8]],
        fresh_skips: &[(&[u8], Option<SkipReason>)],
    ) -> StatusIntent {
        StatusIntent {
            generation,
            members: members.iter().map(|path| path.to_vec()).collect(),
            keep_pending: keep_pending.iter().map(|path| path.to_vec()).collect(),
            marked_through: u64::MAX,
            failed: Default::default(),
            fresh_skips: fresh_skips
                .iter()
                .map(|(path, reason)| (path.to_vec(), *reason))
                .collect(),
        }
    }

    #[test]
    fn settle_clears_other_pending_rows_and_keeps_waiting_paths_and_non_member_failures() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = super::PathStatusStore::open(directory.path()).unwrap();
        for path in [
            b"deleted.png".as_slice(),
            b"excluded.db",
            b"waiting.rs",
            b"joined.rs",
        ] {
            store.mark_pending(path, "scheduled", 1).unwrap();
        }
        store.mark_failed(b"broken.rs", "quarantined", 1).unwrap();
        store.mark_failed(b"fixed.rs", "quarantined", 1).unwrap();
        let intent = intent(
            2,
            &[b"waiting.rs", b"joined.rs", b"fixed.rs"],
            &[b"waiting.rs"],
            &[],
        );

        assert_eq!(store.settle(&intent).unwrap(), 4);

        let summary = store.summary().unwrap();
        assert_eq!((summary.pending_count, summary.failed_count), (1, 1));
        assert_eq!(
            store.status_for(b"waiting.rs").unwrap().unwrap().state,
            PathState::Pending
        );
        assert_eq!(
            store.status_for(b"broken.rs").unwrap().unwrap().state,
            PathState::Failed
        );
        // Settled once, the same intent writes nothing.
        assert!(store.plan_settlement(&intent).unwrap().is_empty());
        assert_eq!(store.settle(&intent).unwrap(), 0);
    }

    #[test]
    fn skips_are_counted_by_reason_apart_from_pending_and_failed() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = super::PathStatusStore::open(directory.path()).unwrap();
        let names = (0..7)
            .map(|index| format!("latin1_{index}.py").into_bytes())
            .collect::<Vec<_>>();
        let mut members = names.iter().map(Vec::as_slice).collect::<Vec<_>>();
        members.push(b"waiting.rs");
        let skipped = names
            .iter()
            .map(|name| (name.as_slice(), Some(SkipReason::NonUtf8Source)))
            .collect::<Vec<_>>();
        store
            .settle(&intent(2, &members, &[b"waiting.rs"], &skipped))
            .unwrap();
        store.mark_pending(b"waiting.rs", "scheduled", 2).unwrap();

        let summary = store.summary().unwrap();
        assert_eq!((summary.pending_count, summary.failed_count), (1, 0));
        assert_eq!(
            summary.skipped_by_reason,
            std::collections::BTreeMap::from([("non_utf8_source".to_string(), 7)])
        );
        assert_eq!(summary.skipped_sample.len(), SKIPPED_PATH_SAMPLE_CAP);
        assert_eq!(summary.skipped_sample[0].rel_path, b"latin1_0.py");
        assert_eq!(summary.skipped_sample[0].reason, "non_utf8_source");
        assert_eq!(
            store
                .status_for(b"waiting.rs")
                .unwrap()
                .unwrap()
                .since_generation,
            2
        );

        // A fresh read that decodes clears its skip; a path that left the
        // generation loses its skip; reused members keep theirs. Exactly
        // those two rows are written.
        members.retain(|path| *path != b"latin1_1.py".as_slice());
        let next = intent(3, &members, &[b"waiting.rs"], &[(b"latin1_0.py", None)]);
        assert_eq!(
            store.plan_settlement(&next).unwrap(),
            StatusChanges {
                clear: Vec::new(),
                fail: Vec::new(),
                skips: vec![
                    (b"latin1_0.py".to_vec(), None),
                    (b"latin1_1.py".to_vec(), None)
                ],
            }
        );
        assert_eq!(store.settle(&next).unwrap(), 2);
        let summary = store.summary().unwrap();
        assert_eq!(summary.skipped_by_reason["non_utf8_source"], 5);
        assert_eq!(summary.skipped_sample[0].rel_path, b"latin1_2.py");
        assert_eq!(summary.pending_count, 1);
    }

    /// A settlement clears only annotations marked up to the epoch it read
    /// when its preparation started; a later mark of the same or another path
    /// survives it.
    #[test]
    fn settle_leaves_annotations_marked_after_its_epoch() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = super::PathStatusStore::open(directory.path()).unwrap();
        store.mark_pending(b"covered.rs", "scheduled", 1).unwrap();
        store.mark_pending(b"remarked.rs", "scheduled", 1).unwrap();
        let mut covering = intent(2, &[b"covered.rs", b"remarked.rs"], &[], &[]);
        covering.marked_through = store.mark_epoch().unwrap();
        store
            .mark_pending_many(
                [(b"remarked.rs".as_slice(), "newer"), (b"newer.rs", "newer")],
                3,
            )
            .unwrap();
        assert!(store.mark_epoch().unwrap() > covering.marked_through);

        assert_eq!(store.settle(&covering).unwrap(), 1);
        assert!(store.status_for(b"covered.rs").unwrap().is_none());
        assert!(store.status_for(b"remarked.rs").unwrap().is_some());
        assert!(store.status_for(b"newer.rs").unwrap().is_some());
    }

    /// Opening an up-to-date ledger takes no write lock, so the status reader
    /// is never queued behind a large marking or settlement write.
    #[test]
    fn opening_an_upgraded_ledger_does_not_wait_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        drop(super::PathStatusStore::open(directory.path()).unwrap());
        let writer = rusqlite::Connection::open(directory.path().join("derived.sqlite")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        let store = super::PathStatusStore::open(directory.path()).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "open waited {:?} behind a writer",
            started.elapsed()
        );
        assert_eq!(store.summary().unwrap().pending_count, 0);
        writer.execute_batch("ROLLBACK").unwrap();
    }

    /// A ledger written before annotations carried a mark still reads, its
    /// rows count as marked before any settlement, and new marks are stamped.
    #[test]
    fn ledger_without_marks_reads_and_settles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("derived.sqlite");
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE path_status (
                 rel_path BLOB NOT NULL PRIMARY KEY,
                 state TEXT NOT NULL CHECK(state IN ('pending', 'failed')),
                 reason TEXT NOT NULL,
                 since_generation INTEGER NOT NULL CHECK(since_generation >= 0)
             ) WITHOUT ROWID;
             INSERT INTO path_status VALUES (x'6f6c642e7273', 'pending', 'scheduled', 1);",
        )
        .unwrap();
        drop(old);

        let mut store = super::PathStatusStore::open(directory.path()).unwrap();
        assert_eq!(store.summary().unwrap().pending_count, 1);
        assert_eq!(store.mark_epoch().unwrap(), 0);
        store.mark_pending(b"new.rs", "scheduled", 2).unwrap();
        assert_eq!(store.mark_epoch().unwrap(), 1);

        let mut covering = intent(2, &[b"old.rs", b"new.rs"], &[], &[]);
        covering.marked_through = 0;
        assert_eq!(store.settle(&covering).unwrap(), 1);
        assert!(store.status_for(b"old.rs").unwrap().is_none());
        assert!(store.status_for(b"new.rs").unwrap().is_some());
        drop(store);
        // Reopening an upgraded ledger leaves it as it is.
        assert_eq!(
            super::PathStatusStore::open(directory.path())
                .unwrap()
                .summary()
                .unwrap()
                .pending_count,
            1
        );
    }
}
