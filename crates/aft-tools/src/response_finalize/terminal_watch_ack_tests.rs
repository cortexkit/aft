use crate::context::AppContext;
use crate::db::{bash_watches, lifecycle::TrackedConnection};
use crate::harness::Harness;
use crate::protocol::Response;
use aft_engine::bash_background::{persistence::read_task, BgTaskRegistry};
use aft_engine::test_bridge::bash_remote::{registry_support, BgTaskRegistryTestExt as _};
use std::cell::Cell;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

const SESSION: &str = "session";
const HARNESS: &str = "opencode";

struct Fixture {
    dir: tempfile::TempDir,
    ctx: AppContext,
    pool: Arc<Mutex<TrackedConnection>>,
    task_id: String,
    task_json: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config::default(),
        );
        let registry = ctx.bash_background();
        registry.set_harness(Harness::Opencode);
        let pool = Arc::new(Mutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        registry.set_db_pool(Arc::clone(&pool));
        let (task_id, task_json) = registry_support::seed_terminal_watch_ack(registry, &dir);
        assert!(registry.has_completions_for_session(Some(SESSION)));
        Self {
            dir,
            ctx,
            pool,
            task_id,
            task_json,
        }
    }

    fn registry(&self) -> &BgTaskRegistry {
        self.ctx.bash_background()
    }

    fn sqlite_ack(&self) -> bool {
        crate::db::bash_tasks::get_bash_task(
            &self.pool.lock().unwrap(),
            HARNESS,
            SESSION,
            &self.task_id,
        )
        .unwrap()
        .unwrap()
        .completion_delivered
    }

    fn finalize(&self) {
        let mut response = Response::success(
            "watch",
            serde_json::json!({
                "task_id": self.task_id,
                "status": "completed",
            }),
        );
        let mut text = "done\n".to_owned();
        crate::response_finalize::finalize_tool_response(
            &mut response,
            &mut text,
            &self.ctx,
            SESSION,
            "bash_watch",
            true,
        );
        assert_eq!(text, "done\n");
        assert!(response.data.get("bg_completions").is_none());
        assert!(!self.registry().has_completions_for_session(Some(SESSION)));
        assert!(read_task(&self.task_json).unwrap().completion_delivered);
    }
}

thread_local! {
    static BUSY_CALLS: Cell<usize> = const { Cell::new(0) };
}

fn refuse_busy(_: i32) -> bool {
    BUSY_CALLS.with(|calls| calls.set(calls.get() + 1));
    false
}

#[test]
fn terminal_watch_ack_finalizes_while_sqlite_writer_holds_lock_then_retries() {
    let f = Fixture::new();
    let db_path = f.dir.path().join("aft.db");
    let foreign = rusqlite::Connection::open(db_path).unwrap();
    let watch = bash_watches::BashPatternWatchRow {
        harness: HARNESS.into(),
        session_id: SESSION.into(),
        task_id: f.task_id.clone(),
        watch_id: "other-watch".into(),
        pattern_kind: "substring".into(),
        pattern: "done".into(),
        once: true,
        each_line: false,
        match_batch: None,
        created_at: 1,
        stdout_offset: 0,
        stderr_offset: 0,
        pty_offset: 0,
        scanning: false,
        pending_match: true,
        match_text: Some("done".into()),
        match_offset: Some(0),
        match_context: None,
    };
    let watch_rows = || {
        bash_watches::list_bash_pattern_watches_for_task(
            &f.pool.lock().unwrap(),
            HARNESS,
            SESSION,
            &f.task_id,
        )
        .unwrap()
    };
    bash_watches::upsert_bash_pattern_watch(&f.pool.lock().unwrap(), &watch).unwrap();
    f.pool
        .lock()
        .unwrap()
        .busy_handler(Some(refuse_busy))
        .unwrap();
    BUSY_CALLS.with(|calls| calls.set(0));
    foreign.execute_batch("BEGIN IMMEDIATE").unwrap();

    f.finalize();
    assert_eq!(
        BUSY_CALLS.with(Cell::get),
        0,
        "finalize attempted a contended SQL write"
    );
    assert!(f
        .registry()
        .pending_pattern_matches_for_session(SESSION)
        .is_empty());
    registry_support::flush_terminal_watch_acks(f.registry());
    assert!(!f.sqlite_ack());
    assert_eq!(watch_rows().len(), 1);

    foreign.execute_batch("ROLLBACK").unwrap();
    registry_support::flush_terminal_watch_acks(f.registry());
    assert!(f.sqlite_ack());
    assert!(watch_rows().is_empty());
    assert!(registry_support::terminal_watch_acks_empty(f.registry()));
}

#[test]
fn terminal_watch_ack_replay_prefers_json_to_stale_sqlite_completion_bit() {
    let f = Fixture::new();
    f.finalize();
    assert!(!f.sqlite_ack(), "SQLite still has the pre-reply checkpoint");

    let restarted = BgTaskRegistry::default();
    restarted.set_harness(Harness::Opencode);
    restarted.set_db_pool(Arc::clone(&f.pool));
    restarted
        .replay_session_for_project(f.dir.path(), SESSION, f.dir.path())
        .unwrap();
    assert!(
        restarted.task_for_test(&f.task_id).is_some(),
        "replay must actually visit the stale task"
    );
    assert!(!restarted.has_completions_for_session(Some(SESSION)));
}

#[test]
fn terminal_watch_ack_cannot_overwrite_a_later_promotion() {
    let f = Fixture::new();
    f.finalize();

    f.registry().promote(&f.task_id, SESSION).unwrap();
    assert!(!read_task(&f.task_json).unwrap().completion_delivered);
    registry_support::flush_terminal_watch_acks(f.registry());

    assert!(
        !f.sqlite_ack(),
        "promotion created a new completion obligation"
    );
    assert!(registry_support::terminal_watch_acks_empty(f.registry()));
}
