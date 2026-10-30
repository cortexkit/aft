//! Exact call-key lookup for read-only command views. No task-owner fallbacks.

use rusqlite::{params, Connection};

/// Filter identity before limiting: unrelated requesters must neither introduce
/// ambiguity nor hide a duplicate in the requested session.
pub(crate) fn resolve(
    conn: &Connection,
    harness: &str,
    session: &str,
    requester: &str,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    let mut statement = conn.prepare(
        "SELECT task_id FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2
           AND json_extract(metadata, '$.call_key.requester') = ?3
           AND json_extract(metadata, '$.call_key.key') = ?4
           AND COALESCE(json_extract(metadata, '$.call_key.minted'), 0) = 0
         LIMIT 2",
    )?;
    let mut rows = statement.query(params![harness, session, requester, key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let task_id = row.get(0)?;
    Ok(rows.next()?.is_none().then_some(task_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn viewed_task_filters_requester_and_non_minted_before_duplicate_limit_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        for reverse in [false, true] {
            for own_count in 0..=2 {
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch(
                    "DROP TABLE IF EXISTS bash_tasks; CREATE TABLE bash_tasks (
                    harness TEXT, session_id TEXT, task_id TEXT, metadata TEXT);",
                )
                .unwrap();
                let mut rows = vec![
                    ("foreign-1", "foreign", false),
                    ("foreign-2", "foreign", false),
                    ("minted", "own", true),
                ];
                if own_count > 0 {
                    rows.push(("own-1", "own", false));
                }
                if own_count > 1 {
                    rows.push(("own-2", "own", false));
                }
                if reverse {
                    rows.reverse();
                }
                let harness: crate::harness::Harness = "mcp:client/raw label".parse().unwrap();
                for (task, requester, minted) in rows {
                    let mut call = json!({"requester":requester,"key":"viewed"});
                    if minted {
                        call["minted"] = true.into();
                    }
                    conn.execute(
                        "INSERT INTO bash_tasks VALUES (?1, ?2, ?3, ?4)",
                        params![
                            harness.storage_segment(),
                            "session",
                            task,
                            json!({"call_key":call}).to_string()
                        ],
                    )
                    .unwrap();
                }
                for conn in [conn, Connection::open(&path).unwrap()] {
                    assert_eq!(
                        resolve(
                            &conn,
                            &harness.storage_segment(),
                            "session",
                            "own",
                            "viewed"
                        )
                        .unwrap(),
                        (own_count == 1).then(|| "own-1".to_string())
                    );
                    for (harness, session, key) in [
                        (harness.wire_label(), "session", "viewed"),
                        (harness.storage_segment(), "foreign", "viewed"),
                        (harness.storage_segment(), "session", "absent"),
                    ] {
                        assert_eq!(resolve(&conn, &harness, session, "own", key).unwrap(), None);
                    }
                }
            }
        }
    }
}
