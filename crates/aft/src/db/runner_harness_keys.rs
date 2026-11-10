//! Preserve old harness-keyed rows while making their canonical Runner copies.

use rusqlite::{Connection, Transaction, TransactionBehavior};

const SOURCE_LIMIT: usize = 65_536;

pub(super) fn copy_aliases(conn: &Connection) -> rusqlite::Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut copied = 0;
    for (table, value_columns) in [
        ("remote_exec_policies", "params,last_used"),
        ("catalog_edit_modes", "hashline,recorded_at"),
    ] {
        let mut statement = tx.prepare(&format!("SELECT rowid FROM {table} WHERE harness IN ('opencode2','claude-code','broca') ORDER BY rowid LIMIT ?1"))?;
        let mut rows = statement.query([(SOURCE_LIMIT + 1) as i64])?;
        let mut sources = Vec::new();
        while let Some(row) = rows.next()? {
            if sources.len() == SOURCE_LIMIT {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::remote_exec::PolicyKey;

    #[test]
    fn runner_harness_value_copy_preserves_original_rows_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        let frozen = crate::exec_remote::FrozenParams {
            host: Some("broca".into()),
            siblings: vec!["/sibling".into()],
            ..Default::default()
        };
        for (index, alias) in ["opencode2", "claude-code", "broca"]
            .into_iter()
            .enumerate()
        {
            let session = format!("session-{index}");
            let key = PolicyKey {
                project_root: "/work".into(),
                harness: alias.into(),
                session,
                principal: "reserved:broca".into(),
                owner: "reserved:prefrontal-core".into(),
                scope_ref: "worker-7".into(),
                epoch: 3,
                preset: "worker".into(),
            };
            crate::db::remote_exec::freeze(&conn, &key, &frozen, 17, || {}).unwrap();
            crate::db::catalog_edit_modes::record(&conn, &key, true, 17).unwrap();
        }
        assert_eq!(copy_aliases(&conn).unwrap(), 6);
        assert_eq!(copy_aliases(&conn).unwrap(), 0);
        for (index, alias) in ["opencode2", "claude-code", "broca"]
            .into_iter()
            .enumerate()
        {
            let key = PolicyKey {
                project_root: "/work".into(),
                harness: "runner".into(),
                session: format!("session-{index}"),
                principal: "reserved:broca".into(),
                owner: "reserved:prefrontal-core".into(),
                scope_ref: "worker-7".into(),
                epoch: 3,
                preset: "worker".into(),
            };
            assert_eq!(
                crate::db::catalog_edit_modes::lookup(&conn, &key).unwrap(),
                Some(true)
            );
            let params = crate::db::remote_exec::lookup(&conn, &key, 18)
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::to_value(params).unwrap(),
                serde_json::to_value(&frozen).unwrap()
            );
            for table in ["catalog_edit_modes", "remote_exec_policies"] {
                let raw: i64 = conn
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE harness=?1 AND session=?2"),
                        rusqlite::params![alias, key.session],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(raw, 1);
            }
        }
    }
}
