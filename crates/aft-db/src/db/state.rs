use rusqlite::{params, Connection, OptionalExtension};

pub(crate) const RUNNER_COMPLETION_DELIVERY_CUTOVER: &str = "runner_completion_delivery_cutover_ms";

pub(crate) fn runner_completion_delivery_cutover(
    conn: &Connection,
    now_ms: u64,
) -> rusqlite::Result<u64> {
    let value = match get_host_state(conn, RUNNER_COMPLETION_DELIVERY_CUTOVER)? {
        Some(value) => value,
        None => {
            // Keep the first recorded time even when another root attaches or
            // the daemon restarts; later completions must not become backlog.
            conn.execute(
                "INSERT INTO host_state (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO NOTHING",
                params![
                    RUNNER_COMPLETION_DELIVERY_CUTOVER,
                    now_ms.to_string(),
                    now_ms as i64
                ],
            )?;
            get_host_state(conn, RUNNER_COMPLETION_DELIVERY_CUTOVER)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?
        }
    };
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

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
