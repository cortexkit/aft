//! Preserve old harness-keyed rows while making their canonical Runner copies.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

const SOURCE_LIMIT: usize = 65_536;

#[derive(Clone, Copy)]
pub(super) enum AliasTable {
    RemoteExec,
    EditModes,
}

impl AliasTable {
    fn fields(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::RemoteExec => ("remote_exec_policies", "params", "last_used"),
            Self::EditModes => ("catalog_edit_modes", "hashline", "recorded_at"),
        }
    }
}

/// Read preserved alias rows only when the caller found no canonical row.
/// A reconnect that advances only the scope epoch keeps its saved catalog.
/// Prefer the requested epoch, then the newest earlier one; within that epoch
/// the latest timestamp wins, then opencode2, claude-code, and broca.
pub(super) fn lookup_alias<T: rusqlite::types::FromSql>(
    conn: &Connection,
    key: &super::remote_exec::PolicyKey,
    table: AliasTable,
) -> rusqlite::Result<Option<T>> {
    if key.harness != "runner" {
        return Ok(None);
    }
    let (table_name, value, timestamp) = table.fields();
    let epoch = key.epoch.to_string();
    let row: Option<(T, String)> = conn.query_row(&format!(
        "SELECT {value},harness FROM {table_name}
         WHERE project_root=?1 AND session=?2 AND principal=?3 AND owner=?4
         AND scope_ref=?5 AND preset=?6 AND harness IN ('opencode2','claude-code','broca')
         AND (epoch=?7 OR length(epoch)<length(?7) OR (length(epoch)=length(?7) AND epoch<?7))
         ORDER BY epoch=?7 DESC,length(epoch) DESC,epoch DESC,{timestamp} DESC,
         CASE harness WHEN 'opencode2' THEN 0 WHEN 'claude-code' THEN 1 ELSE 2 END,rowid DESC LIMIT 1"
    ), params![key.project_root, key.session, key.principal, key.owner, key.scope_ref, key.preset, epoch],
        |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
    if let Some((value, alias)) = row {
        static REMOTE: std::sync::Once = std::sync::Once::new();
        static EDIT: std::sync::Once = std::sync::Once::new();
        let once = match table {
            AliasTable::RemoteExec => &REMOTE,
            AliasTable::EditModes => &EDIT,
        };
        once.call_once(|| log::debug!("canonical runner harness lookup used preserved alias: table={table_name} harness={alias}"));
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

pub(super) fn copy_aliases(conn: &Connection) -> rusqlite::Result<usize> {
    copy_aliases_with_limit(conn, SOURCE_LIMIT)
}

fn copy_aliases_with_limit(conn: &Connection, source_limit: usize) -> rusqlite::Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut copied = 0;
    for table in [AliasTable::RemoteExec, AliasTable::EditModes] {
        let (table, value, timestamp) = table.fields();
        let value_columns = format!("{value},{timestamp}");
        let mut statement = tx.prepare(&format!("SELECT rowid FROM {table} WHERE harness IN ('opencode2','claude-code','broca') ORDER BY {timestamp} DESC,CASE harness WHEN 'opencode2' THEN 0 WHEN 'claude-code' THEN 1 ELSE 2 END,rowid DESC LIMIT ?1"))?;
        let mut rows = statement.query([(source_limit + 1) as i64])?;
        let mut sources = Vec::new();
        while let Some(row) = rows.next()? {
            if sources.len() == source_limit {
                return Err(rusqlite::Error::InvalidParameterName(
                    "runner harness copy exceeds source limit".into(),
                ));
            }
            sources.push(row.get::<_, i64>(0)?);
        }
        drop(rows);
        drop(statement);
        let columns = format!(
            "project_root,harness,session,principal,owner,scope_ref,epoch,preset,{value_columns}"
        );
        let selected = format!(
            "project_root,'runner',session,principal,owner,scope_ref,epoch,preset,{value_columns}"
        );
        let mut insert = tx.prepare(&format!("INSERT OR IGNORE INTO {table} ({columns}) SELECT {selected} FROM {table} WHERE rowid=?1"))?;
        for rowid in sources {
            copied += insert.execute([rowid])?;
        }
    }
    tx.commit()?;
    log::info!("canonical runner harness keys: copied={copied}; raw rows retained for rollback");
    Ok(copied)
}

pub(crate) fn initialize(conn: &Connection) {
    if let Err(error) = copy_aliases(conn) {
        log::warn!("canonical runner harness key copy skipped; raw rows remain readable: {error}");
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests {
    use super::*;

    pub fn canonical_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT (SELECT count(*) FROM remote_exec_policies WHERE harness='runner') + (SELECT count(*) FROM catalog_edit_modes WHERE harness='runner')", [], |row| row.get(0)).unwrap()
    }
}
