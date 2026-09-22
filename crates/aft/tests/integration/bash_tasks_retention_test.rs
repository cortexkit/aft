#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aft::bash_background::persistence::{
    create_task_layout, session_tasks_dir, write_task_at, PersistedTask,
};
use aft::bash_background::BgTaskStatus;
use aft::db::bash_tasks::{
    prune_terminal_rows, terminal_rows_eligible_count, upsert_bash_task, BashTaskRow,
    TERMINAL_ROW_RETENTION_AGE_MS,
};
use aft::db::compression_events::{
    prune_retention_sweep, prune_retention_tick, BASH_TASK_STEADY_STATE_ROWS,
};
use aft::db::TrackedConnection as Connection;
use rusqlite::params;

const NOW_MS: i64 = 45 * 24 * 60 * 60 * 1000;
const OLD_MS: i64 = 1;
const SESSION: &str = "retention-session";

fn task_id(index: usize) -> String {
    format!("bash-{index:016x}")
}

fn insert_task(
    conn: &Connection,
    task_id: &str,
    status: &str,
    completed_at: Option<i64>,
    completion_delivered: bool,
) {
    upsert_bash_task(
        conn,
        &BashTaskRow {
            harness: "opencode".to_string(),
            session_id: SESSION.to_string(),
            task_id: task_id.to_string(),
            project_key: "project".to_string(),
            command: "true".to_string(),
            cwd: ".".to_string(),
            status: status.to_string(),
            exit_code: Some(0),
            pid: None,
            pgid: None,
            started_at: OLD_MS,
            completed_at,
            stdout_path: None,
            stderr_path: None,
            compressed: true,
            timeout_ms: None,
            completion_delivered,
            output_bytes: Some(0),
            metadata: String::new(),
        },
    )
    .expect("insert bash task");
}

fn insert_watch(conn: &Connection, task_id: &str) {
    conn.execute(
        "INSERT INTO bash_pattern_watches (
            harness, session_id, task_id, watch_id, pattern_kind, pattern, created_at
         ) VALUES ('opencode', ?1, ?2, 'watch-00000001', 'substring', 'done', ?3)",
        params![SESSION, task_id, OLD_MS],
    )
    .expect("insert pattern watch");
}

fn task_exists(conn: &Connection, task_id: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM bash_tasks WHERE task_id = ?1)",
        [task_id],
        |row| row.get(0),
    )
    .expect("query task existence")
}

fn task_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
        .expect("count bash tasks")
}

fn create_terminal_layout(storage: &Path, task_id: &str) {
    let resolved = create_task_layout(storage, SESSION, task_id).expect("create task layout");
    let mut metadata = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        "true".to_string(),
        storage.to_path_buf(),
        Some(storage.to_path_buf()),
        None,
        false,
        true,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task_at(&resolved, &metadata).expect("write task metadata");
}

#[test]
fn terminal_pruner_preserves_every_delivery_and_layout_invariant() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");

    let running = task_id(1);
    insert_task(&conn, &running, "running", Some(OLD_MS), true);
    let killing = task_id(2);
    insert_task(&conn, &killing, "killing", Some(OLD_MS), true);
    let fate_unknown = task_id(3);
    insert_task(&conn, &fate_unknown, "fate_unknown", Some(OLD_MS), true);
    let recent = task_id(4);
    insert_task(
        &conn,
        &recent,
        "completed",
        Some(NOW_MS - TERMINAL_ROW_RETENTION_AGE_MS + 1),
        true,
    );
    let layout_present = task_id(5);
    insert_task(&conn, &layout_present, "completed", Some(OLD_MS), true);
    create_terminal_layout(storage.path(), &layout_present);
    let undelivered = task_id(6);
    insert_task(&conn, &undelivered, "completed", Some(OLD_MS), false);
    let watched = task_id(7);
    insert_task(&conn, &watched, "completed", Some(OLD_MS), true);
    insert_watch(&conn, &watched);
    let removable = task_id(8);
    insert_task(&conn, &removable, "completed", Some(OLD_MS), true);

    assert_eq!(
        terminal_rows_eligible_count(&conn, NOW_MS).expect("count eligible rows"),
        2
    );
    let result = prune_terminal_rows(&conn, NOW_MS, 500).expect("prune terminal rows");

    assert_eq!(result.removed, 1);
    assert_eq!(result.remaining_candidates, 1);
    for survivor in [
        &running,
        &killing,
        &fate_unknown,
        &recent,
        &layout_present,
        &undelivered,
        &watched,
    ] {
        assert!(task_exists(&conn, survivor), "pruned survivor {survivor}");
    }
    assert!(!task_exists(&conn, &removable));
}

#[test]
fn terminal_pruner_handles_legacy_ids_without_deleting_surviving_artifacts() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");
    let absent = "bash-deadbeef";
    let present = "bash-cafebabe";
    insert_task(&conn, absent, "completed", Some(OLD_MS), true);
    insert_task(&conn, present, "completed", Some(OLD_MS), true);
    let session_dir = session_tasks_dir(storage.path(), SESSION);
    std::fs::create_dir_all(&session_dir).expect("create legacy session directory");
    std::fs::write(session_dir.join(format!("{present}.json")), b"legacy")
        .expect("write legacy artifact");

    let result = prune_terminal_rows(&conn, NOW_MS, 500).expect("prune terminal rows");

    assert_eq!(result.removed, 1);
    assert_eq!(result.remaining_candidates, 1);
    assert!(!task_exists(&conn, absent));
    assert!(task_exists(&conn, present));
}

#[test]
fn terminal_pruner_never_removes_more_than_five_hundred_rows() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");
    for index in 0..501 {
        insert_task(&conn, &task_id(index + 1_000), "failed", Some(OLD_MS), true);
    }

    let result = prune_terminal_rows(&conn, NOW_MS, usize::MAX).expect("prune terminal rows");

    assert_eq!(result.removed, 500);
    assert_eq!(result.remaining_candidates, 1);
    assert_eq!(task_count(&conn), 1);
}

#[test]
fn retention_sweep_drains_backlog_in_bounded_transactions() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");
    let backlog = BASH_TASK_STEADY_STATE_ROWS * 2 + 1;
    for index in 0..backlog {
        insert_task(
            &conn,
            &task_id(index + 10_000),
            "completed",
            Some(OLD_MS),
            true,
        );
    }
    let db = Arc::new(Mutex::new(conn));

    let sweep = prune_retention_sweep(&db, NOW_MS, Some(&[]))
        .expect("retention sweep")
        .expect("uncontended retention sweep");

    assert_eq!(sweep.initial_eligible_rows, backlog);
    assert_eq!(sweep.row_ceiling, backlog);
    assert_eq!(sweep.passes, 3);
    assert_eq!(sweep.bash_tasks_removed, backlog);
    assert_eq!(sweep.remaining_eligible_rows, Some(0));
    assert_eq!(task_count(&db.lock().expect("database lock")), 0);
}

#[test]
fn retention_steady_state_keeps_single_batch_ceiling_without_backlog() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");
    for index in 0..3 {
        insert_task(
            &conn,
            &task_id(index + 20_000),
            "completed",
            Some(OLD_MS),
            true,
        );
    }
    let db = Arc::new(Mutex::new(conn));

    let sweep = prune_retention_sweep(&db, NOW_MS, Some(&[]))
        .expect("retention sweep")
        .expect("uncontended retention sweep");

    assert_eq!(sweep.initial_eligible_rows, 3);
    assert_eq!(sweep.row_ceiling, BASH_TASK_STEADY_STATE_ROWS);
    assert_eq!(sweep.passes, 1);
    assert_eq!(sweep.bash_tasks_removed, 3);
}

#[test]
fn retention_catch_up_never_deletes_a_row_for_a_live_pid() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");
    let live = task_id(30_000);
    insert_task(&conn, &live, "completed", Some(OLD_MS), true);
    conn.execute(
        "UPDATE bash_tasks SET pid = ?1, started_at = ?2 WHERE task_id = ?3",
        params![i64::from(std::process::id()), current_unix_millis(), live],
    )
    .expect("record live process");
    for index in 0..=BASH_TASK_STEADY_STATE_ROWS {
        insert_task(
            &conn,
            &task_id(index + 31_000),
            "completed",
            Some(OLD_MS),
            true,
        );
    }
    let db = Arc::new(Mutex::new(conn));

    let sweep = prune_retention_sweep(&db, NOW_MS, Some(&[]))
        .expect("retention sweep")
        .expect("uncontended retention sweep");

    assert!(sweep.initial_eligible_rows > BASH_TASK_STEADY_STATE_ROWS);
    assert_eq!(sweep.bash_tasks_removed, BASH_TASK_STEADY_STATE_ROWS + 1);
    assert_eq!(sweep.remaining_eligible_rows, Some(1));
    assert!(task_exists(&db.lock().expect("database lock"), &live));
}

#[test]
fn retention_tick_rolls_back_task_delete_when_event_pruning_fails() {
    let storage = tempfile::tempdir().expect("storage tempdir");
    let mut conn = aft::db::open(&storage.path().join("aft.db")).expect("open database");
    let task_id = task_id(9_000);
    insert_task(&conn, &task_id, "completed", Some(OLD_MS), true);
    conn.execute(
        "INSERT INTO compression_events (
            harness, session_id, project_key, tool, task_id, command, compressor,
            original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at
         ) VALUES ('opencode', ?1, 'project', 'bash', ?2, 'true', 'test', 10, 5, 10, 5, ?3)",
        params![SESSION, task_id, OLD_MS],
    )
    .expect("insert linked compression event");
    conn.execute(
        "INSERT INTO compression_events (
            harness, session_id, project_key, tool, task_id, command, compressor,
            original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at
         ) VALUES ('opencode', ?1, 'project', 'bash', NULL, 'watermark', 'test', 1, 1, 1, 1, ?2)",
        params![SESSION, NOW_MS],
    )
    .expect("insert compression watermark");
    conn.execute_batch(&format!(
        "CREATE TRIGGER reject_linked_event_delete
         BEFORE DELETE ON compression_events
         WHEN OLD.task_id = '{task_id}'
         BEGIN SELECT RAISE(ABORT, 'linked event delete rejected'); END;"
    ))
    .expect("install rejection trigger");

    assert!(prune_retention_tick(&mut conn, NOW_MS).is_err());
    assert!(task_exists(&conn, &task_id), "task delete escaped rollback");
    assert_eq!(compression_event_count(&conn, &task_id), 1);

    conn.execute_batch("DROP TRIGGER reject_linked_event_delete")
        .expect("drop rejection trigger");
    let tick = prune_retention_tick(&mut conn, NOW_MS).expect("retry retention tick");
    assert_eq!(tick.bash_tasks.removed, 1);
    assert_eq!(tick.compression_events_removed, 1);
    assert!(!task_exists(&conn, &task_id));
    assert_eq!(compression_event_count(&conn, &task_id), 0);
}

fn compression_event_count(conn: &Connection, task_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM compression_events WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )
    .expect("count compression events")
}

#[test]
#[ignore = "manual measurement against a copied aft.db"]
fn measure_terminal_pruner_on_database_copy() {
    let path = std::env::var_os("AFT_BASH_TASK_PRUNE_DB_COPY")
        .map(std::path::PathBuf::from)
        .expect("set AFT_BASH_TASK_PRUNE_DB_COPY to an offline copy");
    assert_ne!(
        path.file_name().and_then(|name| name.to_str()),
        Some("aft.db")
    );
    let conn = aft::db::open(&path).expect("open copied database");
    let db = Arc::new(Mutex::new(conn));
    let now_ms = current_unix_millis();
    let before_rows = task_count(&db.lock().expect("database lock"));
    let before_eligible = terminal_rows_eligible_count(&db.lock().expect("database lock"), now_ms)
        .expect("count eligible rows");
    let started = Instant::now();
    let sweep = prune_retention_sweep(&db, now_ms, Some(&[]))
        .expect("retention sweep")
        .expect("uncontended retention sweep");
    let elapsed_micros = started.elapsed().as_micros();
    eprintln!(
        "bash task retention measurement: before_rows={before_rows} after_rows={} before_eligible={before_eligible} after_eligible={:?} removed={} passes={} row_ceiling={} worst_count_lock_us={} worst_selection_lock_us={} worst_mutation_lock_us={} worst_lock_us={} elapsed_us={elapsed_micros}",
        task_count(&db.lock().expect("database lock")),
        sweep.remaining_eligible_rows,
        sweep.bash_tasks_removed,
        sweep.passes,
        sweep.row_ceiling,
        sweep.worst_count_lock_micros,
        sweep.worst_selection_lock_micros,
        sweep.worst_mutation_lock_micros,
        sweep.worst_lock_micros,
    );
    assert!(
        sweep.worst_lock_micros < 100_000,
        "retention lock hold exceeded 100 ms: {sweep:?}"
    );
}

fn current_unix_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}
