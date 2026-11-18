//! Remote folder grants are additive; older builds neither read nor write them.
use rusqlite::{params, Connection, OptionalExtension};

const DDL: &str = "
CREATE TABLE IF NOT EXISTS remote_folder_grants (
    id TEXT NOT NULL UNIQUE,
    pairing_id TEXT NOT NULL,
    caller_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    root_kind TEXT NOT NULL,
    root_key TEXT NOT NULL,
    folder TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode IN ('read', 'read_write')),
    label TEXT NOT NULL,
    granted_at_ms INTEGER NOT NULL,
    card_ref TEXT NOT NULL,
    PRIMARY KEY (pairing_id, caller_id, project_id, root_kind, root_key)
);";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    Read,
    ReadWrite,
}

impl Mode {
    pub fn covers(self, needed: Self) -> bool {
        self == Self::ReadWrite || self == needed
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub pairing_id: String,
    pub caller_id: String,
    pub project_id: String,
    pub root_kind: &'static str,
    pub root_key: String,
}

impl Key {
    fn params(&self) -> [&str; 5] {
        [
            &self.pairing_id,
            &self.caller_id,
            &self.project_id,
            self.root_kind,
            &self.root_key,
        ]
    }
}

pub struct Row {
    pub id: String,
    pub caller_id: String,
    pub folder: String,
    pub mode: Mode,
    pub label: String,
    pub granted_at_ms: i64,
    pub card_ref: String,
}

fn present(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='remote_folder_grants')",
        [],
        |row| row.get(0),
    )
}

fn mode(write: bool) -> Mode {
    if write {
        Mode::ReadWrite
    } else {
        Mode::Read
    }
}

#[allow(
    dead_code,
    reason = "read by remote authorization before dispatch is enabled"
)]
pub fn find(
    conn: &Connection,
    key: &Key,
    folder: &str,
    needed: Mode,
) -> rusqlite::Result<Option<String>> {
    if !present(conn)? {
        return Ok(None);
    }
    let row = conn
        .query_row(
            "SELECT id, folder, mode = 'read_write' FROM remote_folder_grants
         WHERE pairing_id=?1 AND caller_id=?2 AND project_id=?3 AND root_kind=?4 AND root_key=?5",
            key.params(),
            |row| Ok((row.get(0)?, row.get::<_, String>(1)?, mode(row.get(2)?))),
        )
        .optional()?;
    Ok(row.and_then(|(id, saved_folder, saved_mode)| {
        (saved_folder == folder && saved_mode.covers(needed)).then_some(id)
    }))
}

#[allow(
    dead_code,
    reason = "written by remote authorization before dispatch is enabled"
)]
pub fn save(
    conn: &Connection,
    key: &Key,
    folder: &str,
    mode: Mode,
    label: &str,
    card_ref: &str,
) -> rusqlite::Result<String> {
    conn.execute_batch(DDL)?;
    // A relocated folder inherits neither the authority nor the id of the old
    // grant. At the same path, refresh the row only for an actual upgrade.
    conn.query_row(
        "INSERT INTO remote_folder_grants AS saved
         (id, pairing_id, caller_id, project_id, root_kind, root_key, folder, mode, label, granted_at_ms, card_ref)
         VALUES ('aftrg_' || lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (pairing_id, caller_id, project_id, root_kind, root_key) DO UPDATE SET
             id=CASE WHEN saved.folder=excluded.folder THEN saved.id ELSE excluded.id END,
             folder=excluded.folder,
             mode=excluded.mode,
             label=excluded.label,
             granted_at_ms=excluded.granted_at_ms,
             card_ref=excluded.card_ref
         WHERE saved.folder<>excluded.folder OR (saved.mode='read' AND excluded.mode='read_write')
         RETURNING id",
        params![key.pairing_id, key.caller_id, key.project_id, key.root_kind, key.root_key,
            folder, mode.as_str(), label, crate::db::call_ledger::now_ms(), card_ref],
        |row| row.get(0),
    ).optional()?.map_or_else(|| {
        conn.query_row(
            "SELECT id FROM remote_folder_grants
             WHERE pairing_id=?1 AND caller_id=?2 AND project_id=?3 AND root_kind=?4 AND root_key=?5",
            key.params(),
            |row| row.get(0),
        )
    }, Ok)
}

pub fn list(conn: &Connection, caller_id: Option<&str>) -> rusqlite::Result<Vec<Row>> {
    if !present(conn)? {
        return Ok(Vec::new());
    }
    conn.prepare(
        "SELECT id, caller_id, folder, mode = 'read_write', label, granted_at_ms, card_ref
         FROM remote_folder_grants WHERE ?1 IS NULL OR caller_id=?1
         ORDER BY granted_at_ms, id",
    )?
    .query_map([caller_id], |row| {
        Ok(Row {
            id: row.get(0)?,
            caller_id: row.get(1)?,
            folder: row.get(2)?,
            mode: mode(row.get(3)?),
            label: row.get(4)?,
            granted_at_ms: row.get(5)?,
            card_ref: row.get(6)?,
        })
    })?
    .collect()
}

pub fn revoke(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    if !id.starts_with("aftrg_") || !present(conn)? {
        return Ok(false);
    }
    conn.execute("DELETE FROM remote_folder_grants WHERE id=?1", [id])
        .map(|count| count != 0)
}
