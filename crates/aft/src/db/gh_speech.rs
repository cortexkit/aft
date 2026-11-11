//! Durable speech rows. Authority validation belongs to the caller.

use std::path::Path;
use std::time::Duration;

use rusqlite::{params, OptionalExtension};

use super::{SqliteStore, TrackedConnection};

pub(crate) mod retention;

pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(50);

pub(crate) const LEGACY_DISPATCH_MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS gh_speech_legacy_dispatch (
    request_nonce TEXT PRIMARY KEY NOT NULL,
    incarnation TEXT NOT NULL,
    forwarding_run TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('queued','settled')),
    enqueued_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS gh_speech_legacy_pending ON gh_speech_legacy_dispatch(incarnation,state);
"#;

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS gh_speech_authorities (
    incarnation TEXT PRIMARY KEY NOT NULL,
    minting_run TEXT NOT NULL,
    harness TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    record_json TEXT NOT NULL,
    record_digest BLOB NOT NULL CHECK(length(record_digest)=32),
    ticket_digest BLOB UNIQUE CHECK(ticket_digest IS NULL OR length(ticket_digest)=32),
    phase TEXT NOT NULL CHECK(phase IN ('candidate','active','revoked')),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation>=0),
    UNIQUE(harness,session_id,task_id)
);
CREATE INDEX IF NOT EXISTS gh_speech_task_identity ON gh_speech_authorities(session_id,task_id);
CREATE TABLE IF NOT EXISTS gh_speech_task_revocations (
    incarnation TEXT PRIMARY KEY NOT NULL,
    reason TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS gh_speech_scope_ends (
    owner TEXT NOT NULL,
    scope_ref TEXT NOT NULL,
    epoch TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY(owner,scope_ref,epoch)
);
CREATE TABLE IF NOT EXISTS gh_speech_policy_versions (
    artifact_id TEXT NOT NULL,
    version TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest)=32),
    envelope BLOB NOT NULL,
    PRIMARY KEY(artifact_id,version)
);
CREATE TABLE IF NOT EXISTS gh_speech_policy_watermarks (
    artifact_id TEXT PRIMARY KEY NOT NULL,
    version TEXT NOT NULL,
    digest BLOB NOT NULL CHECK(length(digest)=32),
    generation INTEGER NOT NULL CHECK(generation>=0)
);
CREATE TABLE IF NOT EXISTS gh_speech_requests (
    request_nonce TEXT PRIMARY KEY NOT NULL,
    incarnation TEXT NOT NULL,
    payload_digest BLOB NOT NULL CHECK(length(payload_digest)=32),
    expect_json TEXT NOT NULL,
    prepared_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS gh_speech_initialization (
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    initialized INTEGER NOT NULL CHECK(initialized IN (0,1))
);
INSERT OR IGNORE INTO gh_speech_initialization(singleton,initialized) VALUES(1,0);
"#;

pub(crate) fn connect(path: &Path) -> rusqlite::Result<TrackedConnection> {
    let conn = TrackedConnection::open_path_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        SqliteStore::AftDb,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(conn)
}

pub(crate) struct AuthorityRow {
    pub(crate) record_json: String,
    pub(crate) record_digest: Vec<u8>,
    pub(crate) ticket_digest: Option<Vec<u8>>,
    pub(crate) phase: String,
    pub(crate) generation: i64,
    pub(crate) minting_run: String,
    pub(crate) harness: String,
    pub(crate) session_id: String,
    pub(crate) task_id: String,
}

pub(crate) fn load(
    conn: &rusqlite::Connection,
    incarnation: &str,
) -> rusqlite::Result<Option<AuthorityRow>> {
    conn.query_row(
        "SELECT record_json,record_digest,ticket_digest,phase,generation,minting_run,harness,session_id,task_id
             FROM gh_speech_authorities WHERE incarnation=?1",
        [incarnation],
        |row| Ok(AuthorityRow {
            record_json: row.get(0)?, record_digest: row.get(1)?, ticket_digest: row.get(2)?,
            phase: row.get(3)?, generation: row.get(4)?, minting_run: row.get(5)?,
            harness: row.get(6)?, session_id: row.get(7)?, task_id: row.get(8)?,
        }),
    ).optional()
}

pub(crate) fn capture(
    conn: &rusqlite::Connection,
    incarnation: &str,
    row: &AuthorityRow,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO gh_speech_authorities(incarnation,minting_run,harness,session_id,task_id,record_json,record_digest,phase)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'candidate')",
        params![incarnation,row.minting_run,row.harness,row.session_id,row.task_id,row.record_json,row.record_digest],
    )?;
    Ok(())
}

pub(crate) struct ScopeEndRow {
    pub(crate) owner: String,
    pub(crate) scope_ref: String,
    pub(crate) epoch: String,
    pub(crate) evidence_json: String,
    pub(crate) recorded_at_ms: i64,
}

pub(crate) fn record_end(conn: &rusqlite::Connection, row: &ScopeEndRow) -> rusqlite::Result<()> {
    conn.execute("INSERT OR IGNORE INTO gh_speech_scope_ends(owner,scope_ref,epoch,evidence_json,recorded_at_ms) VALUES(?1,?2,?3,?4,?5)",
        params![row.owner,row.scope_ref,row.epoch,row.evidence_json,row.recorded_at_ms])?;
    Ok(())
}
