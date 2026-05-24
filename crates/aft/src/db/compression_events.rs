use rusqlite::{params, Connection};

pub struct CompressionEventRow<'a> {
    pub harness: &'a str,
    pub session_id: Option<&'a str>,
    pub project_key: &'a str,
    pub tool: &'a str,
    pub task_id: Option<&'a str>,
    pub command: Option<&'a str>,
    pub compressor: &'a str,
    pub original_bytes: i64,
    pub compressed_bytes: i64,
    pub original_tokens: u32,
    pub compressed_tokens: u32,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CompressionAggregate {
    pub events: u64,
    pub original_tokens: u64,
    pub compressed_tokens: u64,
}

impl CompressionAggregate {
    pub fn savings_tokens(&self) -> u64 {
        self.original_tokens.saturating_sub(self.compressed_tokens)
    }
}

pub fn insert_compression_event(
    conn: &Connection,
    row: &CompressionEventRow<'_>,
) -> rusqlite::Result<()> {
    conn.execute(
        r#"
        INSERT INTO compression_events (
            harness, session_id, project_key, tool, task_id, command, compressor,
            original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at
        )
        SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
        WHERE NOT EXISTS (
            SELECT 1
            FROM compression_events
            WHERE harness = ?1
              AND session_id IS ?2
              AND task_id IS ?5
              AND tool = ?4
            LIMIT 1
        )
        "#,
        params![
            row.harness,
            row.session_id,
            row.project_key,
            row.tool,
            row.task_id,
            row.command,
            row.compressor,
            row.original_bytes,
            row.compressed_bytes,
            row.original_tokens,
            row.compressed_tokens,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn aggregate_for_project(
    conn: &Connection,
    harness: &str,
    project_key: &str,
) -> rusqlite::Result<CompressionAggregate> {
    conn.query_row(
        r#"
        SELECT
            COUNT(*) AS events,
            COALESCE(SUM(original_tokens), 0) AS original,
            COALESCE(SUM(compressed_tokens), 0) AS compressed
        FROM compression_events
        WHERE harness = ?1 AND project_key = ?2
        "#,
        params![harness, project_key],
        |row| {
            Ok(CompressionAggregate {
                events: row.get::<_, i64>(0)? as u64,
                original_tokens: row.get::<_, i64>(1)? as u64,
                compressed_tokens: row.get::<_, i64>(2)? as u64,
            })
        },
    )
}

pub fn aggregate_for_session(
    conn: &Connection,
    harness: &str,
    project_key: &str,
    session_id: &str,
) -> rusqlite::Result<CompressionAggregate> {
    conn.query_row(
        r#"
        SELECT
            COUNT(*) AS events,
            COALESCE(SUM(original_tokens), 0) AS original,
            COALESCE(SUM(compressed_tokens), 0) AS compressed
        FROM compression_events
        WHERE harness = ?1 AND project_key = ?2 AND session_id = ?3
        "#,
        params![harness, project_key, session_id],
        |row| {
            Ok(CompressionAggregate {
                events: row.get::<_, i64>(0)? as u64,
                original_tokens: row.get::<_, i64>(1)? as u64,
                compressed_tokens: row.get::<_, i64>(2)? as u64,
            })
        },
    )
}
