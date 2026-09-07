use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use aft::config::Config;
use aft::context::AppContext;
use aft::db::compression_events::{insert_compression_event, CompressionEventRow};
use aft::db::{SqliteStore, TrackedConnection as Connection};
use aft::harness::Harness;
use aft::parser::TreeSitterProvider;
use aft::path_identity::project_scope_key;
use tempfile::tempdir;

static STATUS_SQL_TRACE: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn capture_status_sql(sql: &str) {
    STATUS_SQL_TRACE
        .lock()
        .expect("status SQL trace lock")
        .push(sql.to_string());
}

fn context_with_db(project_root: &Path, harness: Harness) -> (AppContext, Arc<Mutex<Connection>>) {
    let mut conn = Connection::open_in_memory(SqliteStore::AftDb).expect("open test DB");
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
    let project_key = project_scope_key(project_root);
    let harness_seg = harness.storage_segment();
    let row = CompressionEventRow {
        harness: &harness_seg,
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
fn warm_status_snapshot_does_not_rescan_compression_aggregates() {
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
    let _ = ctx.build_status_snapshot_for_session("session-a");

    STATUS_SQL_TRACE
        .lock()
        .expect("status SQL trace lock")
        .clear();
    conn.lock()
        .expect("DB lock")
        .trace(Some(capture_status_sql));
    let status = ctx.build_status_snapshot_for_session("session-a");
    conn.lock().expect("DB lock").trace(None);

    assert_eq!(status["compression"]["project"]["events"], 1);
    let traced = STATUS_SQL_TRACE
        .lock()
        .expect("status SQL trace lock")
        .clone();
    assert!(
        traced
            .iter()
            .any(|sql| { sql.contains("MAX(id)") && sql.contains("FROM compression_events") }),
        "warm status must retain the constant-time external-writer guard: {traced:?}"
    );
    assert!(
        traced.iter().all(|sql| {
            !(sql.contains("FROM compression_events")
                && (sql.contains("COUNT(") || sql.contains("SUM(")))
        }),
        "warm status unexpectedly rescanned compression aggregates: {traced:?}"
    );
}

#[test]
fn status_compression_cache_refreshes_after_second_connection_insert() {
    let project = tempdir().expect("project dir");
    let storage = tempdir().expect("storage dir");
    let db_path = storage.path().join("aft.db");
    let primary = Arc::new(Mutex::new(
        aft::db::open(&db_path).expect("open primary DB"),
    ));
    let ctx = context_without_db(project.path(), Harness::Opencode);
    ctx.set_db(primary);

    let initial = ctx.build_status_snapshot_for_session("session-a");
    assert_eq!(initial["compression"]["project"]["events"], 0);

    let external = Arc::new(Mutex::new(
        aft::db::open(&db_path).expect("open external DB connection"),
    ));
    insert_event(
        &external,
        Harness::Opencode,
        project.path(),
        "session-a",
        "external-task",
        200,
        125,
    );

    let refreshed = ctx.build_status_snapshot_for_session("session-a");
    assert_eq!(refreshed["compression"]["project"]["events"], 1);
    assert_eq!(refreshed["compression"]["session"]["events"], 1);
    assert_eq!(refreshed["compression"]["session"]["savings_tokens"], 75);
}

#[test]
fn status_db_unavailable_returns_zero_compression() {
    let project = tempdir().expect("project dir");
    let ctx = context_without_db(project.path(), Harness::Opencode);

    let status = ctx.build_status_snapshot_for_session("session-a");

    assert_eq!(status["compression"]["project"]["events"], 0);
    assert_eq!(status["compression"]["session"]["events"], 0);
}
