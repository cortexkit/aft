use super::*;

pub(super) fn list_delivery_gaps(
    conn: &rusqlite::Connection,
    harness: &str,
    session: Option<&str>,
) -> rusqlite::Result<Vec<BashPatternWatchRow>> {
    crate::db::bash_watches::list_delivery_gaps(conn, harness, session).map(|gaps| {
        gaps.into_iter()
            .map(|(mut row, batch)| {
                row.match_context = Some(crate::task_watch_lines::format_watch_line_batch(
                    &row.task_id,
                    &batch,
                ));
                row
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_delivery_gap_keeps_rendered_context() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let saved = r#"{"version":17,"lines":[{"stream":"stderr","offset":23,"end_offset":37,"text":"saved warning","count":2}],"dropped_lines":0,"first_dropped_offset":null,"first_dropped_stream":null,"skipped_long_lines":0}"#;
        conn.execute(
        "INSERT INTO bash_watch_delivery_gaps(harness,session_id,task_id,watch_id,batch) VALUES('opencode','session','task','watch',?1)",
        [saved],
    ).unwrap();
        let rows = list_delivery_gaps(&conn, "opencode", Some("session")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].match_context.as_deref(),
            Some("stderr byte 23 ×2: saved warning")
        );
        assert_eq!(rows[0].match_batch.as_deref(), Some(saved));
        assert_eq!(rows[0].match_offset, Some(17));
        assert_eq!(rows[0].match_text.as_deref(), Some("watch delivery gap"));
    }
}
