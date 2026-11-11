//! Consumer-only enumeration for automatic cleanup of the bound session.
//! There is no target-session or kill argument: callers must bind as the
//! owner, then use the existing bash_kill command with that same identity.

use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    started_before_ms: Option<u64>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    if req.session_id.as_deref().is_none_or(str::is_empty) {
        return Response::error(&req.id, "invalid_request", "missing session_id");
    }
    let mut params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    if let Some(object) = params.as_object_mut() {
        object.remove(crate::protocol::WORKER_SESSION_FIELD);
        object.remove(crate::host_tool_names::HOST_TOOL_NAMES_FIELD);
    }
    let params: Params = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => return Response::error(&req.id, "invalid_request", error.to_string()),
    };
    let config = ctx.config();
    let Some(root) = config.project_root.as_deref() else {
        return Response::success(
            &req.id,
            json!({ "task_ids": [], "skipped_missing_owner": 0 }),
        );
    };
    match ctx.bash_background().owned_running_task_ids(
        &crate::bash_background::task_storage_dir(ctx),
        root,
        req.session(),
        params.started_before_ms,
    ) {
        Ok((task_ids, skipped_missing_owner)) => {
            crate::slog_info!(
                "bash_owned_tasks session={} skipped_missing_owner={skipped_missing_owner}",
                req.session()
            );
            Response::success(
                &req.id,
                json!({ "task_ids": task_ids, "skipped_missing_owner": skipped_missing_owner }),
            )
        }
        Err(error) => Response::error(&req.id, "internal_error", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::config::Config;
    use crate::context::App;
    use crate::db::{bash_tasks, TrackedConnection};
    use crate::harness::Harness;
    use crate::protocol::BgTaskStatus;
    use std::sync::{Arc, Mutex};

    struct Fixture {
        root: tempfile::TempDir,
        storage: tempfile::TempDir,
        ctx: AppContext,
        pool: Arc<Mutex<TrackedConnection>>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let ctx = AppContext::from_app(
                App::default_shared(),
                Config {
                    project_root: Some(root.path().to_path_buf()),
                    storage_dir: Some(storage.path().to_path_buf()),
                    ..Config::default()
                },
            );
            let pool = Arc::new(Mutex::new(
                crate::db::open(&storage.path().join("aft.db")).unwrap(),
            ));
            ctx.bash_background().set_harness(Harness::Opencode);
            ctx.bash_background().set_db_pool(Arc::clone(&pool));
            Self {
                root,
                storage,
                ctx,
                pool,
            }
        }
        fn task(&self, session: &str, id: &str, started: u64, owner_present: bool) {
            let paths = task_paths(self.storage.path(), session, id).unwrap();
            let mut task = PersistedTask::starting(
                id.into(),
                session.into(),
                "sleep 300".into(),
                self.root.path().to_path_buf(),
                Some(self.root.path().to_path_buf()),
                Some(300_000),
                true,
                true,
            );
            task.harness = owner_present.then(|| "opencode".into());
            task.status = BgTaskStatus::Running;
            task.started_at = started;
            write_task(&paths.json, &task).unwrap();
            let row = task.to_bash_task_row("opencode", &paths).unwrap();
            bash_tasks::upsert_bash_task(&self.pool.lock().unwrap(), &row).unwrap();
        }
        fn request(&self, session: Option<&str>, params: serde_json::Value) -> serde_json::Value {
            serde_json::to_value(handle(
                &RawRequest {
                    id: "owned".into(),
                    command: "bash_owned_tasks".into(),
                    lsp_hints: None,
                    session_id: session.map(str::to_owned),
                    params: json!({ "params": params }),
                },
                &self.ctx,
            ))
            .unwrap()
        }
    }

    #[test]
    fn owned_tasks_selects_only_bound_session_records() {
        let _env = crate::test_env::without_storage_override();
        let fixture = Fixture::new();
        fixture.task("owner", "bash-1111111111111111", 10, true);
        fixture.task("parent", "bash-2222222222222222", 10, true);
        let response = fixture.request(Some("owner"), json!({}));
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(response["task_ids"], json!(["bash-1111111111111111"]));
        assert_eq!(response["skipped_missing_owner"], 0);
    }

    #[test]
    fn owned_tasks_cannot_target_or_kill_another_session() {
        let _env = crate::test_env::without_storage_override();
        let fixture = Fixture::new();
        fixture.task("parent", "bash-2222222222222222", 10, true);
        let response = fixture.request(
            Some("child"),
            json!({ "session_id": "parent", "task_id": "bash-2222222222222222", "kill": true }),
        );
        assert_eq!(response["success"], false);
        assert_eq!(response["code"], "invalid_request");
        let row = bash_tasks::get_bash_task(
            &fixture.pool.lock().unwrap(),
            "opencode",
            "parent",
            "bash-2222222222222222",
        )
        .unwrap()
        .unwrap();
        assert_eq!(row.status, "running");
        assert_eq!(
            fixture.request(Some("child"), json!({}))["task_ids"],
            json!([])
        );
        assert_eq!(fixture.request(None, json!({}))["success"], false);
    }

    #[test]
    fn owned_tasks_skips_and_counts_missing_persisted_owner() {
        let _env = crate::test_env::without_storage_override();
        let fixture = Fixture::new();
        fixture.task("owner", "bash-3333333333333333", 10, false);
        let response = fixture.request(Some("owner"), json!({}));
        assert_eq!(response["task_ids"], json!([]));
        assert_eq!(response["skipped_missing_owner"], 1);
    }

    #[test]
    fn owned_tasks_stop_cutoff_excludes_resumed_execution_tasks() {
        let _env = crate::test_env::without_storage_override();
        let fixture = Fixture::new();
        fixture.task("owner", "bash-1111111111111111", 10, true);
        fixture.task("owner", "bash-4444444444444444", 30, true);
        let response = fixture.request(Some("owner"), json!({ "started_before_ms": 20 }));
        assert_eq!(response["task_ids"], json!(["bash-1111111111111111"]));
    }

    #[test]
    fn owned_tasks_atomic_owner_overrides_stale_db_mirror() {
        let _env = crate::test_env::without_storage_override();
        let fixture = Fixture::new();
        fixture.task("owner", "bash-1111111111111111", 10, true);
        let paths = task_paths(fixture.storage.path(), "owner", "bash-1111111111111111").unwrap();
        let mut task = crate::bash_background::persistence::read_task(&paths.json).unwrap();
        task.session_id = "parent".into();
        write_task(&paths.json, &task).unwrap();
        let response = fixture.request(Some("owner"), json!({}));
        assert_eq!(response["task_ids"], json!([]));
    }
}
