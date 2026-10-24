use super::*;
use rusqlite::ffi;
use std::ffi::{c_int, c_void, CStr};

const NOW: i64 = 100 * TERMINAL_ROW_RETENTION_AGE_MS;
const TASK_HISTORY_ROWS: usize = 4096;
const EVENT_HISTORY_ROWS: usize = 8192;

struct Fixture {
    conn: crate::db::TrackedConnection,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_sizes(TASK_HISTORY_ROWS, EVENT_HISTORY_ROWS)
    }

    fn with_sizes(task_rows: usize, event_rows: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        // Keep almost all history recent: a small retention batch must not walk it.
        conn.execute_batch(&format!(
            "BEGIN;
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<{task_rows})
             INSERT INTO bash_tasks
               (harness, session_id, task_id, project_key, command, cwd, status,
                started_at, completed_at, completion_delivered, metadata)
             SELECT 'opencode', 's' || (i % 1000), 't' || i, 'p' || (i % 100),
                    'echo history', '.', 'completed', 0,
                    CASE WHEN i<=64 THEN 0 ELSE {NOW}-1 END, 1, '{{}}' FROM n;
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<{event_rows})
             INSERT INTO compression_events
               (harness, session_id, project_key, tool, task_id, compressor,
                original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at)
             SELECT 'opencode', 's' || (i % 1000), 'p' || (i % 100), 'bash', 't' || i,
                    'test', 100, 50, 20, 10,
                    CASE WHEN i<=64 THEN 0 ELSE {NOW}-1 END FROM n;
             COMMIT;"
        ))
        .unwrap();
        let statistics: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name='sqlite_stat1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            statistics, 0,
            "the regression requires an unanalyzed database"
        );
        Self { conn, _dir: dir }
    }

    fn prepared(&self, count: usize) -> PreparedTerminalPrune {
        PreparedTerminalPrune {
            identities: (1..=count)
                .map(|i| TerminalPruneCandidate {
                    harness: "opencode".into(),
                    session_id: format!("s{}", i % 1000),
                    task_id: format!("t{i}"),
                    pid: None,
                    pgid: None,
                    started_at: 0,
                    stdout_path: None,
                    stderr_path: None,
                })
                .collect(),
            probed_candidates: count,
            cutoff: NOW - TERMINAL_ROW_RETENTION_AGE_MS,
        }
    }
}

#[derive(Debug)]
struct Executed {
    sql: String,
    template: String,
    vm_steps: i32,
}

// PROFILE observes the statements the production functions actually execute,
// including uncached statements, so replacing their SQL cannot bypass the test.
fn record<T>(conn: &mut Connection, run: impl FnOnce(&mut Connection) -> T) -> (T, Vec<Executed>) {
    unsafe extern "C" fn profile(
        _: u32,
        context: *mut c_void,
        statement: *mut c_void,
        _: *mut c_void,
    ) -> c_int {
        let statement = statement.cast::<ffi::sqlite3_stmt>();
        let sql = ffi::sqlite3_expanded_sql(statement);
        if !sql.is_null() {
            let records = &mut *context.cast::<Vec<Executed>>();
            records.push(Executed {
                sql: CStr::from_ptr(sql).to_string_lossy().into_owned(),
                template: CStr::from_ptr(ffi::sqlite3_sql(statement))
                    .to_string_lossy()
                    .into_owned(),
                vm_steps: ffi::sqlite3_stmt_status(statement, ffi::SQLITE_STMTSTATUS_VM_STEP, 1),
            });
            ffi::sqlite3_free(sql.cast());
        }
        0
    }
    struct Reset(*mut ffi::sqlite3);
    impl Drop for Reset {
        fn drop(&mut self) {
            unsafe { ffi::sqlite3_trace_v2(self.0, 0, None, std::ptr::null_mut()) };
        }
    }
    let mut records = Vec::<Executed>::new();
    // The callback and its stack-owned context live only for this synchronous call.
    let result = unsafe {
        ffi::sqlite3_trace_v2(
            conn.handle(),
            ffi::SQLITE_TRACE_PROFILE as u32,
            Some(profile),
            (&mut records as *mut Vec<Executed>).cast(),
        )
    };
    assert_eq!(result, ffi::SQLITE_OK);
    let reset = Reset(unsafe { conn.handle() });
    let value = run(conn);
    drop(reset);
    (value, records)
}

fn explain(conn: &Connection, sql: &str) -> String {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ")
}

#[test]
fn terminal_retention_delete_uses_primary_key_and_batch_work() {
    let mut histories = Vec::new();
    // A 16-fold history increase must not increase point-delete VM work. Even
    // the smaller history makes a status-index scan exceed the per-key bound.
    for task_rows in [256, TASK_HISTORY_ROWS] {
        let mut fixture = Fixture::with_sizes(task_rows, 128);
        let mut measured = Vec::new();
        for count in [1, 8, 32] {
            fixture.conn.execute_batch("BEGIN").unwrap();
            let prepared = fixture.prepared(count);
            let (result, statements) = record(&mut fixture.conn, |conn| {
                delete_prepared_terminal_rows(conn, prepared).unwrap()
            });
            assert_eq!(result.removed, count);
            assert_eq!(result.remaining_candidates, 0);
            let deletes = statements
                .iter()
                .filter(|s| s.sql.starts_with("DELETE FROM bash_tasks"))
                .collect::<Vec<_>>();
            assert!(!deletes.is_empty(), "no production delete was observed");
            let steps: i32 = deletes.iter().map(|s| s.vm_steps).sum();
            let plan = explain(&fixture.conn, &deletes[0].sql);
            println!("history={task_rows} batch={count} VM_STEP={steps} plan={plan}");
            assert!(
                plan.contains("(harness=? AND session_id=? AND task_id=?)")
                    && !plan.contains("idx_bash_tasks_status")
                    && steps > 0
                    && steps < 500 * count as i32,
                "retention must probe full primary keys, not history: history={task_rows} batch={count} VM_STEP={steps} plan={plan}"
            );
            measured.push(steps);
            fixture.conn.execute_batch("ROLLBACK").unwrap();
        }
        assert!(measured[2] <= measured[0] * 32 + 100);
        histories.push(measured);
    }
    for (small, large) in histories[0].iter().zip(&histories[1]) {
        assert!(
            *large <= small * 2,
            "delete work grew with history: {histories:?}"
        );
    }
}

#[test]
fn terminal_retention_delete_rechecks_prepared_identities() {
    let fixture = Fixture::with_sizes(64, 128);
    let prepared = fixture.prepared(8);
    for (task, change) in [
        ("t2", "completion_delivered=0"),
        ("t3", "status='running'"),
        ("t4", "completed_at=NULL"),
        ("t5", "completed_at=9223372036854775807"),
    ] {
        fixture
            .conn
            .execute(
                &format!("UPDATE bash_tasks SET {change} WHERE task_id=?1"),
                [task],
            )
            .unwrap();
    }
    fixture.conn.execute_batch(
        "INSERT INTO bash_pattern_watches
           (harness,session_id,task_id,watch_id,pattern_kind,pattern,created_at)
         VALUES ('opencode','s6','t6','w','literal','x',0);
         INSERT INTO bash_tasks
           (harness,session_id,task_id,project_key,command,cwd,status,started_at,completed_at,completion_delivered,metadata)
         VALUES ('other','s1','t1','p','echo','.','completed',0,0,1,'{}');"
    ).unwrap();
    let result = delete_prepared_terminal_rows(&fixture.conn, prepared).unwrap();
    assert_eq!(result.removed, 3);
    assert_eq!(result.remaining_candidates, 5);
    for i in 1..=8 {
        assert_eq!(
            get_bash_task(
                &fixture.conn,
                "opencode",
                &format!("s{i}"),
                &format!("t{i}")
            )
            .unwrap()
            .is_some(),
            (2..=6).contains(&i)
        );
    }
    assert!(get_bash_task(&fixture.conn, "other", "s1", "t1")
        .unwrap()
        .is_some());
    assert_eq!(
        terminal_rows_eligible_count(&fixture.conn, NOW).unwrap(),
        57
    );
    assert_eq!(
        delete_prepared_terminal_rows(&fixture.conn, fixture.prepared(0))
            .unwrap()
            .removed,
        0
    );
}

#[test]
fn retention_and_hot_query_plans_on_unanalyzed_history() {
    use crate::db::compression_events::*;
    let mut fixture = Fixture::new();
    fixture
        .conn
        .execute_batch(
            "INSERT INTO bash_pattern_watches
           (harness,session_id,task_id,watch_id,pattern_kind,pattern,created_at)
         VALUES ('opencode','s1','t1','w','literal','x',0);",
        )
        .unwrap();
    let (_, statements) = record(&mut fixture.conn, |conn| {
        terminal_rows_eligible_count(conn, NOW).unwrap();
        let mut row = get_bash_task(conn, "opencode", "s65", "t65")
            .unwrap()
            .unwrap();
        row.pid = Some(123);
        upsert_bash_task(conn, &row).unwrap();
        // Completion acknowledgements issue this point update directly from
        // the task registry rather than through the database helper module.
        conn.execute(
            "UPDATE bash_tasks SET completion_delivered = 1
             WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3
               AND status IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown')",
            params!["opencode", "s65", "t65"],
        )
        .unwrap();
        assert_eq!(
            list_bash_task_process_ids(conn, &["t65".into(), "missing".into()])
                .unwrap()
                .len(),
            1
        );
        find_bash_task_session(conn, "opencode", "t65").unwrap();
        list_bash_tasks_for_session(conn, "opencode", "s65").unwrap();
        list_replayable_bash_tasks_for_project(conn, "opencode", "p65").unwrap();
        find_bash_task_for_project(conn, "opencode", "p65", "t65").unwrap();
        delete_delivered_terminal_bash_task(conn, "opencode", "s999", "t3999", "plan audit")
            .unwrap();
        delete_bash_task(conn, "opencode", "s998", "t3998").unwrap();
        let cache = CompressionAggregateCache::default();
        cache
            .aggregates_for_session(conn, "opencode", "p65", "s65")
            .unwrap();
        let event = CompressionEventRow {
            harness: "opencode",
            session_id: Some("s65"),
            project_key: "p65",
            tool: "bash",
            task_id: Some("new"),
            compressor: "test",
            original_bytes: 100,
            compressed_bytes: 50,
            original_tokens: 20,
            compressed_tokens: 10,
            created_at: NOW,
        };
        let id = insert_compression_event(conn, &event).unwrap().unwrap();
        cache.record_successful_insert(conn, &event, id);
        // Use a valid persisted-task id so the real selection/layout path also
        // reaches the delete; the synthetic history ids are not layout names.
        let mut old = row.clone();
        old.task_id = "bash-0000000000000000".into();
        old.completed_at = Some(0);
        old.pid = None;
        upsert_bash_task(conn, &old).unwrap();
        // Two ticks exercise both the initial and persisted cursor paths.
        for _ in 0..2 {
            prune_retention_tick(conn, NOW).unwrap();
        }
    });
    let mut unique = std::collections::BTreeMap::new();
    for statement in statements {
        unique
            .entry(statement.template.clone())
            .or_insert(statement);
    }
    println!(
        "SQLite {}: {TASK_HISTORY_ROWS} tasks, {EVENT_HISTORY_ROWS} events, no ANALYZE; {} distinct statements",
        rusqlite::version(),
        unique.len()
    );
    let mut process_lookup = None;
    let mut session_aggregate = None;
    for statement in unique.values() {
        let plan = explain(&fixture.conn, &statement.sql);
        println!(
            "SQL: {}\nPLAN: {plan}\nVM_STEP: {}",
            statement
                .template
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            statement.vm_steps
        );
        if plan.is_empty() {
            // INSERT/UPSERT and transaction statements have no EQP rows. Their
            // bytecode still exposes unique-index and rowid conflict probes.
            let opcodes = fixture
                .conn
                .prepare(&format!("EXPLAIN {}", statement.sql))
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            println!(
                "KEY OPCODES: {:?}",
                opcodes
                    .iter()
                    .filter(|op| matches!(
                        op.as_str(),
                        "NoConflict" | "NotExists" | "SeekRowid" | "Rewind" | "Next"
                    ))
                    .collect::<Vec<_>>()
            );
        }
        if statement
            .template
            .trim_start()
            .starts_with("SELECT task_id, pid, pgid, started_at")
        {
            process_lookup = Some((plan, statement.vm_steps));
        } else if statement.template.contains("SELECT SUM(events)")
            && statement.template.contains("AND session_id = ?3")
        {
            session_aggregate = Some((plan, statement.vm_steps));
        }
    }
    let (plan, steps) = process_lookup.expect("production process lookup must be observed");
    let (session_plan, session_steps) =
        session_aggregate.expect("production session aggregate must be observed");
    assert!(plan.contains("task_id=?") && steps > 0 && steps < 500
        && session_plan.contains("(harness=? AND session_id=?)") && session_steps > 0 && session_steps < 10000,
        "hot queries must seek task/session keys: process VM_STEP={steps} plan={plan}; session VM_STEP={session_steps} plan={session_plan}");
}
