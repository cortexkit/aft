use rusqlite::{params, Connection, OptionalExtension};

pub fn get_harness_state(
    conn: &Connection,
    harness: &str,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM harness_state WHERE harness = ?1 AND key = ?2",
        params![harness, key],
        |row| row.get(0),
    )
    .optional()
}

pub fn set_harness_state(
    conn: &Connection,
    harness: &str,
    key: &str,
    value: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO harness_state (harness, key, value, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(harness, key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at",
        params![harness, key, value, now_ms],
    )?;
    Ok(())
}

pub fn get_host_state(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM host_state WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
}

pub fn set_host_state(
    conn: &Connection,
    key: &str,
    value: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO host_state (key, value, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
            value = excluded.value,
            updated_at = excluded.updated_at",
        params![key, value, now_ms],
    )?;
    Ok(())
}
