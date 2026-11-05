//! The edit mode each runner head's catalog was rendered for, stored in aft.db.
//!
//! A runner head fetches its catalog once and keeps using it across AFT
//! restarts, route closes and rebinds, so the grammar its `edit` calls are
//! decoded with has to outlive the process that rendered the catalog. A row
//! uses the same key as the other per-preset catalog params (bind identity,
//! scope, epoch and preset) and is deleted only when its scope ends.
//!
//! Rollback to schema 25 requires stopping AFT, dropping catalog_edit_modes,
//! then setting schema_version.version to 25. Runner heads then decode `edit`
//! with the standard grammar until they fetch their catalog again.

use rusqlite::{params, Connection, OptionalExtension};

use super::remote_exec::PolicyKey;

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS catalog_edit_modes (
  project_root TEXT NOT NULL, harness TEXT NOT NULL, session TEXT NOT NULL,
  principal TEXT NOT NULL, owner TEXT NOT NULL, scope_ref TEXT NOT NULL,
  epoch TEXT NOT NULL, preset TEXT NOT NULL,
  hashline INTEGER NOT NULL CHECK (hashline IN (0, 1)),
  recorded_at INTEGER NOT NULL,
  PRIMARY KEY (project_root, harness, session, principal, owner, scope_ref, epoch, preset)
);
"#;

const IDENTITY: &str = "project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
    AND owner=?5 AND scope_ref=?6 AND preset=?7";

/// Record whether the catalog just served for `key` takes hashline patches in
/// `edit`. A later fetch for the same key replaces the earlier answer.
pub fn record(
    conn: &Connection,
    key: &PolicyKey,
    hashline: bool,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO catalog_edit_modes
         (project_root,harness,session,principal,owner,scope_ref,epoch,preset,hashline,recorded_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(project_root,harness,session,principal,owner,scope_ref,epoch,preset)
         DO UPDATE SET hashline=excluded.hashline, recorded_at=excluded.recorded_at",
        params![
            key.project_root,
            key.harness,
            key.session,
            key.principal,
            key.owner,
            key.scope_ref,
            key.epoch.to_string(),
            key.preset,
            hashline,
            now_ms
        ],
    )?;
    Ok(())
}

/// The recorded edit mode for `key`: `Some(true)` for hashline, `Some(false)`
/// for the standard grammar, `None` when no catalog was recorded for it.
///
/// A new epoch of the same scope is a rebind of the same head, which keeps the
/// catalog it already holds. When this epoch has no row of its own, the newest
/// earlier epoch's answer is copied into it, so the answer survives that
/// earlier epoch's scope end.
pub fn lookup(conn: &Connection, key: &PolicyKey) -> rusqlite::Result<Option<bool>> {
    let epoch = key.epoch.to_string();
    let values = params![
        key.project_root,
        key.harness,
        key.session,
        key.principal,
        key.owner,
        key.scope_ref,
        key.preset,
        epoch
    ];
    let exact: Option<bool> = conn
        .query_row(
            &format!("SELECT hashline FROM catalog_edit_modes WHERE {IDENTITY} AND epoch=?8"),
            values,
            |row| row.get(0),
        )
        .optional()?;
    if exact.is_some() {
        return Ok(exact);
    }
    // Epochs are canonical unsigned decimal text. Length then lexical ordering
    // preserves the full u64 range, as in `remote_exec::catalog_params`.
    let older: Option<(i64, bool)> = conn
        .query_row(
            &format!(
                "SELECT rowid,hashline FROM catalog_edit_modes WHERE {IDENTITY}
                 AND (length(epoch)<length(?8) OR (length(epoch)=length(?8) AND epoch<?8))
                 ORDER BY length(epoch) DESC,epoch DESC LIMIT 1"
            ),
            values,
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((rowid, hashline)) = older else {
        return Ok(None);
    };
    conn.execute(
        "INSERT OR IGNORE INTO catalog_edit_modes
         (project_root,harness,session,principal,owner,scope_ref,epoch,preset,hashline,recorded_at)
         SELECT project_root,harness,session,principal,owner,scope_ref,?1,preset,hashline,recorded_at
         FROM catalog_edit_modes WHERE rowid=?2",
        params![epoch, rowid],
    )?;
    Ok(Some(hashline))
}

/// Delete the answers recorded under an ended scope. Only the scope-end path
/// calls this: a route close or detach keeps them, because the head keeps its
/// catalog across those.
pub fn forget_scope(
    conn: &Connection,
    owner: &str,
    scope_ref: &str,
    epoch: u64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM catalog_edit_modes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
        params![owner, scope_ref, epoch.to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(root: &std::path::Path, epoch: u64) -> PolicyKey {
        PolicyKey {
            project_root: root.display().to_string(),
            harness: "runner".into(),
            session: "head".into(),
            principal: "reserved:broca".into(),
            owner: "prefrontal-core".into(),
            scope_ref: "head-scope".into(),
            epoch,
            preset: "head".into(),
        }
    }

    #[test]
    fn edit_modes_survive_a_reopen_and_carry_into_a_later_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        {
            let conn = crate::db::open(&path).unwrap();
            assert_eq!(lookup(&conn, &key(dir.path(), 3)).unwrap(), None);
            record(&conn, &key(dir.path(), 3), true, 10).unwrap();
        }
        let conn = crate::db::open(&path).unwrap();
        assert_eq!(lookup(&conn, &key(dir.path(), 3)).unwrap(), Some(true));
        // A rebind under a later epoch keeps the answer, also once the earlier
        // epoch's scope has ended.
        assert_eq!(lookup(&conn, &key(dir.path(), 4)).unwrap(), Some(true));
        forget_scope(&conn, "prefrontal-core", "head-scope", 3).unwrap();
        assert_eq!(lookup(&conn, &key(dir.path(), 3)).unwrap(), None);
        assert_eq!(lookup(&conn, &key(dir.path(), 4)).unwrap(), Some(true));
        // A later fetch replaces the answer; an earlier epoch is not consulted
        // once this epoch has its own.
        record(&conn, &key(dir.path(), 4), false, 20).unwrap();
        assert_eq!(lookup(&conn, &key(dir.path(), 4)).unwrap(), Some(false));
        let mut other = key(dir.path(), 4);
        other.session = "other".into();
        assert_eq!(lookup(&conn, &other).unwrap(), None);
    }

    #[test]
    fn ending_a_scope_forgets_its_edit_modes() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        record(&conn, &key(dir.path(), 5), true, 10).unwrap();
        crate::db::access_grants::end_scope_grants(
            &conn,
            &crate::db::access_grants::ScopeKey::from_policy(&key(dir.path(), 5)),
        )
        .unwrap();
        assert_eq!(lookup(&conn, &key(dir.path(), 5)).unwrap(), None);
    }
}
