//! Saved access grants as the permissions page lists and revokes them.
//!
//! Schema 27 gives every `access_grants` row an opaque `grant_id`, the time
//! its current form was granted and, for a grant saved from a consent card,
//! that card's elicitation id. The grant id is minted when the row is first
//! written and kept while the row lives, including when a read grant is
//! widened to write in place; a folder granted again after a revoke gets a
//! new id because its row was deleted.
//!
//! Rollback below schema 27: stop AFT, drop the `idx_access_grants_grant_id`
//! index and the three columns (`ALTER TABLE access_grants DROP COLUMN
//! grant_id`, `granted_at_ms`, `card_ref`; schema-26 builds also run with the
//! columns left in place because their inserts name their columns), set
//! `schema_version.version` to 26, lower the `aft.db` entry in the storage
//! root's `reader-floor.json` to 26, then start the older AFT. No grant is
//! lost.
use crate::access_policy::{AccessOperation, GrantKind};
use crate::db::access_grants::ScopeKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;

pub(crate) const SCHEMA_VERSION: u32 = 27;
pub(crate) const SOURCE_SCHEMA_VERSION: u32 = 28;

/// Schema 28 is additive: existing grants keep unknown provenance. To roll
/// back, stop AFT, drop `access_grants.source` (or leave it for schema 27),
/// set schema_version to 27 and lower the `aft.db` minimum reader version in
/// the storage root's `reader-floor.json` to 27.
pub(crate) fn migrate_source(conn: &Connection) -> rusqlite::Result<()> {
    let exists = conn
        .prepare("PRAGMA table_info(access_grants)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "source");
    if !exists {
        conn.execute_batch("ALTER TABLE access_grants ADD COLUMN source TEXT CHECK(source IN ('person','head_grant','head_policy','unknown'))")?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantSource {
    Person,
    HeadGrant,
    HeadPolicy,
    Unknown,
}

impl GrantSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::HeadGrant => "head_grant",
            Self::HeadPolicy => "head_policy",
            Self::Unknown => "unknown",
        }
    }
}

const COLUMNS: [(&str, &str); 3] = [
    ("grant_id", "TEXT"),
    ("granted_at_ms", "INTEGER"),
    ("card_ref", "TEXT"),
];

/// Add the row-identity columns. Safe to run again on a database that
/// already has some or all of them, for example after a rollback that left
/// the columns in place.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let existing = conn
        .prepare("PRAGMA table_info(access_grants)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (column, kind) in COLUMNS {
        if !existing.iter().any(|name| name == column) {
            conn.execute_batch(&format!(
                "ALTER TABLE access_grants ADD COLUMN {column} {kind}"
            ))?;
        }
    }
    // A grant saved before this schema has no surviving record of when it
    // was granted or which card granted it: a claimed ask drops its
    // elicitation id and the consent answer itself is only logged. Such rows
    // show the upgrade time.
    backfill(conn, now_ms())?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_access_grants_grant_id ON access_grants(grant_id)",
    )
}

/// Give rows written without an id or a time (by a build older than schema
/// 27 running after a rollback) both, so no listed row lacks them.
fn backfill(conn: &Connection, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE access_grants SET grant_id='aftg_' || lower(hex(randomblob(12))) WHERE grant_id IS NULL",
        [],
    )?;
    conn.execute(
        "UPDATE access_grants SET granted_at_ms=?1 WHERE granted_at_ms IS NULL",
        params![now_ms],
    )?;
    Ok(())
}

/// A new opaque grant id: `aftg_` and 24 hex digits from the system's random
/// source.
pub(crate) fn new_grant_id() -> String {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).expect("operating system random source for grant ids");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("aftg_{hex}")
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Who holds a saved grant, as recorded when its scope registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantHolder {
    /// A head's own scope, or a head's code run saved under its own scope.
    Agent { agent_id: Option<String> },
    /// A worker's scope. `agent_id` is the owning head's, read from the
    /// head scope the worker's stamp names as its parent.
    Worker {
        agent_id: Option<String>,
        scope_ref: String,
    },
}

impl GrantHolder {
    pub fn agent_id(&self) -> Option<&str> {
        match self {
            Self::Agent { agent_id } | Self::Worker { agent_id, .. } => agent_id.as_deref(),
        }
    }
}

/// One live saved grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRow {
    pub grant_id: String,
    pub scope: ScopeKey,
    pub holder: GrantHolder,
    pub path: PathBuf,
    pub kind: GrantKind,
    pub operation: AccessOperation,
    pub granted_at_ms: i64,
    pub card_ref: Option<String>,
    pub source: Option<GrantSource>,
}

/// A grant counts as live while its scope row is the newest epoch recorded
/// for that scope and has not ended, and the scope registered as a head, a
/// worker or a run. An older epoch's grants no longer authorize calls: each
/// epoch registers its own scope row, and a call reads only its own epoch's
/// grants (a head reads the newest epoch at or before its own). Scopes that
/// never registered a role, such as unstamped session scopes, hold no saved
/// answers and are left out.
const LIVE: &str = "FROM access_grants g
 JOIN access_scopes s ON s.owner=g.owner AND s.scope_ref=g.scope_ref AND s.epoch=g.epoch AND s.ended=0
 JOIN access_scope_agents a ON a.owner=g.owner AND a.scope_ref=g.scope_ref AND a.epoch=g.epoch
 WHERE a.kind IN ('head','worker','run')
 AND NOT EXISTS (SELECT 1 FROM access_scopes n WHERE n.owner=g.owner AND n.scope_ref=g.scope_ref
   AND (length(n.epoch)>length(g.epoch) OR (length(n.epoch)=length(g.epoch) AND n.epoch>g.epoch)))";

const ROW: &str = "g.grant_id,g.owner,g.scope_ref,g.epoch,g.path,g.kind,g.mode,g.granted_at_ms,g.card_ref,a.kind,a.agent_id,s.parent_owner,s.parent,s.parent_epoch,g.source";

struct RawRow {
    row: GrantRow,
    role: String,
    parent: Option<(String, String, String)>,
}

fn raw_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    let epoch: String = r.get(3)?;
    let agent_id: String = r.get(10)?;
    let parent = match (
        r.get::<_, Option<String>>(11)?,
        r.get::<_, Option<String>>(12)?,
        r.get::<_, Option<String>>(13)?,
    ) {
        (Some(owner), Some(scope_ref), Some(epoch)) => Some((owner, scope_ref, epoch)),
        _ => None,
    };
    let scope = ScopeKey {
        owner: r.get(1)?,
        scope_ref: r.get(2)?,
        epoch: epoch.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    };
    Ok(RawRow {
        row: GrantRow {
            grant_id: r.get(0)?,
            holder: GrantHolder::Agent {
                agent_id: (!agent_id.is_empty()).then_some(agent_id),
            },
            scope,
            path: PathBuf::from(r.get::<_, String>(4)?),
            kind: match r.get::<_, String>(5)?.as_str() {
                "subtree" => GrantKind::Subtree,
                _ => GrantKind::File,
            },
            operation: match r.get::<_, String>(6)?.as_str() {
                "write" => AccessOperation::Write,
                _ => AccessOperation::Read,
            },
            granted_at_ms: r.get(7)?,
            card_ref: r.get(8)?,
            source: match r.get::<_, Option<String>>(14)?.as_deref() {
                Some("person") => Some(GrantSource::Person),
                Some("head_grant") => Some(GrantSource::HeadGrant),
                Some("head_policy") => Some(GrantSource::HeadPolicy),
                Some("unknown") => Some(GrantSource::Unknown),
                Some(_) => Some(GrantSource::Unknown),
                None => None,
            },
        },
        role: r.get(9)?,
        parent,
    })
}

/// The agent id a head scope registered with: the exact epoch the worker's
/// stamp names, or else that head's newest registration, because a head
/// keeps its agent id across revivals.
fn head_agent(
    conn: &Connection,
    (owner, scope_ref, epoch): &(String, String, String),
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT agent_id FROM access_scope_agents WHERE owner=?1 AND scope_ref=?2 AND kind='head' AND agent_id<>''
         ORDER BY epoch=?3 DESC, length(epoch) DESC, epoch DESC LIMIT 1",
        params![owner, scope_ref, epoch],
        |row| row.get(0),
    )
    .optional()
}

fn resolve(conn: &Connection, raw: RawRow) -> rusqlite::Result<GrantRow> {
    let RawRow {
        mut row,
        role,
        parent,
    } = raw;
    if role == "worker" {
        row.holder = GrantHolder::Worker {
            agent_id: match parent.as_ref() {
                Some(parent) => head_agent(conn, parent)?,
                None => None,
            },
            scope_ref: row.scope.scope_ref.clone(),
        };
    }
    Ok(row)
}

/// Every live saved grant, ordered by holder scope and path.
pub fn live_rows(conn: &Connection) -> rusqlite::Result<Vec<GrantRow>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    backfill(&tx, now_ms())?;
    let raw = tx
        .prepare(&format!(
            "SELECT {ROW} {LIVE} ORDER BY g.owner,g.scope_ref,g.path,g.kind"
        ))?
        .query_map([], raw_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rows = raw
        .into_iter()
        .map(|raw| resolve(&tx, raw))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tx.commit()?;
    Ok(rows)
}

/// Delete the live grant with this id and return it. `None` when no live
/// grant has the id: never issued, already revoked, or its scope has ended
/// or been superseded by a newer epoch.
pub fn revoke_live(conn: &Connection, grant_id: &str) -> rusqlite::Result<Option<GrantRow>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let raw = tx
        .query_row(
            &format!("SELECT {ROW} {LIVE} AND g.grant_id=?1"),
            params![grant_id],
            raw_row,
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let row = resolve(&tx, raw)?;
    tx.execute(
        "DELETE FROM access_grants WHERE grant_id=?1",
        params![grant_id],
    )?;
    tx.commit()?;
    Ok(Some(row))
}

#[cfg(test)]
#[path = "access_grant_rows_tests.rs"]
mod tests;
