use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRow {
    pub backup_id: String,
    pub harness: String,
    pub session_id: String,
    pub project_key: String,
    pub op_id: Option<String>,
    pub order: u128,
    pub file_path: String,
    pub path_hash: String,
    pub backup_path: Option<String>,
    pub kind: String,
    pub description: String,
    pub created_at: i64,
    pub is_tombstone: bool,
    pub restore_meta: Option<String>,
}

pub fn upsert_backup(conn: &Connection, row: &BackupRow) -> rusqlite::Result<()> {
    delete_backup_for_order(
        conn,
        &row.harness,
        &row.session_id,
        &row.path_hash,
        row.order,
    )?;
    insert_backup(conn, row)
}

pub fn insert_backup(conn: &Connection, row: &BackupRow) -> rusqlite::Result<()> {
    let order_blob = row.order.to_be_bytes();
    conn.execute(
        "INSERT INTO backups (
            backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
            path_hash, backup_path, kind, description, created_at, is_tombstone, restore_meta
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12, ?13, ?14
         )",
        params![
            row.backup_id,
            row.harness,
            row.session_id,
            row.project_key,
            row.op_id,
            &order_blob[..],
            row.file_path,
            row.path_hash,
            row.backup_path,
            row.kind,
            row.description,
            row.created_at,
            row.is_tombstone,
            row.restore_meta,
        ],
    )?;

    Ok(())
}

pub fn delete_backup_for_order(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    path_hash: &str,
    order: u128,
) -> rusqlite::Result<usize> {
    let order_blob = order.to_be_bytes();
    conn.execute(
        "DELETE FROM backups
         WHERE harness = ?1 AND session_id = ?2 AND path_hash = ?3 AND order_blob = ?4",
        params![harness, session_id, path_hash, &order_blob[..]],
    )
}

pub fn get_latest_backup(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    path_hash: &str,
) -> rusqlite::Result<Option<BackupRow>> {
    conn.query_row(
        "SELECT backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
                path_hash, backup_path, kind, description, created_at, is_tombstone, restore_meta
         FROM backups
         WHERE harness = ?1 AND session_id = ?2 AND path_hash = ?3
         ORDER BY order_blob DESC
         LIMIT 1",
        params![harness, session_id, path_hash],
        map_backup_row,
    )
    .optional()
}

pub fn list_backups(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    path_hash: &str,
) -> rusqlite::Result<Vec<BackupRow>> {
    let mut stmt = conn.prepare(
        "SELECT backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
                path_hash, backup_path, kind, description, created_at, is_tombstone, restore_meta
         FROM backups
         WHERE harness = ?1 AND session_id = ?2 AND path_hash = ?3
         ORDER BY order_blob ASC",
    )?;

    let rows = stmt
        .query_map(params![harness, session_id, path_hash], map_backup_row)?
        .collect();
    rows
}

pub fn list_backups_by_op(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    op_id: &str,
) -> rusqlite::Result<Vec<BackupRow>> {
    let mut stmt = conn.prepare(
        "SELECT backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
                path_hash, backup_path, kind, description, created_at, is_tombstone, restore_meta
         FROM backups
         WHERE harness = ?1 AND session_id = ?2 AND op_id = ?3
         ORDER BY file_path ASC, order_blob ASC",
    )?;

    let rows = stmt
        .query_map(params![harness, session_id, op_id], map_backup_row)?
        .collect();
    rows
}

pub fn get_latest_operation_backup(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> rusqlite::Result<Option<BackupRow>> {
    conn.query_row(
        "SELECT backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
                path_hash, backup_path, kind, description, created_at, is_tombstone, restore_meta
         FROM backups
         WHERE harness = ?1 AND session_id = ?2 AND op_id IS NOT NULL
         ORDER BY order_blob DESC
         LIMIT 1",
        params![harness, session_id],
        map_backup_row,
    )
    .optional()
}

pub fn delete_backups_for_path(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    path_hash: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM backups WHERE harness = ?1 AND session_id = ?2 AND path_hash = ?3",
        params![harness, session_id, path_hash],
    )
}

/// One backup stack as the database records it: the rows that share one
/// `(harness, session, path_hash)` identity, summarized without their content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupStackSummary {
    pub harness: String,
    pub session_id: String,
    pub path_hash: String,
    pub file_path: String,
    pub rows: usize,
}

/// List every recorded backup stack, optionally narrowed to one harness and/or
/// one session. The purge command uses this to find stacks whose rows outlived
/// their files as well as ordinary ones.
pub fn list_backup_stacks(
    conn: &Connection,
    harness: Option<&str>,
    session_id: Option<&str>,
) -> rusqlite::Result<Vec<BackupStackSummary>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, path_hash, MIN(file_path), COUNT(*)
         FROM backups
         WHERE (?1 IS NULL OR harness = ?1) AND (?2 IS NULL OR session_id = ?2)
         GROUP BY harness, session_id, path_hash
         ORDER BY harness, session_id, path_hash",
    )?;
    let rows = stmt
        .query_map(params![harness, session_id], |row| {
            Ok(BackupStackSummary {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                path_hash: row.get(2)?,
                file_path: row.get(3)?,
                rows: row.get::<_, i64>(4)?.max(0) as usize,
            })
        })?
        .collect();
    rows
}

/// Distinct harness segments that have at least one backup row.
pub fn list_backup_harnesses(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT harness FROM backups ORDER BY harness")?;
    let rows = stmt.query_map([], |row| row.get(0))?.collect();
    rows
}

/// Number of rows recorded for one `(harness, session, path_hash)` stack.
pub fn count_backups_for_path(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    path_hash: &str,
) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT COUNT(*) FROM backups WHERE harness = ?1 AND session_id = ?2 AND path_hash = ?3",
        params![harness, session_id, path_hash],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count.max(0) as usize)
}

fn map_backup_row(row: &Row<'_>) -> rusqlite::Result<BackupRow> {
    let order_blob: Vec<u8> = row.get(5)?;
    let order = order_from_blob(&order_blob).unwrap_or_default();
    Ok(BackupRow {
        backup_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
        harness: row.get(1)?,
        session_id: row.get(2)?,
        project_key: row.get(3)?,
        op_id: row.get(4)?,
        order,
        file_path: row.get(6)?,
        path_hash: row.get(7)?,
        backup_path: row.get(8)?,
        kind: row.get(9)?,
        description: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
        created_at: row.get(11)?,
        is_tombstone: row.get::<_, i64>(12)? != 0,
        restore_meta: row.get(13)?,
    })
}

fn order_from_blob(blob: &[u8]) -> Option<u128> {
    let bytes: [u8; 16] = blob.try_into().ok()?;
    Some(u128::from_be_bytes(bytes))
}
