use super::*;
use rusqlite::ffi;
use std::ffi::{c_int, c_void, CStr};

pub const NOW: i64 = 100 * TERMINAL_ROW_RETENTION_AGE_MS;
pub const TASK_HISTORY_ROWS: usize = 4096;
pub const EVENT_HISTORY_ROWS: usize = 8192;

pub struct Fixture {
    pub conn: crate::db::TrackedConnection,
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub fn new() -> Self {
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
pub struct Executed {
    sql: String,
    pub template: String,
    vm_steps: i32,
}

// PROFILE observes the statements the production functions actually execute,
// including uncached statements, so replacing their SQL cannot bypass the test.
pub fn record<T>(
    conn: &mut Connection,
    run: impl FnOnce(&mut Connection) -> T,
) -> (T, Vec<Executed>) {
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

pub fn explain(conn: &Connection, sql: &str) -> String {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("; ")
}
#[cfg(test)]
#[test]
fn project_replay_reads_candidates_not_delivered_history() {
    let mut fixture = Fixture::with_sizes(TASK_HISTORY_ROWS, 1);
    let conn = &mut fixture.conn;
    // Model a database populated before the optional replay index was installed.
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_bash_tasks_project_replay;
         UPDATE bash_tasks SET project_key='replay-project',
             metadata=json_object('completion_delivered', json('true'),
                                  'padding', printf('%08192d', 0));
         INSERT INTO bash_tasks
             (harness, session_id, task_id, project_key, command, cwd, status,
              started_at, completion_delivered, metadata)
         VALUES
             ('opencode', 's', 'undelivered', 'replay-project', '', '.', 'failed',
              1, 0, '{\"completion_delivered\":true}'),
             ('opencode', 's', 'running', 'replay-project', '', '.', 'running',
              2, 1, '{}'),
             ('opencode', 's', 'disagreement', 'replay-project', '', '.', 'completed',
              2, 1, '{\"completion_delivered\":false}'),
             ('opencode', 's', 'invalid-json', 'replay-project', '', '.', 'completed',
              0, 1, 'not-json'),
             ('opencode', 's', 'string-false', 'replay-project', '', '.', 'completed',
              0, 1, '{\"completion_delivered\":\"false\"}'),
             ('opencode', 's', 'other-project', 'other-project', '', '.', 'running',
              0, 0, '{}'),
             ('pi', 's', 'other-harness', 'replay-project', '', '.', 'running',
              0, 0, '{}');",
    )
    .unwrap();

    let (before, before_executed) = record(conn, |conn| {
        list_replayable_bash_tasks_for_project(conn, "opencode", "replay-project").unwrap()
    });
    assert_eq!(before_executed.len(), 1, "{before_executed:?}");
    let before_plan = explain(conn, &before_executed[0].sql);
    println!(
        "project replay before index: {before_plan}; VM steps: {}",
        before_executed[0].vm_steps
    );
    assert!(before_executed[0].vm_steps > TASK_HISTORY_ROWS as i32);
    let version_before = crate::db::schema_version_without_wait(conn).unwrap();
    crate::db::ensure_additive_indexes(conn).unwrap();
    assert_eq!(
        crate::db::schema_version_without_wait(conn).unwrap(),
        version_before,
        "installing an optional index must not change the schema version"
    );

    let (rows, executed) = record(conn, |conn| {
        list_replayable_bash_tasks_for_project(conn, "opencode", "replay-project").unwrap()
    });
    assert_eq!(
        rows.iter()
            .map(|row| row.task_id.as_str())
            .collect::<Vec<_>>(),
        ["undelivered", "disagreement", "running"]
    );
    assert_eq!(
        before
            .iter()
            .map(|row| row.task_id.as_str())
            .collect::<Vec<_>>(),
        rows.iter()
            .map(|row| row.task_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(executed.len(), 1, "{executed:?}");
    let query = &executed[0];
    let plan = explain(conn, &query.sql);
    println!("project replay: {plan}; VM steps: {}", query.vm_steps);
    assert!(plan.contains("idx_bash_tasks_project_replay"), "{plan}");
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    // A project-history scan executes thousands of steps even if it returns no rows.
    assert!(query.vm_steps > 0 && query.vm_steps < 1000, "{query:?}");
}
#[cfg(test)]
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
#[cfg(test)]
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
