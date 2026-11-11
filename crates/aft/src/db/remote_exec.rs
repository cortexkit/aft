//! Durable catalog routing rows. Callers decode them into domain values.

use rusqlite::{params, Connection, OptionalExtension, Transaction};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PolicyKey {
    pub project_root: String,
    pub harness: String,
    pub session: String,
    pub principal: String,
    pub owner: String,
    pub scope_ref: String,
    pub epoch: u64,
    pub preset: String,
}

impl PolicyKey {
    pub(crate) fn values(&self) -> [&str; 7] {
        [
            &self.project_root,
            &self.harness,
            &self.session,
            &self.principal,
            &self.owner,
            &self.scope_ref,
            &self.preset,
        ]
    }
}

pub(crate) struct PolicyRow {
    pub(crate) params: String,
}

pub(crate) fn prior(conn: &Connection, key: &PolicyKey) -> rusqlite::Result<Option<PolicyRow>> {
    let v = key.values();
    let epoch = key.epoch.to_string();
    conn.query_row(
        "SELECT params FROM remote_exec_policies
             WHERE project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
             AND owner=?5 AND scope_ref=?6 AND preset=?7
             AND (length(epoch)<length(?8) OR (length(epoch)=length(?8) AND epoch<?8))
             ORDER BY length(epoch) DESC,epoch DESC LIMIT 1",
        params![v[0], v[1], v[2], v[3], v[4], v[5], v[6], epoch],
        |row| {
            Ok(PolicyRow {
                params: row.get(0)?,
            })
        },
    )
    .optional()
}

pub(crate) fn insert(
    conn: &Connection,
    key: &PolicyKey,
    row: &PolicyRow,
    now: u64,
) -> rusqlite::Result<()> {
    let v = key.values();
    let epoch = key.epoch.to_string();
    conn.execute(
        "INSERT OR IGNORE INTO remote_exec_policies
        (project_root,harness,session,principal,owner,scope_ref,preset,epoch,params,last_used)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![v[0], v[1], v[2], v[3], v[4], v[5], v[6], epoch, row.params, now as i64],
    )?;
    Ok(())
}

pub(crate) fn confine(conn: &Connection, key: &PolicyKey) -> rusqlite::Result<()> {
    let v = key.values();
    conn.execute(
        "UPDATE remote_exec_policies SET params=json_set(params,'$.confine','root')
             WHERE project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
             AND owner=?5 AND scope_ref=?6 AND preset=?7 AND epoch=?8",
        params![
            v[0],
            v[1],
            v[2],
            v[3],
            v[4],
            v[5],
            v[6],
            key.epoch.to_string()
        ],
    )?;
    Ok(())
}

pub(crate) fn lookup(
    tx: &Transaction<'_>,
    key: &PolicyKey,
    now: u64,
) -> rusqlite::Result<Option<PolicyRow>> {
    catalog_params(tx, key, CatalogParams::RemoteExec)?
        .map(|(rowid, json)| {
            tx.execute(
                "UPDATE remote_exec_policies SET last_used=?1 WHERE rowid=?2",
                params![now as i64, rowid],
            )?;
            Ok(PolicyRow { params: json })
        })
        .transpose()
}

pub(crate) fn current(conn: &Connection, key: &PolicyKey) -> rusqlite::Result<Option<PolicyRow>> {
    let v = key.values();
    let epoch = key.epoch.to_string();
    conn.query_row(
        "SELECT params FROM remote_exec_policies
             WHERE project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
             AND owner=?5 AND scope_ref=?6 AND preset=?7
             AND (epoch=?8 OR length(epoch)<length(?8)
                  OR (length(epoch)=length(?8) AND epoch<?8))
             ORDER BY epoch=?8 DESC, length(epoch) DESC, epoch DESC LIMIT 1",
        params![v[0], v[1], v[2], v[3], v[4], v[5], v[6], epoch],
        |row| {
            Ok(PolicyRow {
                params: row.get(0)?,
            })
        },
    )
    .optional()
}

pub(super) enum CatalogParams {
    RemoteExec,
    AccessLaunch,
}

/// A new epoch of the same scope is a rebound of the same worker. Carry saved
/// catalog inputs forward, but never grant rows or materialization markers.
pub(super) fn catalog_params(
    tx: &Transaction<'_>,
    key: &PolicyKey,
    table: CatalogParams,
) -> rusqlite::Result<Option<(i64, String)>> {
    let (table, extra_column) = match table {
        CatalogParams::RemoteExec => ("remote_exec_policies", ",last_used"),
        CatalogParams::AccessLaunch => ("access_launch_params", ""),
    };
    let v = key.values();
    let epoch = key.epoch.to_string();
    let identity = "project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
        AND owner=?5 AND scope_ref=?6 AND preset=?7";
    let exact_sql = format!("SELECT rowid,params FROM {table} WHERE {identity} AND epoch=?8");
    let exact = || {
        tx.query_row(
            &exact_sql,
            params![v[0], v[1], v[2], v[3], v[4], v[5], v[6], epoch],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
    };
    if let Some(row) = exact()? {
        return Ok(Some(row));
    }
    // Epochs are canonical unsigned decimal text. Length then lexical ordering
    // preserves the full u64 range, unlike SQLite's signed INTEGER cast.
    let older: Option<(i64, String)> = tx
        .query_row(
            &format!(
                "SELECT rowid,epoch FROM {table} WHERE {identity}
            AND (length(epoch)<length(?8) OR (length(epoch)=length(?8) AND epoch<?8))
            ORDER BY length(epoch) DESC,epoch DESC LIMIT 1"
            ),
            params![v[0], v[1], v[2], v[3], v[4], v[5], v[6], epoch],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((rowid, from_epoch)) = older else {
        return Ok(None);
    };
    let inserted = tx.execute(
        &format!("INSERT OR IGNORE INTO {table}
            (project_root,harness,session,principal,owner,scope_ref,preset,epoch,params{extra_column})
            SELECT project_root,harness,session,principal,owner,scope_ref,preset,?1,params{extra_column}
            FROM {table} WHERE rowid=?2"),
        params![epoch, rowid],
    )?;
    if inserted == 1 {
        log::info!(
            "catalog params carried forward: session={} scope_ref={:.12} from_epoch={} to_epoch={} preset={} table={}",
            key.session, key.scope_ref, from_epoch, key.epoch, key.preset, table
        );
    }
    exact()
}

/// Called by scope-end handling once it is available. Ordinary route detach
/// must not forget a policy: catalog and calls use different routes.
pub fn forget_scope(
    conn: &Connection,
    owner: &str,
    scope_ref: &str,
    epoch: u64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM remote_exec_policies WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
        params![owner, scope_ref, epoch.to_string()],
    )
}
