#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aft::bash_background::{BgCompletion, BgTaskRegistry, BgTaskStatus};
use aft::harness::Harness;
use rusqlite::{params, Connection};

const SESSION: &str = "compression-events-session";

#[derive(Debug)]
struct CompressionEvent {
    harness: String,
    session_id: Option<String>,
    project_key: String,
    tool: String,
    task_id: Option<String>,
    command: Option<String>,
    original_tokens: u32,
    compressed_tokens: u32,
    created_at: i64,
}

fn registry_with_db(storage: &Path, harness: Harness) -> (BgTaskRegistry, Arc<Mutex<Connection>>) {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(harness);
    let conn = aft::db::open(&storage.join("aft.db")).expect("open test DB");
    let shared = Arc::new(Mutex::new(conn));
    registry.set_db_pool(shared.clone());
    (registry, shared)
}

fn fresh_registry(conn: Arc<Mutex<Connection>>, harness: Harness) -> BgTaskRegistry {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(harness);
    registry.set_db_pool(conn);
    registry
}

fn spawn_task(registry: &BgTaskRegistry, storage: &Path, project: &Path, command: &str) -> String {
    registry
        .spawn(
            command,
            SESSION.to_string(),
            project.to_path_buf(),
            HashMap::new(),
            Some(Duration::from_secs(30)),
            storage.to_path_buf(),
            16,
            true,
            true,
            Some(project.to_path_buf()),
        )
        .expect("spawn background task")
}

fn wait_for_completion(registry: &BgTaskRegistry, task_id: &str) -> BgCompletion {
    let started = Instant::now();
    loop {
        if let Some(completion) = registry
            .drain_completions_for_session(Some(SESSION))
            .into_iter()
            .find(|completion| completion.task_id == task_id)
        {
            return completion;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for completion for {task_id}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_event_count(conn: &Arc<Mutex<Connection>>, task_id: &str, expected: i64) {
    let started = Instant::now();
    loop {
        let count = event_count(conn, task_id);
        if count == expected {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for {expected} compression events for {task_id}; saw {count}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn event_count(conn: &Arc<Mutex<Connection>>, task_id: &str) -> i64 {
    conn.lock()
        .expect("DB lock")
        .query_row(
            "SELECT COUNT(*) FROM compression_events WHERE task_id = ?1",
            params![task_id],
            |row| row.get(0),
        )
        .expect("count compression events")
}

fn fetch_event(conn: &Arc<Mutex<Connection>>, task_id: &str) -> CompressionEvent {
    conn.lock()
        .expect("DB lock")
        .query_row(
            "SELECT harness, session_id, project_key, tool, task_id, command,
                    original_tokens, compressed_tokens, created_at
             FROM compression_events
             WHERE task_id = ?1",
            params![task_id],
            |row| {
                Ok(CompressionEvent {
                    harness: row.get(0)?,
                    session_id: row.get(1)?,
                    project_key: row.get(2)?,
                    tool: row.get(3)?,
                    task_id: row.get(4)?,
                    command: row.get(5)?,
                    original_tokens: row.get(6)?,
                    compressed_tokens: row.get(7)?,
                    created_at: row.get(8)?,
                })
            },
        )
        .expect("fetch compression event")
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[test]
fn completed_task_records_compression_event() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "echo compression-event",
    );
    let completion = wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    let event = fetch_event(&conn, &task_id);

    assert!(!completion.tokens_skipped);
    assert!(event.original_tokens > 0);
    assert!(event.compressed_tokens > 0);
    registry.detach();
}

#[test]
fn completed_task_with_large_output_records_event_from_tail() {
    // Previously the 128KB-per-stream tokenize cap caused
    // `read_for_token_count` to return `Skipped`, which made
    // `record_compression_event_if_applicable` early-return silently.
    // That broke compression accounting for exactly the tasks that
    // benefit most from compression (huge log/test output). The fix
    // tokenizes the last 128KB of each stream so an event IS recorded
    // — `tokens_skipped` stays false, and the bash_tasks/compression_events
    // join still works for downstream analytics.
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "perl -e 'print \"token skip\\n\" x 20000'",
    );
    let completion = wait_for_completion(&registry, &task_id);

    assert!(!completion.tokens_skipped);
    wait_for_event_count(&conn, &task_id, 1);
    let event = fetch_event(&conn, &task_id);
    assert!(
        event.original_tokens > 1_000,
        "tail of ~200KB output should tokenize to substantially more than a tiny task, got {}",
        event.original_tokens
    );
    registry.detach();
}

#[test]
fn compression_event_has_correct_harness_and_project_key() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Pi);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "echo project-key",
    );
    wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    let event = fetch_event(&conn, &task_id);

    assert_eq!(event.harness, "pi");
    assert_eq!(event.session_id.as_deref(), Some(SESSION));
    assert_eq!(
        event.project_key,
        aft::search_index::project_cache_key(project.path())
    );
    assert_eq!(event.tool, "bash");
    registry.detach();
}

#[test]
fn compression_event_links_to_bash_task_via_task_id() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(&registry, storage.path(), project.path(), "echo join-task");
    wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    let joined_count: i64 = conn
        .lock()
        .expect("DB lock")
        .query_row(
            "SELECT COUNT(*)
             FROM compression_events compression
             JOIN bash_tasks task
               ON task.harness = compression.harness
              AND task.session_id = compression.session_id
              AND task.task_id = compression.task_id
             WHERE compression.task_id = ?1",
            params![task_id],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(joined_count, 1);
    registry.detach();
}

#[test]
fn compression_event_records_command_for_diagnostics() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);
    let command = "printf 'diagnostic-command'";

    let task_id = spawn_task(&registry, storage.path(), project.path(), command);
    wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    let event = fetch_event(&conn, &task_id);

    assert_eq!(event.task_id.as_deref(), Some(task_id.as_str()));
    assert_eq!(event.command.as_deref(), Some(command));
    registry.detach();
}

#[test]
fn completion_re_entry_does_not_duplicate_event() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "echo replay-once",
    );
    wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    registry.detach();

    let fresh = fresh_registry(conn.clone(), Harness::Opencode);
    fresh.replay_session(storage.path(), SESSION).unwrap();

    assert_eq!(event_count(&conn, &task_id), 1);
    fresh.detach();
}

#[test]
fn compression_event_for_compressed_output_shows_savings() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);
    registry.set_compressor(|_, _| "short".to_string());

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "perl -e 'print \"modified file\\n\" x 1000'",
    );
    wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    let event = fetch_event(&conn, &task_id);

    assert!(
        event.compressed_tokens < event.original_tokens,
        "expected savings in event: {event:?}"
    );
    registry.detach();
}

#[test]
fn compression_event_record_resilient_to_db_pool_absent() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "sleep 0.2; echo no-db-pool",
    );
    registry.clear_db_pool();
    let completion = wait_for_completion(&registry, &task_id);

    assert_eq!(completion.status, BgTaskStatus::Completed);
    assert_eq!(event_count(&conn, &task_id), 0);
    registry.detach();
}

#[test]
fn multiple_tasks_in_session_each_get_event() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_ids = [
        spawn_task(
            &registry,
            storage.path(),
            project.path(),
            "echo first-event",
        ),
        spawn_task(
            &registry,
            storage.path(),
            project.path(),
            "echo second-event",
        ),
        spawn_task(
            &registry,
            storage.path(),
            project.path(),
            "echo third-event",
        ),
    ];
    for task_id in &task_ids {
        wait_for_completion(&registry, task_id);
        wait_for_event_count(&conn, task_id, 1);
    }

    let total: i64 = conn
        .lock()
        .expect("DB lock")
        .query_row(
            "SELECT COUNT(*) FROM compression_events WHERE session_id = ?1",
            params![SESSION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total, 3);
    registry.detach();
}

#[test]
fn compression_event_created_at_is_close_to_completion_time() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "echo timestamp-event",
    );
    let before_completion = unix_millis();
    wait_for_completion(&registry, &task_id);
    wait_for_event_count(&conn, &task_id, 1);
    let after_completion = unix_millis();
    let event = fetch_event(&conn, &task_id);

    assert!(
        (before_completion - 5_000..=after_completion + 5_000).contains(&event.created_at),
        "event timestamp {} was not close to completion window {before_completion}..{after_completion}",
        event.created_at
    );
    registry.detach();
}
