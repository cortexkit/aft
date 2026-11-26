//! Scope-bound capabilities and durable launch parameters on the owning aft.db handle.
//! Rollback to schema 15 requires stopping AFT, dropping access_scopes, access_grants, access_launch_params, and access_materialized, then setting schema_version.version to 15; this discards grants and launch state.
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
use crate::config::Config;
use crate::db::remote_exec::PolicyKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

mod prefrontal_owner;
pub(crate) use prefrontal_owner::migrate_prefrontal_owner;

pub fn owner_key(principal: &subc_protocol::Principal) -> String {
    serde_json::to_string(principal).expect("scope owner serializes")
}

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS access_scopes (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 parent_owner TEXT, parent TEXT, parent_epoch TEXT, config TEXT NOT NULL, ended INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(owner,scope_ref,epoch)
);
CREATE TABLE IF NOT EXISTS access_grants (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 path TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('file','subtree')),
 mode TEXT NOT NULL CHECK(mode IN ('read','write')),
 PRIMARY KEY(owner,scope_ref,epoch,path,kind)
);
CREATE TABLE IF NOT EXISTS access_launch_params (
 project_root TEXT NOT NULL, harness TEXT NOT NULL, session TEXT NOT NULL,
 principal TEXT NOT NULL, owner TEXT NOT NULL, scope_ref TEXT NOT NULL,
 epoch TEXT NOT NULL, preset TEXT NOT NULL, params TEXT NOT NULL,
 PRIMARY KEY(project_root,harness,session,principal,owner,scope_ref,epoch,preset)
);
CREATE TABLE IF NOT EXISTS access_materialized (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 PRIMARY KEY(owner,scope_ref,epoch)
);
"#;

// Manual rollback procedure for operators downgrading from schema 18: stop AFT,
// drop access_modes, access_scope_agents and macos_permission_cards, then set
// schema_version to 17.
// This discards approval modes and informational-card tracking, not path grants.
pub(crate) const MODES_MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS access_modes (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 mode TEXT NOT NULL, PRIMARY KEY(owner,scope_ref,epoch)
);
CREATE TABLE IF NOT EXISTS access_scope_agents (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 agent_id TEXT NOT NULL, kind TEXT NOT NULL,
 PRIMARY KEY(owner,scope_ref,epoch)
);
CREATE TABLE IF NOT EXISTS macos_permission_cards (
 folder TEXT NOT NULL, program TEXT NOT NULL, call_key TEXT NOT NULL,
 selector TEXT NOT NULL, project_root TEXT NOT NULL, session TEXT NOT NULL, elicitation_id TEXT,
 PRIMARY KEY(folder,program)
);
"#;

/// The caller supplies only identities verified in the daemon's scope stamp.
/// A head's code run has no parent in that stamp, so its agent id links its
/// approval mode to the corresponding live head session.
pub fn register_agent(
    conn: &Connection,
    key: &ScopeKey,
    agent_id: &str,
    head: bool,
) -> rusqlite::Result<()> {
    register_agent_kind(conn, key, agent_id, if head { "head" } else { "run" })
}

pub fn register_worker(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<()> {
    register_agent_kind(conn, key, "", "worker")
}

fn register_agent_kind(
    conn: &Connection,
    key: &ScopeKey,
    agent_id: &str,
    kind: &str,
) -> rusqlite::Result<()> {
    conn.execute("INSERT OR REPLACE INTO access_scope_agents(owner,scope_ref,epoch,agent_id,kind) VALUES (?1,?2,?3,?4,?5)", params![key.owner,key.scope_ref,key.epoch.to_string(),agent_id,kind])?;
    Ok(())
}

/// Only a live daemon-attested worker receives built-in worker capabilities.
pub fn is_worker(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM access_scope_agents a JOIN access_scopes s ON s.owner=a.owner AND s.scope_ref=a.scope_ref AND s.epoch=a.epoch WHERE a.owner=?1 AND a.scope_ref=?2 AND a.epoch=?3 AND a.kind='worker' AND s.ended=0)", params![key.owner,key.scope_ref,key.epoch.to_string()], |row| row.get(0))
}

fn agent_head(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Option<ScopeKey>> {
    conn.query_row("SELECT h.scope_ref,h.epoch FROM access_scope_agents r JOIN access_scope_agents h ON h.owner=r.owner AND h.agent_id=r.agent_id AND h.kind='head' JOIN access_scopes s ON s.owner=h.owner AND s.scope_ref=h.scope_ref AND s.epoch=h.epoch AND s.ended=0 WHERE r.owner=?1 AND r.scope_ref=?2 AND r.epoch=?3 AND r.kind='run' AND r.agent_id<>'' ORDER BY length(h.epoch) DESC,h.epoch DESC LIMIT 1", params![key.owner,key.scope_ref,key.epoch.to_string()], |r| {
        let epoch: String = r.get(1)?;
        Ok(ScopeKey {owner:key.owner.clone(),scope_ref:r.get(0)?,epoch:epoch.parse().map_err(|e| rusqlite::Error::FromSqlConversionFailure(1,rusqlite::types::Type::Text,Box::new(e)))?})
    }).optional()
}

pub fn is_head(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM access_scope_agents a JOIN access_scopes s USING(owner,scope_ref,epoch) WHERE a.owner=?1 AND a.scope_ref=?2 AND a.epoch=?3 AND a.kind='head' AND s.ended=0)",
        params![key.owner, key.scope_ref, key.epoch.to_string()],
        |row| row.get(0),
    )
}

pub fn set_mode(
    conn: &Connection,
    key: &ScopeKey,
    mode: crate::access_policy::ApprovalMode,
) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO access_modes(owner,scope_ref,epoch,mode) SELECT ?1,?2,?3,?4 WHERE EXISTS (SELECT 1 FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0) ON CONFLICT(owner,scope_ref,epoch) DO UPDATE SET mode=excluded.mode",
        params![key.owner,key.scope_ref,key.epoch.to_string(),serde_json::to_string(&mode).expect("mode serializes")])?;
    Ok(())
}

/// For attested heads, absent stored settings allow outside-root access for now
/// (the deny floor still wins). Workers and standalone scopes still ask. Only
/// explicitly stored parent settings are inherited, never the head's fallback.
/// Bound ancestry traversal also fails closed on corrupt cyclic rows.
pub fn mode(
    conn: &Connection,
    key: &ScopeKey,
) -> rusqlite::Result<crate::access_policy::ApprovalMode> {
    use crate::access_policy::ApprovalMode;
    let kind: Option<String> = conn.query_row(
        "SELECT a.kind FROM access_scope_agents a JOIN access_scopes s ON s.owner=a.owner AND s.scope_ref=a.scope_ref AND s.epoch=a.epoch AND s.ended=0 WHERE a.owner=?1 AND a.scope_ref=?2 AND a.epoch=?3",
        params![key.owner,key.scope_ref,key.epoch.to_string()], |row| row.get(0),
    ).optional()?;
    let fallback = if kind.as_deref() == Some("head")
        || (kind.as_deref() == Some("run") && parent(conn, key)?.is_none())
    {
        ApprovalMode::FullAccess
    } else {
        ApprovalMode::Ask
    };
    let mut scope = key.clone();
    for _ in 0..16 {
        let value: Option<String> = conn
            .query_row(
                "SELECT mode FROM access_modes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
                params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(value) = value {
            return decode(&value);
        }
        let next = parent(conn, &scope)?.or(agent_head(conn, &scope)?);
        let Some(next) = next else {
            return Ok(fallback);
        };
        scope = next;
    }
    Err(rusqlite::Error::InvalidQuery)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeKey {
    pub owner: String,
    pub scope_ref: String,
    pub epoch: u64,
}

impl ScopeKey {
    /// Exact wire identity for callers that also use this shape for history.
    pub fn from_stamp(stamp: &subc_protocol::scope::ScopeStamp) -> Self {
        Self {
            owner: owner_key(&stamp.owner),
            scope_ref: stamp.scope_ref.clone(),
            epoch: stamp.scope_epoch,
        }
    }

    /// Storage identity shared by grants, their scope registrations and roles.
    pub fn for_grants(stamp: &subc_protocol::scope::ScopeStamp) -> Self {
        Self::from_stamp(stamp)
    }

    pub fn from_policy(key: &PolicyKey) -> Self {
        Self {
            owner: serde_json::from_str(&key.owner)
                .map(|owner| owner_key(&owner))
                .unwrap_or_else(|_| key.owner.clone()),
            scope_ref: key.scope_ref.clone(),
            epoch: key.epoch,
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> rusqlite::Result<T> {
    serde_json::from_str(json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

/// Only a daemon-attested bind may register the ancestry used by grant operations.
/// Ended identities are tombstoned so a delayed catalog cannot revive their grants.
pub fn register_scope(
    conn: &Connection,
    key: &ScopeKey,
    parent: Option<&ScopeKey>,
    config: &Config,
) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO access_scopes(owner,scope_ref,epoch,parent_owner,parent,parent_epoch,config) VALUES (?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(owner,scope_ref,epoch) DO UPDATE SET parent_owner=excluded.parent_owner, parent=excluded.parent, parent_epoch=excluded.parent_epoch, config=excluded.config WHERE ended=0",
        params![key.owner,key.scope_ref,key.epoch.to_string(),parent.map(|p|&p.owner),parent.map(|p|&p.scope_ref),parent.map(|p|p.epoch.to_string()),serde_json::to_string(config).expect("config serializes")])?;
    Ok(())
}

pub fn scope_config(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Option<Config>> {
    let json: Option<String> = conn.query_row("SELECT config FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0",
        params![key.owner,key.scope_ref,key.epoch.to_string()], |r| r.get(0)).optional()?;
    json.map(|json| decode(&json)).transpose()
}

/// Confirmed ends remain authoritative even when cleanup of another table fails.
pub fn scope_ended(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended!=0)",
        params![key.owner,key.scope_ref,key.epoch.to_string()], |row| row.get(0))
}

/// Resolve a head's last observed policy at or before its attested epoch.
/// Select tombstones too: an ended epoch must block an older live policy.
/// Read grants from the selected row instead of copying them, so revocation
/// remains effective on the next check.
pub fn head_policy_scope(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Option<ScopeKey>> {
    // Match remote-exec's unsigned decimal ordering without a signed SQL cast.
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT epoch,ended FROM access_scopes WHERE owner=?1 AND scope_ref=?2
             AND (length(epoch)<length(?3) OR (length(epoch)=length(?3) AND epoch<=?3))
             ORDER BY length(epoch) DESC,epoch DESC LIMIT 1",
            params![key.owner, key.scope_ref, key.epoch.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((epoch, ended)) = row else {
        return Ok(None);
    };
    if ended {
        return Ok(None);
    }
    Ok(Some(ScopeKey {
        epoch: epoch.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
        ..key.clone()
    }))
}

pub fn resolve_scope(
    conn: &Connection,
    owner: &str,
    scope_ref: &str,
) -> rusqlite::Result<Option<ScopeKey>> {
    let epoch: Option<String> = conn.query_row("SELECT epoch FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND ended=0 ORDER BY length(epoch) DESC, epoch DESC LIMIT 1",
        params![owner,scope_ref], |r| r.get(0)).optional()?;
    epoch
        .map(|epoch| {
            Ok(ScopeKey {
                owner: owner.into(),
                scope_ref: scope_ref.into(),
                epoch: epoch.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })
        .transpose()
}

/// The registered parent of a live scope, if it has one.
pub fn parent(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Option<ScopeKey>> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT parent_owner,parent,parent_epoch FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0",
            params![key.owner, key.scope_ref, key.epoch.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Some(owner), Some(scope_ref), Some(epoch))) => {
            epoch.parse().ok().map(|epoch| ScopeKey {
                owner,
                scope_ref,
                epoch,
            })
        }
        _ => None,
    })
}

pub fn authorized(
    conn: &Connection,
    caller: &ScopeKey,
    target: &ScopeKey,
) -> rusqlite::Result<bool> {
    if scope_config(conn, caller)?.is_none() {
        return Ok(false);
    }
    let parent: Option<(Option<String>, Option<String>, Option<String>)> = conn.query_row("SELECT parent_owner,parent,parent_epoch FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0",
        params![target.owner,target.scope_ref,target.epoch.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let current_caller = resolve_scope(conn, &caller.owner, &caller.scope_ref)?;
    Ok(current_caller.as_ref() == Some(caller)
        && parent.is_some_and(|(owner, parent, epoch)| {
            caller == target
                || (owner.as_deref() == Some(&caller.owner)
                    && parent.as_deref() == Some(&caller.scope_ref)
                    && epoch == Some(caller.epoch.to_string()))
        }))
}

pub fn list(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Vec<AccessGrant>> {
    let mut stmt = conn.prepare("SELECT path,kind,mode FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 ORDER BY path,kind")?;
    let rows = stmt
        .query_map(
            params![key.owner, key.scope_ref, key.epoch.to_string()],
            |r| {
                Ok(AccessGrant {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    kind: match r.get::<_, String>(1)?.as_str() {
                        "subtree" => GrantKind::Subtree,
                        _ => GrantKind::File,
                    },
                    operation: match r.get::<_, String>(2)?.as_str() {
                        "write" => AccessOperation::Write,
                        _ => AccessOperation::Read,
                    },
                })
            },
        )?
        .collect();
    rows
}

pub fn insert(conn: &Connection, key: &ScopeKey, grant: &AccessGrant) -> rusqlite::Result<()> {
    insert_from_card(conn, key, grant, None)
}

/// Person-given authority only; legacy rows cannot establish provenance.
pub fn person_grant_covers(
    conn: &Connection,
    key: &ScopeKey,
    requested: &AccessGrant,
) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare("SELECT path,kind,mode FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND source='person'")?;
    let rows = stmt.query_map(
        params![key.owner, key.scope_ref, key.epoch.to_string()],
        |row| {
            Ok(AccessGrant {
                path: PathBuf::from(row.get::<_, String>(0)?),
                kind: if row.get::<_, String>(1)? == "subtree" {
                    GrantKind::Subtree
                } else {
                    GrantKind::File
                },
                operation: if row.get::<_, String>(2)? == "write" {
                    AccessOperation::Write
                } else {
                    AccessOperation::Read
                },
            })
        },
    )?;
    for row in rows {
        if row?.covers_kind(requested.operation, &requested.path, requested.kind) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Save a grant, recording the consent card whose answer granted it. A new
/// row gets a fresh grant id and the current time. A read grant widened to
/// write keeps its row and id, and takes the new time and card, because its
/// form changed; an answer that changes nothing leaves the row as it was.
pub fn insert_from_card(
    conn: &Connection,
    key: &ScopeKey,
    grant: &AccessGrant,
    card_ref: Option<&str>,
) -> rusqlite::Result<()> {
    insert_with_source(
        conn,
        key,
        grant,
        card_ref,
        Some(super::access_grant_rows::GrantSource::Unknown),
    )
}

/// Record provenance with the same row identity and widening rules as grants.
pub fn insert_with_source(
    conn: &Connection,
    key: &ScopeKey,
    grant: &AccessGrant,
    card_ref: Option<&str>,
    source: Option<super::access_grant_rows::GrantSource>,
) -> rusqlite::Result<()> {
    if !grant.path.is_absolute() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let mut resolved = grant.clone();
    resolved.path = crate::policy_path::policy_path(&grant.path);
    insert_resolved_with_source(conn, key, &resolved, card_ref, source)
}

/// Insert the already-resolved grant key without probing the filesystem.
pub fn insert_resolved_with_source(
    conn: &Connection,
    key: &ScopeKey,
    grant: &AccessGrant,
    card_ref: Option<&str>,
    source: Option<super::access_grant_rows::GrantSource>,
) -> rusqlite::Result<()> {
    if !grant.path.is_absolute() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    conn.execute("INSERT INTO access_grants(owner,scope_ref,epoch,path,kind,mode,grant_id,granted_at_ms,card_ref,source)
        SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10 WHERE EXISTS (SELECT 1 FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0)
        ON CONFLICT(owner,scope_ref,epoch,path,kind) DO UPDATE SET
         granted_at_ms=CASE WHEN (excluded.mode='write' AND access_grants.mode<>'write') OR access_grants.granted_at_ms IS NULL THEN excluded.granted_at_ms ELSE access_grants.granted_at_ms END,
          card_ref=CASE WHEN excluded.mode='write' AND access_grants.mode<>'write' THEN excluded.card_ref ELSE access_grants.card_ref END,
          source=CASE WHEN excluded.mode='write' AND access_grants.mode<>'write' THEN excluded.source ELSE access_grants.source END,
         grant_id=coalesce(access_grants.grant_id,excluded.grant_id),
         mode=CASE WHEN excluded.mode='write' THEN 'write' ELSE access_grants.mode END",
        params![key.owner,key.scope_ref,key.epoch.to_string(),grant.path.to_string_lossy(),match grant.kind {GrantKind::File=>"file",GrantKind::Subtree=>"subtree"},match grant.operation {AccessOperation::Read=>"read",AccessOperation::Write=>"write"},super::access_grant_rows::new_grant_id(),super::access_grant_rows::now_ms(),card_ref,source.map(|source| source.as_str())])?;
    aft_base::scope_watch_events::changed(aft_base::scope_watch_events::Source::Grants);
    Ok(())
}

pub fn revoke(conn: &Connection, key: &ScopeKey, path: &Path) -> rusqlite::Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND path=?4",
        params![
            key.owner,
            key.scope_ref,
            key.epoch.to_string(),
            path.to_string_lossy()
        ],
    )?;
    if deleted > 0 {
        aft_base::scope_watch_events::changed(aft_base::scope_watch_events::Source::Grants);
    }
    Ok(deleted)
}

/// Whether `key` still holds a saved grant for exactly `path`, of any kind or
/// mode. A command's sandbox records the grant paths it was built with in
/// this stored form, so a missing row means the grant was revoked.
pub fn holds(conn: &Connection, key: &ScopeKey, path: &Path) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND path=?4)",
        params![
            key.owner,
            key.scope_ref,
            key.epoch.to_string(),
            path.to_string_lossy()
        ],
        |row| row.get(0),
    )
}

/// Only the describe-confirmed scope-end path may call this helper. A route
/// close, detach, or missing worktree is not proof that a scope ended. The
/// daemon must report ended, a different live epoch, or not_live with
/// owner_synced=true; not_synced, errors, and timeouts retain all scope state.
pub fn end_scope_grants(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    end_scope_rows(&tx, key)?;
    tx.commit()?;
    aft_base::scope_watch_events::changed(aft_base::scope_watch_events::Source::Grants);
    Ok(())
}

/// The database half of a confirmed scope end, shared by the live scope-end
/// handler and the background sweep so both leave the same rows behind.
/// Returns how many grants were deleted.
fn end_scope_rows(tx: &rusqlite::Transaction<'_>, key: &ScopeKey) -> rusqlite::Result<usize> {
    // The tombstone keeps a late catalog from reviving the scope. Nothing
    // reads an ended scope's configuration, so the tombstone drops it.
    tx.execute(
        "INSERT INTO access_scopes(owner,scope_ref,epoch,config,ended) VALUES (?1,?2,?3,'{}',1)
        ON CONFLICT(owner,scope_ref,epoch) DO UPDATE SET ended=1, config='{}'",
        params![key.owner, key.scope_ref, key.epoch.to_string()],
    )?;
    let mut deleted_grants = 0;
    for table in [
        "access_grants",
        "access_launch_params",
        "access_materialized",
        "access_modes",
        "access_scope_agents",
    ] {
        let deleted = tx.execute(
            &format!("DELETE FROM {table} WHERE owner=?1 AND scope_ref=?2 AND epoch=?3"),
            params![key.owner, key.scope_ref, key.epoch.to_string()],
        )?;
        if table == "access_grants" {
            deleted_grants = deleted;
        }
    }
    crate::db::remote_exec::forget_scope(tx, &key.owner, &key.scope_ref, key.epoch)?;
    crate::db::catalog_edit_modes::forget_scope(tx, &key.owner, &key.scope_ref, key.epoch)?;
    Ok(deleted_grants)
}

/// Preserve per-row decoding errors so one corrupt row cannot hide other saved
/// scopes from the lifecycle census.
pub fn saved_scopes(conn: &Connection) -> rusqlite::Result<Vec<rusqlite::Result<ScopeKey>>> {
    let mut stmt = conn.prepare("SELECT DISTINCT owner,scope_ref,epoch FROM access_grants")?;
    let rows = stmt
        .query_map([], |row| {
            let epoch: String = row.get(2)?;
            Ok(ScopeKey {
                owner: row.get(0)?,
                scope_ref: row.get(1)?,
                epoch: epoch.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })?
        .collect();
    Ok(rows)
}

/// Keyset pagination bounds housekeeping work, not a tool's result list.
pub fn saved_scope_batch(
    conn: &Connection,
    after: Option<&ScopeKey>,
    limit: usize,
) -> rusqlite::Result<Vec<ScopeKey>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT owner,scope_ref,epoch FROM access_grants
        WHERE (owner,scope_ref,epoch) > (?1,?2,?3)
        ORDER BY owner,scope_ref,epoch LIMIT ?4",
    )?;
    let epoch = after.map(|key| key.epoch.to_string()).unwrap_or_default();
    let rows = stmt
        .query_map(
            params![
                after.map_or("", |key| key.owner.as_str()),
                after.map_or("", |key| key.scope_ref.as_str()),
                epoch,
                limit as i64
            ],
            |row| {
                let epoch: String = row.get(2)?;
                Ok(ScopeKey {
                    owner: row.get(0)?,
                    scope_ref: row.get(1)?,
                    epoch: epoch.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                })
            },
        )?
        .collect();
    rows
}

/// A definite daemon end and its saved-row cleanup commit together. The
/// cleanup matches the live scope-end handler's: `end_scope_grants`, the
/// code-mode owner rows, and the call-ledger settlement of calls that never
/// started. The sweep only sees ends that no AFT process was watching, so no
/// in-process waiter holds those calls; runners read the settlements through
/// `late_results`, as after any restart. One transaction keeps a failed step
/// retryable: until it commits, the scope's grants keep it in the sweep.
pub fn end_saved_scope(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<usize> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let deleted = end_scope_rows(&tx, key)?;
    crate::db::codemode_owners::end(&tx, key)?;
    let ledger = crate::db::call_ledger::ledger_scope(key);
    if let Some(scope) = &ledger {
        crate::db::call_ledger::end_scope_in(&tx, scope, crate::db::call_ledger::now_ms())?;
    }
    tx.commit()?;
    aft_base::scope_watch_events::changed(aft_base::scope_watch_events::Source::Grants);
    if ledger.is_some() {
        crate::db::call_ledger::scope_end_committed();
    }
    Ok(deleted)
}

pub fn launch_params(
    conn: &Connection,
    key: &PolicyKey,
) -> rusqlite::Result<Option<serde_json::Value>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let row = super::remote_exec::catalog_params(
        &tx,
        key,
        super::remote_exec::CatalogParams::AccessLaunch,
    )?;
    let params = row.map(|(_, json)| decode(&json)).transpose()?;
    tx.commit()?;
    Ok(params)
}

/// A validated launch capability and the source of its authority.
pub type SourcedGrant = (AccessGrant, Option<super::access_grant_rows::GrantSource>);

/// Validation is completed before entering this transaction. The scope-wide
/// marker and the grants commit together, including for an empty launch list.
pub fn materialize(
    conn: &Connection,
    key: &PolicyKey,
    raw: &serde_json::Value,
    grants: &[SourcedGrant],
) -> rusqlite::Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let scope = ScopeKey::from_policy(key);
    if scope_config(&tx, &scope)?.is_none() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    tx.execute("INSERT OR IGNORE INTO access_launch_params(project_root,harness,session,principal,owner,scope_ref,epoch,preset,params) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![key.project_root,key.harness,key.session,key.principal,key.owner,key.scope_ref,key.epoch.to_string(),key.preset,raw.to_string()])?;
    materialize_on(&tx, &scope, grants)?;
    tx.commit()?;
    aft_base::scope_watch_events::changed(aft_base::scope_watch_events::Source::Grants);
    Ok(())
}

fn materialize_on(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    grants: &[SourcedGrant],
) -> rusqlite::Result<()> {
    let first = tx.execute(
        "INSERT OR IGNORE INTO access_materialized(owner,scope_ref,epoch) VALUES (?1,?2,?3)",
        params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
    )? == 1;
    if first {
        for (grant, source) in grants {
            insert_with_source(tx, scope, grant, None, *source)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchRestore {
    Materialized,
    AlreadyMaterialized,
    NoParams,
}

/// Read the launch inputs without committing a carry-forward row. Validation
/// can then probe paths without holding the database lock; installation checks
/// the inputs again in its own immediate transaction.
pub fn preview_launch(
    conn: &Connection,
    key: &PolicyKey,
) -> rusqlite::Result<Option<serde_json::Value>> {
    let tx = conn.unchecked_transaction()?;
    let row = super::remote_exec::catalog_params(
        &tx,
        key,
        super::remote_exec::CatalogParams::AccessLaunch,
    )?;
    row.map(|(_, json)| decode(&json)).transpose()
}

/// Saved capabilities that have not been installed in this epoch. Reading them
/// must not probe the filesystem: an unavailable mount is precisely why callers
/// need this refusal-only description. Restoration alone validates and installs.
pub fn unrestored_launch(conn: &Connection, key: &PolicyKey) -> rusqlite::Result<Vec<AccessGrant>> {
    let scope = ScopeKey::from_policy(key);
    let materialized: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM access_materialized WHERE owner=?1 AND scope_ref=?2 AND epoch=?3)",
        params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
        |r| r.get(0),
    )?;
    if materialized {
        return Ok(Vec::new());
    }
    let Some(raw) = preview_launch(conn, key)? else {
        return Ok(Vec::new());
    };
    raw.as_array()
        .ok_or(rusqlite::Error::InvalidQuery)?
        .iter()
        .map(|entry| {
            let path = entry
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or(rusqlite::Error::InvalidQuery)?;
            let operation = match entry.get("mode").and_then(serde_json::Value::as_str) {
                Some("read") => AccessOperation::Read,
                Some("write") => AccessOperation::Write,
                _ => return Err(rusqlite::Error::InvalidQuery),
            };
            Ok(AccessGrant {
                path,
                kind: GrantKind::Subtree,
                operation,
            })
        })
        .collect()
}

/// A call can restore only persisted launch inputs, never standing consent grants.
/// The immediate transaction serializes the marker check, params carry-forward,
/// current-head validation and grant installation, even across database handles.
pub fn restore_launch(
    conn: &Connection,
    key: &PolicyKey,
    validate: impl FnOnce(&Connection, &serde_json::Value) -> rusqlite::Result<Vec<SourcedGrant>>,
) -> rusqlite::Result<LaunchRestore> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let scope = ScopeKey::from_policy(key);
    let materialized: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM access_materialized WHERE owner=?1 AND scope_ref=?2 AND epoch=?3)",
        params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
        |r| r.get(0),
    )?;
    if materialized {
        return Ok(LaunchRestore::AlreadyMaterialized);
    }
    let Some((_, json)) = super::remote_exec::catalog_params(
        &tx,
        key,
        super::remote_exec::CatalogParams::AccessLaunch,
    )?
    else {
        // A call before the initial catalog must not freeze an empty launch.
        return Ok(LaunchRestore::NoParams);
    };
    if scope_config(&tx, &scope)?.is_none() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let grants = validate(&tx, &decode(&json)?)?;
    materialize_on(&tx, &scope, &grants)?;
    tx.commit()?;
    Ok(LaunchRestore::Materialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_grants_startup_moves_prefrontal_grants_and_preserves_tombstones() {
        use crate::access_policy::ApprovalMode;
        use crate::db::access_grant_rows::{live_rows, GrantSource};
        use subc_protocol::Principal;

        let key = |module: &str, scope_ref: &str| ScopeKey {
            owner: owner_key(&Principal::Reserved {
                module_id: module.into(),
            }),
            scope_ref: scope_ref.into(),
            epoch: 7,
        };
        let old = |scope_ref| key("prefrontal-core", scope_ref);
        let new = |scope_ref| key("prefrontal", scope_ref);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        let config = Config::default();
        for name in ["head", "worker", "ended", "new-ended"] {
            register_scope(
                &conn,
                &old(name),
                (name == "worker").then(|| old("head")).as_ref(),
                &config,
            )
            .unwrap();
        }
        for name in ["worker", "ended", "new-ended"] {
            register_scope(
                &conn,
                &new(name),
                (name == "worker").then(|| old("head")).as_ref(),
                &config,
            )
            .unwrap();
        }
        let unrelated = key("other", "linked");
        register_scope(&conn, &unrelated, Some(&old("head")), &config).unwrap();
        register_agent(&conn, &old("head"), "agent", true).unwrap();
        register_worker(&conn, &old("worker")).unwrap();
        set_mode(&conn, &old("head"), ApprovalMode::Ask).unwrap();
        end_scope_grants(&conn, &old("ended")).unwrap();
        end_scope_grants(&conn, &new("new-ended")).unwrap();

        let grant = AccessGrant {
            path: crate::policy_path::policy_path(dir.path()).join("approved"),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let write = AccessGrant {
            operation: AccessOperation::Write,
            ..grant.clone()
        };
        insert_with_source(
            &conn,
            &old("head"),
            &grant,
            Some("head-card"),
            Some(GrantSource::Person),
        )
        .unwrap();
        let head_id = live_rows(&conn).unwrap()[0].grant_id.clone();
        insert(&conn, &old("worker"), &write).unwrap();
        insert(&conn, &new("worker"), &grant).unwrap();
        insert(&conn, &new("ended"), &write).unwrap();
        insert(&conn, &old("new-ended"), &write).unwrap();
        insert(&conn, &unrelated, &grant).unwrap();
        drop(conn);

        let mut conn = crate::db::open(&path).unwrap();
        assert_eq!(crate::db::current_schema_version(&conn).unwrap(), 29);
        assert!(list(&conn, &old("head")).unwrap().is_empty());
        assert_eq!(
            head_policy_scope(
                &conn,
                &ScopeKey {
                    epoch: 8,
                    ..new("head")
                }
            )
            .unwrap(),
            Some(new("head"))
        );
        assert!(is_head(&conn, &new("head")).unwrap());
        assert!(is_worker(&conn, &new("worker")).unwrap());
        assert_eq!(mode(&conn, &new("worker")).unwrap(), ApprovalMode::Ask);
        assert!(authorized(&conn, &new("head"), &new("worker")).unwrap());
        assert!(authorized(&conn, &new("head"), &unrelated).unwrap());
        assert!(!authorized(&conn, &old("head"), &new("worker")).unwrap());
        let rows = live_rows(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].scope, new("head"));
        assert_eq!(rows[0].grant_id, head_id);
        assert_eq!(rows[0].card_ref.as_deref(), Some("head-card"));
        assert_eq!(rows[0].source, Some(GrantSource::Person));
        assert_eq!(rows[1].scope, new("worker"));
        assert_eq!(rows[1].operation, AccessOperation::Read);
        assert_eq!(list(&conn, &unrelated).unwrap(), [grant]);
        for ended in ["ended", "new-ended"] {
            for scope in [old(ended), new(ended)] {
                register_scope(&conn, &scope, None, &config).unwrap();
                assert!(scope_ended(&conn, &scope).unwrap());
                assert!(list(&conn, &scope).unwrap().is_empty());
            }
        }
        let changes = |c: &Connection| {
            c.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
                .unwrap()
        };
        let before = changes(&conn);
        migrate_prefrontal_owner(&mut conn).unwrap();
        assert_eq!(changes(&conn), before);
    }

    #[test]
    fn grants_head_policy_orders_unsigned_epochs_without_signed_casts() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let config = Config::default();
        for epoch in [7, 10, u64::MAX - 1] {
            register_scope(&conn, &scope("head", epoch), None, &config).unwrap();
        }
        for (epoch, expected) in [
            (6, None),
            (7, Some(7)),
            (12, Some(10)),
            (u64::MAX, Some(u64::MAX - 1)),
        ] {
            let resolved = head_policy_scope(&conn, &scope("head", epoch)).unwrap();
            assert_eq!(resolved.map(|key| key.epoch), expected);
        }
    }
    fn scope(name: &str, epoch: u64) -> ScopeKey {
        ScopeKey {
            owner: "owner".into(),
            scope_ref: name.into(),
            epoch,
        }
    }
    fn policy(key: &ScopeKey, root: &Path) -> PolicyKey {
        PolicyKey {
            project_root: root.display().to_string(),
            harness: "broca".into(),
            session: "worker".into(),
            principal: "reserved:broca".into(),
            owner: key.owner.clone(),
            scope_ref: key.scope_ref.clone(),
            epoch: key.epoch,
            preset: "worker".into(),
        }
    }
    fn launch_grant(grant: &AccessGrant) -> SourcedGrant {
        (
            grant.clone(),
            Some(super::super::access_grant_rows::GrantSource::Unknown),
        )
    }
    #[test]
    fn grants_materialization_revoke_restart_reclaim_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let key = scope("worker", u64::MAX);
        let root = dir.path().join("root");
        let bind = policy(&key, &root);
        // Stored grants carry the canonical policy identity (on macOS the
        // temp dir's /var is /private/var), so build the expected grant the
        // same way.
        let grant = AccessGrant {
            path: crate::policy_path::policy_path(&dir.path().join("outside")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        };
        let raw = serde_json::json!([{"path": grant.path, "mode": "write"}]);
        {
            let conn = crate::db::open(&path).unwrap();
            register_scope(&conn, &key, Some(&scope("head", 1)), &Config::default()).unwrap();
            materialize(&conn, &bind, &raw, &[launch_grant(&grant)]).unwrap();
            assert_eq!(list(&conn, &key).unwrap(), vec![grant.clone()]);
            assert_eq!(revoke(&conn, &key, &grant.path).unwrap(), 1);
        }
        std::fs::create_dir(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        let conn = crate::db::open(&path).unwrap();
        assert_eq!(launch_params(&conn, &bind).unwrap(), Some(raw.clone()));
        materialize(&conn, &bind, &raw, &[launch_grant(&grant)]).unwrap();
        let mut rebound = bind.clone();
        rebound.session = "rebound".into();
        materialize(&conn, &rebound, &raw, &[launch_grant(&grant)]).unwrap();
        assert!(list(&conn, &key).unwrap().is_empty());
        let mut access = crate::access_policy::AccessPolicy::from_config(
            &Config::default(),
            &dir.path().join("store"),
            vec![root],
            &dir.path().join("home"),
        );
        access.grants = list(&conn, &key).unwrap();
        assert_eq!(
            access.decide(AccessOperation::Write, &grant.path, false),
            crate::access_policy::AccessDecision::Ask
        );
    }
    #[test]
    fn grants_failed_install_rolls_back_params_and_materialization() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let scope = scope("worker", 7);
        register_scope(&conn, &scope, None, &Config::default()).unwrap();
        let bind = policy(&scope, dir.path());
        let grant = AccessGrant {
            path: crate::policy_path::policy_path(&dir.path().join("future")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let raw = serde_json::json!([{"path":grant.path,"mode":"read"}]);
        conn.execute_batch("CREATE TRIGGER fail_install BEFORE INSERT ON access_grants BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(materialize(&conn, &bind, &raw, &[launch_grant(&grant)]).is_err());
        assert!(launch_params(&conn, &bind).unwrap().is_none());
        assert!(list(&conn, &scope).unwrap().is_empty());
        conn.execute_batch("DROP TRIGGER fail_install;").unwrap();
        materialize(&conn, &bind, &raw, &[launch_grant(&grant)]).unwrap();
        assert_eq!(list(&conn, &scope).unwrap(), vec![grant]);
    }

    #[test]
    fn grants_revived_launch_concurrent_first_calls_materialize_once() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Barrier,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        let bind = policy(&first, dir.path());
        let revived = policy(&second, dir.path());
        let grant = AccessGrant {
            path: crate::policy_path::policy_path(&dir.path().join("outside")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        };
        let conn = crate::db::open(&path).unwrap();
        for key in [&first, &second] {
            register_scope(&conn, key, None, &Config::default()).unwrap();
        }
        let raw = serde_json::json!([{"path":grant.path,"mode":"write"}]);
        materialize(&conn, &bind, &raw, &[launch_grant(&grant)]).unwrap();
        conn.execute_batch(
            "CREATE TABLE installs(n INTEGER); INSERT INTO installs VALUES(0);
            CREATE TRIGGER count_install BEFORE INSERT ON access_grants WHEN NEW.epoch='2'
            BEGIN UPDATE installs SET n=n+1; END;",
        )
        .unwrap();
        let handles = [
            crate::db::open(&path).unwrap(),
            crate::db::open(&path).unwrap(),
        ];
        let barrier = Arc::new(Barrier::new(2));
        let validations = Arc::new(AtomicUsize::new(0));
        let installed: Vec<LaunchRestore> = std::thread::scope(|threads| {
            let jobs: Vec<_> = handles
                .into_iter()
                .map(|conn| {
                    let barrier = barrier.clone();
                    let validations = validations.clone();
                    let revived = &revived;
                    let grant = &grant;
                    let raw = &raw;
                    threads.spawn(move || {
                        barrier.wait();
                        restore_launch(&conn, revived, |_, params| {
                            assert_eq!(params, raw, "only stored launch inputs may be validated");
                            validations.fetch_add(1, Ordering::SeqCst);
                            Ok(vec![launch_grant(&grant)])
                        })
                        .unwrap()
                    })
                })
                .collect();
            jobs.into_iter().map(|job| job.join().unwrap()).collect()
        });
        assert_eq!(
            installed
                .iter()
                .filter(|result| **result == LaunchRestore::Materialized)
                .count(),
            1
        );
        assert_eq!(
            installed
                .iter()
                .filter(|result| **result == LaunchRestore::AlreadyMaterialized)
                .count(),
            1
        );
        assert_eq!(validations.load(Ordering::SeqCst), 1);
        let installs: i64 = conn
            .query_row("SELECT n FROM installs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            installs, 1,
            "upserts must not hide duplicate materializations"
        );
        assert_eq!(list(&conn, &second).unwrap(), vec![grant.clone()]);
        assert_eq!(list(&conn, &first).unwrap(), vec![grant]);
        let markers: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_materialized", [], |r| r.get(0))
            .unwrap();
        assert_eq!(markers, 2);
    }

    #[test]
    fn grants_revived_launch_failed_install_rolls_back_carry_forward() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        for key in [&first, &second] {
            register_scope(&conn, key, None, &Config::default()).unwrap();
        }
        let grant = AccessGrant {
            // Grants use canonical policy identities, including temp-directory aliases.
            path: crate::policy_path::policy_path(
                &dir.path().join("nested").join("..").join("outside"),
            ),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let raw = serde_json::json!([{"path":grant.path,"mode":"read"}]);
        materialize(
            &conn,
            &policy(&first, dir.path()),
            &raw,
            &[launch_grant(&grant)],
        )
        .unwrap();
        let revived = policy(&second, dir.path());
        conn.execute_batch("CREATE TRIGGER fail_install BEFORE INSERT ON access_grants BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(restore_launch(&conn, &revived, |_, _| Ok(vec![launch_grant(&grant)])).is_err());
        for table in [
            "access_launch_params",
            "access_materialized",
            "access_grants",
        ] {
            let count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE epoch='2'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "failed restore must roll back {table}");
        }
        conn.execute_batch("DROP TRIGGER fail_install;").unwrap();
        assert_eq!(
            restore_launch(&conn, &revived, |_, _| Ok(vec![launch_grant(&grant)])).unwrap(),
            LaunchRestore::Materialized
        );
        assert_eq!(list(&conn, &second).unwrap(), vec![grant]);
    }

    #[test]
    fn grants_revived_launch_restoration_isolates_full_identity() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        register_scope(&conn, &first, None, &Config::default()).unwrap();
        materialize(
            &conn,
            &policy(&first, dir.path()),
            &serde_json::json!([]),
            &[],
        )
        .unwrap();
        for dimension in [
            "ref",
            "owner",
            "session",
            "preset",
            "principal",
            "harness",
            "root",
        ] {
            let mut key = policy(&scope("worker", 2), dir.path());
            match dimension {
                "ref" => key.scope_ref = "other".into(),
                "owner" => key.owner = "other".into(),
                "session" => key.session = "other".into(),
                "preset" => key.preset = "head".into(),
                "principal" => key.principal = "other".into(),
                "harness" => key.harness = "other".into(),
                _ => key.project_root = dir.path().join("other").display().to_string(),
            }
            register_scope(
                &conn,
                &ScopeKey::from_policy(&key),
                None,
                &Config::default(),
            )
            .unwrap();
            assert_eq!(
                restore_launch(&conn, &key, |_, _| panic!(
                    "crossed {dimension} identity boundary"
                ))
                .unwrap(),
                LaunchRestore::NoParams
            );
        }
    }

    #[test]
    fn grants_revived_worker_carries_launch_params_but_not_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        let head = scope("head", 1);
        for key in [&first, &second] {
            register_scope(&conn, key, Some(&head), &Config::default()).unwrap();
        }
        let bind = policy(&first, dir.path());
        let revived = policy(&second, dir.path());
        let grant = AccessGrant {
            path: crate::policy_path::policy_path(&dir.path().join("outside")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let raw = serde_json::json!([{"path":grant.path,"mode":"read"}]);
        materialize(&conn, &bind, &raw, &[launch_grant(&grant)]).unwrap();
        assert_eq!(launch_params(&conn, &revived).unwrap(), Some(raw.clone()));
        let copied: String = conn
            .query_row(
                "SELECT params FROM access_launch_params WHERE epoch='2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied, raw.to_string());
        assert_eq!(list(&conn, &first).unwrap(), vec![grant]);
        assert!(list(&conn, &second).unwrap().is_empty());
        let markers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_materialized WHERE epoch='2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(markers, 0);
        end_scope_grants(&conn, &first).unwrap();
        assert_eq!(launch_params(&conn, &revived).unwrap(), Some(raw));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_launch_params", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn grants_revived_launch_params_use_highest_older_epoch_and_isolate_identity() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        for key in [&first, &second] {
            register_scope(&conn, key, None, &Config::default()).unwrap();
        }
        materialize(
            &conn,
            &policy(&first, dir.path()),
            &serde_json::json!(["first"]),
            &[],
        )
        .unwrap();
        let newest = serde_json::json!(["second"]);
        materialize(&conn, &policy(&second, dir.path()), &newest, &[]).unwrap();
        let revived = policy(&scope("worker", 3), dir.path());
        assert_eq!(launch_params(&conn, &revived).unwrap(), Some(newest));
        for different in 0..8 {
            let mut other = revived.clone();
            match different {
                0 => other.epoch = 0,
                1 => other.principal = "direct".into(),
                2 => other.owner = "other".into(),
                3 => other.scope_ref = "other".into(),
                4 => other.harness = "other".into(),
                5 => other.project_root = "/other".into(),
                6 => other.preset = "head".into(),
                _ => other.session = "two".into(),
            }
            assert!(
                launch_params(&conn, &other).unwrap().is_none(),
                "key dimension {different}"
            );
        }
    }

    #[test]
    fn grants_scope_authorization_and_end_are_epoch_bound() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let head = scope("head", 1);
        let worker = scope("worker", 2);
        let sibling = scope("sibling", 2);
        for (key, parent) in [(&head, None), (&worker, Some(&head)), (&sibling, None)] {
            register_scope(&conn, key, parent, &Config::default()).unwrap();
        }
        assert!(authorized(&conn, &head, &head).unwrap());
        assert!(authorized(&conn, &head, &worker).unwrap());
        assert!(!authorized(&conn, &worker, &head).unwrap());
        assert!(!authorized(&conn, &worker, &sibling).unwrap());
        let grant = AccessGrant {
            path: dir.path().join("future"),
            kind: GrantKind::File,
            operation: AccessOperation::Write,
        };
        insert(&conn, &worker, &grant).unwrap();
        assert!(list(&conn, &scope("worker", 3)).unwrap().is_empty());
        end_scope_grants(&conn, &worker).unwrap();
        assert!(list(&conn, &worker).unwrap().is_empty());
        register_scope(&conn, &worker, Some(&head), &Config::default()).unwrap();
        insert(&conn, &worker, &grant).unwrap();
        assert!(list(&conn, &worker).unwrap().is_empty());
        assert!(!authorized(&conn, &head, &worker).unwrap());
    }

    /// Rows a scope end leaves for the ended epoch, the launch parameters a
    /// later epoch of the same scope would receive, the ledger state of each
    /// call, and the late-results entries the calls' runner reads afterwards
    /// as (call key, kind, reason).
    type EndOutcome = (
        Vec<(&'static str, i64)>,
        Option<serde_json::Value>,
        Vec<(&'static str, crate::db::call_ledger::State)>,
        Vec<(String, String, String)>,
    );

    /// How the live scope-end handler identifies the scope in the call ledger.
    fn live_ledger_scope() -> crate::db::call_ledger::ScopeIdentity {
        crate::db::call_ledger::ScopeIdentity {
            owner: "reserved:prefrontal".into(),
            scope_ref: "worker".into(),
            scope_epoch: 5,
        }
    }

    fn end_with(end: impl Fn(&Connection, &ScopeKey)) -> EndOutcome {
        use crate::access_policy::ApprovalMode;
        use subc_protocol::Principal;
        let owner = owner_key(&Principal::Reserved {
            module_id: "prefrontal".into(),
        });
        let policy = |epoch| PolicyKey {
            project_root: "/project".into(),
            harness: "opencode".into(),
            session: "session".into(),
            principal: "principal".into(),
            owner: owner.clone(),
            scope_ref: "worker".into(),
            epoch,
            preset: "default".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let scope = ScopeKey::from_policy(&policy(5));
        register_scope(&conn, &scope, None, &Config::default()).unwrap();
        register_worker(&conn, &scope).unwrap();
        set_mode(&conn, &scope, ApprovalMode::Ask).unwrap();
        let outside = crate::policy_path::policy_path(dir.path()).join("outside");
        let grant = AccessGrant {
            path: outside.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        materialize(
            &conn,
            &policy(5),
            &serde_json::json!([{"path": outside, "mode": "read"}]),
            &[(grant, None)],
        )
        .unwrap();
        use crate::db::call_ledger::{self as ledger, Key, State};
        const CARRIER: &str = "reserved:runner";
        let calls = [
            ("prepared", State::Prepared),
            ("authorized", State::Authorized),
            ("started", State::DispatchStarted),
        ];
        for (call_key, state) in calls {
            let key = Key {
                carrier: CARRIER.into(),
                call_key: call_key.into(),
            };
            ledger::admit(&conn, &key, "digest", Some(&live_ledger_scope()), state).unwrap();
        }
        end(&conn, &scope);
        let states = calls
            .map(|(call_key, _)| {
                let key = Key {
                    carrier: CARRIER.into(),
                    call_key: call_key.into(),
                };
                (call_key, ledger::get(&conn, &key).unwrap().unwrap().state)
            })
            .to_vec();
        let page = ledger::tests::page(&conn, CARRIER, None, None, ledger::now_ms(), 1 << 20);
        let mut entries: Vec<(String, String, String)> = page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                let field = |name: &str| entry[name].as_str().unwrap_or_default().to_string();
                (field("call_key"), field("kind"), field("reason"))
            })
            .collect();
        entries.sort();
        let left = [
            "access_grants",
            "access_launch_params",
            "access_materialized",
            "access_modes",
            "access_scope_agents",
        ]
        .map(|table| {
            let count = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE scope_ref='worker' AND epoch='5'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            (table, count)
        })
        .to_vec();
        register_scope(
            &conn,
            &ScopeKey::from_policy(&policy(6)),
            None,
            &Config::default(),
        )
        .unwrap();
        (
            left,
            launch_params(&conn, &policy(6)).unwrap(),
            states,
            entries,
        )
    }

    #[test]
    fn sweep_scope_end_cleans_the_same_rows_as_the_live_scope_end() {
        // The live handler's database steps, in its order.
        let live = end_with(|conn, key| {
            end_scope_grants(conn, key).unwrap();
            crate::db::codemode_owners::end(conn, key).unwrap();
            crate::db::call_ledger::end_scope(
                conn,
                &live_ledger_scope(),
                crate::db::call_ledger::now_ms(),
            )
            .unwrap();
        });
        let sweep = end_with(|conn, key| {
            end_saved_scope(conn, key).unwrap();
        });
        assert!(live.0.iter().all(|(_, count)| *count == 0), "{live:?}");
        assert_eq!(live.1, None);
        use crate::db::call_ledger::State;
        assert_eq!(
            live.2,
            [
                ("prepared", State::Settled),
                ("authorized", State::Settled),
                ("started", State::DispatchStarted),
            ]
        );
        let settled = |call_key: &str| {
            (
                call_key.to_string(),
                "not_started".to_string(),
                "scope_ended".to_string(),
            )
        };
        // The runner's late_results read returns both never-started calls as
        // settled by the scope end, and nothing for the call already running.
        assert_eq!(
            live.3
                .iter()
                .filter(|(_, kind, _)| kind == "not_started")
                .cloned()
                .collect::<Vec<_>>(),
            [settled("authorized"), settled("prepared")]
        );
        assert!(live.3.iter().all(|(call_key, _, _)| call_key != "started"));
        assert_eq!(sweep, live);
    }

    #[test]
    fn ended_scope_tombstone_drops_its_configuration() {
        for sweep in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
            let key = ScopeKey {
                owner: "owner".into(),
                scope_ref: "run".into(),
                epoch: 3,
            };
            register_scope(&conn, &key, None, &Config::default()).unwrap();
            if sweep {
                end_saved_scope(&conn, &key).unwrap();
            } else {
                end_scope_grants(&conn, &key).unwrap();
            }
            let (config, ended): (String, i64) = conn
                .query_row(
                    "SELECT config, ended FROM access_scopes WHERE owner='owner' AND scope_ref='run' AND epoch='3'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((config.as_str(), ended), ("{}", 1), "sweep={sweep}");
            assert!(scope_ended(&conn, &key).unwrap());
            // A late registration of the ended identity cannot revive it.
            register_scope(&conn, &key, None, &Config::default()).unwrap();
            assert!(scope_config(&conn, &key).unwrap().is_none());
        }
    }
}
