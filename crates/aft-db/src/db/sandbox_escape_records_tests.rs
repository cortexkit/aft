use super::*;

fn record(key: String, at: i64) -> NewRecord {
    NewRecord {
        call_key: key,
        requested_at_ms: at,
        caller_session: "session-a".into(),
        caller_scope: "head-a".into(),
        agent_id: Some("agent-a".into()),
        command: "printf approved > marker".into(),
        cwd: "/work/project".into(),
        shell: "/bin/bash".into(),
        escapes_level: "strict".into(),
        background: true,
    }
}

#[test]
fn record_contains_request_and_outcome_but_has_no_output_field() {
    let conn = Connection::open_in_memory().unwrap();
    insert(&conn, &record("call-a".into(), 100)).unwrap();
    note_elicitation(&conn, "call-a", "el_1").unwrap();
    note_decision(&conn, "call-a", "run_once").unwrap();
    finish(
        &conn,
        "call-a",
        "run_once",
        Some("running"),
        None,
        Some("bash-1"),
    )
    .unwrap();
    finish_task(&conn, "bash-1", "completed", Some(0)).unwrap();

    let row = get(&conn, "call-a").unwrap().unwrap();
    assert_eq!(row.command, "printf approved > marker");
    assert_eq!(row.cwd, "/work/project");
    assert_eq!(row.shell, "/bin/bash");
    assert_eq!(row.escapes_level, "strict");
    assert!(row.background);
    assert_eq!(row.decision, "run_once");
    assert_eq!(row.exit_status.as_deref(), Some("completed"));
    assert_eq!(row.exit_code, Some(0));

    let columns = conn
        .prepare("PRAGMA table_info(sandbox_escape_records)")
        .unwrap()
        .query_map([], |column| column.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!columns.iter().any(|name| name.contains("output")));
}

#[test]
fn maintenance_prunes_ninety_days_and_keeps_newest_ten_thousand() {
    let conn = Connection::open_in_memory().unwrap();
    let now = 2 * RETENTION_MS;
    insert(&conn, &record("old".into(), now - RETENTION_MS - 1)).unwrap();
    insert(&conn, &record("boundary".into(), now - RETENTION_MS)).unwrap();
    assert_eq!(sweep(&conn, now).unwrap(), 1);
    assert!(get(&conn, "old").unwrap().is_none());
    assert!(get(&conn, "boundary").unwrap().is_some());

    for number in 0..=MAX_ROWS {
        insert(
            &conn,
            &record(format!("call-{number}"), now + number as i64 + 1),
        )
        .unwrap();
    }
    assert_eq!(sweep(&conn, now + MAX_ROWS as i64 + 1).unwrap(), 2);
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM sandbox_escape_records", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(remaining, MAX_ROWS as i64);
    assert!(get(&conn, "call-0").unwrap().is_none());
    assert!(get(&conn, &format!("call-{MAX_ROWS}")).unwrap().is_some());
}
