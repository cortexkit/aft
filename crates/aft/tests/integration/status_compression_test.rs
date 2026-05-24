use std::path::Path;
use std::sync::{Arc, Mutex};

use aft::config::Config;
use aft::context::AppContext;
use aft::db::compression_events::{insert_compression_event, CompressionEventRow};
use aft::harness::Harness;
use aft::parser::TreeSitterProvider;
use aft::search_index::project_cache_key;
use rusqlite::Connection;
use tempfile::tempdir;

fn context_with_db(project_root: &Path, harness: Harness) -> (AppContext, Arc<Mutex<Connection>>) {
    let mut conn = Connection::open_in_memory().expect("open test DB");
    aft::db::run_migrations(&mut conn).expect("migrate test DB");
    let shared = Arc::new(Mutex::new(conn));
    let ctx = context_without_db(project_root, harness);
    ctx.set_db(shared.clone());
    (ctx, shared)
}

fn context_without_db(project_root: &Path, harness: Harness) -> AppContext {
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project_root.to_path_buf()),
            ..Config::default()
        },
    );
    ctx.set_harness(harness);
    ctx
}

fn insert_event(
    conn: &Arc<Mutex<Connection>>,
    harness: Harness,
    project_root: &Path,
    session_id: &str,
    task_id: &str,
    original_tokens: u32,
    compressed_tokens: u32,
) {
    let project_key = project_cache_key(project_root);
    let row = CompressionEventRow {
        harness: harness.as_str(),
        session_id: Some(session_id),
        project_key: &project_key,
        tool: "bash",
        task_id: Some(task_id),
        command: Some("echo status-compression"),
        compressor: "test",
        original_bytes: i64::from(original_tokens),
        compressed_bytes: i64::from(compressed_tokens),
        original_tokens,
        compressed_tokens,
        created_at: 1_700_000_000_000,
    };
    insert_compression_event(&conn.lock().expect("DB lock"), &row)
        .expect("insert compression event");
}

#[test]
fn status_includes_compression_section_when_db_available() {
    let project = tempdir().expect("project dir");
    let (ctx, _conn) = context_with_db(project.path(), Harness::Opencode);

    let status = ctx.build_status_snapshot_for_session("session-a");

    assert_eq!(status["compression"]["project"]["events"], 0);
    assert_eq!(status["compression"]["session"]["events"], 0);
}

#[test]
fn status_compression_project_totals_aggregate_all_session_events() {
    let project = tempdir().expect("project dir");
    let (ctx, conn) = context_with_db(project.path(), Harness::Opencode);
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-1",
        "task-1",
        100,
        80,
    );
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-1",
        "task-2",
        120,
        90,
    );
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-2",
        "task-3",
        140,
        100,
    );

    let status = ctx.build_status_snapshot_for_session("session-2");

    assert_eq!(status["compression"]["project"]["events"], 3);
    assert_eq!(status["compression"]["session"]["events"], 1);
}

#[test]
fn status_compression_savings_computed_correctly() {
    let project = tempdir().expect("project dir");
    let (ctx, conn) = context_with_db(project.path(), Harness::Opencode);
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-a",
        "task-1",
        100,
        70,
    );

    let status = ctx.build_status_snapshot_for_session("session-a");

    assert_eq!(status["compression"]["project"]["savings_tokens"], 30);
    assert_eq!(status["compression"]["session"]["savings_tokens"], 30);
}

#[test]
fn status_compression_aggregates_zero_when_no_events() {
    let project = tempdir().expect("project dir");
    let (ctx, _conn) = context_with_db(project.path(), Harness::Opencode);

    let status = ctx.build_status_snapshot_for_session("session-a");

    assert_eq!(status["compression"]["project"]["events"], 0);
    assert_eq!(status["compression"]["project"]["original_tokens"], 0);
    assert_eq!(status["compression"]["project"]["compressed_tokens"], 0);
    assert_eq!(status["compression"]["project"]["savings_tokens"], 0);
    assert_eq!(status["compression"]["session"]["events"], 0);
    assert_eq!(status["compression"]["session"]["original_tokens"], 0);
    assert_eq!(status["compression"]["session"]["compressed_tokens"], 0);
    assert_eq!(status["compression"]["session"]["savings_tokens"], 0);
}

#[test]
fn status_compression_harness_isolation() {
    let project = tempdir().expect("project dir");
    let (ctx, conn) = context_with_db(project.path(), Harness::Pi);
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-a",
        "task-1",
        100,
        70,
    );

    let status = ctx.build_status_snapshot_for_session("session-a");

    assert_eq!(status["compression"]["project"]["events"], 0);
    assert_eq!(status["compression"]["session"]["events"], 0);
}

#[test]
fn status_compression_session_filter_correct() {
    let project = tempdir().expect("project dir");
    let (ctx, conn) = context_with_db(project.path(), Harness::Opencode);
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-x",
        "task-x-1",
        100,
        60,
    );
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-x",
        "task-x-2",
        80,
        50,
    );
    insert_event(
        &conn,
        Harness::Opencode,
        project.path(),
        "session-y",
        "task-y-1",
        200,
        150,
    );

    let status = ctx.build_status_snapshot_for_session("session-x");

    assert_eq!(status["compression"]["project"]["events"], 3);
    assert_eq!(status["compression"]["session"]["events"], 2);
    assert_eq!(status["compression"]["session"]["original_tokens"], 180);
    assert_eq!(status["compression"]["session"]["compressed_tokens"], 110);
    assert_eq!(status["compression"]["session"]["savings_tokens"], 70);
}

#[test]
fn status_db_unavailable_returns_zero_compression() {
    let project = tempdir().expect("project dir");
    let ctx = context_without_db(project.path(), Harness::Opencode);

    let status = ctx.build_status_snapshot_for_session("session-a");

    assert_eq!(status["compression"]["project"]["events"], 0);
    assert_eq!(status["compression"]["session"]["events"], 0);
}
