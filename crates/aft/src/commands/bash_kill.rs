use crate::commands::bash_status::format_unknown_task_message;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashKillParams {
    #[serde(default)]
    task_id: Option<String>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashKillParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_kill: invalid params: {e}"),
            );
        }
    };

    let Some(task_id) = params.task_id else {
        return Response::error(&req.id, "invalid_request", "bash_kill: missing task_id");
    };

    let storage_dir = crate::bash_background::task_storage_dir(ctx);
    let result = ctx
        .bash_background()
        .kill(&task_id, req.session())
        .or_else(|message| {
            if !message.contains("not found") {
                return Err(message);
            }
            {
                let config = ctx.config();
                let _ = if let Some(project_root) = config.project_root.as_deref() {
                    ctx.bash_background().replay_session_for_project(
                        &storage_dir,
                        req.session(),
                        project_root,
                    )
                } else {
                    ctx.bash_background()
                        .replay_session(&storage_dir, req.session())
                };
            }
            ctx.bash_background().kill(&task_id, req.session())
        })
        .or_else(|message| {
            if !message.contains("not found") {
                return Err(message);
            }
            let config = ctx.config();
            let Some(project_root) = config.project_root.as_deref() else {
                return Err(message);
            };
            ctx.bash_background()
                .kill_relaxed(&task_id, project_root, &storage_dir)
        });

    match result {
        Ok(snapshot) => Response::success(&req.id, json!(snapshot)),
        Err(message) if message.contains("not found") => Response::error(
            &req.id,
            "task_not_found",
            format_unknown_task_message(&task_id),
        ),
        Err(message) => Response::error(&req.id, "kill_failed", message),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    use crate::config::Config;
    use crate::context::{App, AppContext};

    fn actor(app: &Arc<App>, project: &Path, storage: &Path) -> AppContext {
        let config = Config {
            project_root: Some(project.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..Config::default()
        };
        AppContext::from_app(Arc::clone(app), config)
    }

    /// Spawn a disposable child whose PID can be recorded as a task's
    /// `child_pid` in KILL-path tests. bash_kill terminates the recorded
    /// PID for running tasks, and on Windows that termination has no
    /// process-group indirection: recording the harness's own PID makes
    /// the kill take down the whole libtest process mid-run (observed as
    /// a clean exit 1 with no failures summary on Windows CI). Read-only
    /// paths (status replay, GC refusal) may still use the harness PID as
    /// an always-alive process; kill paths must use a child like this one.
    fn spawn_disposable_kill_target() -> std::process::Child {
        let mut cmd = if cfg!(windows) {
            // timeout.exe needs a console; ping is the standard sleep shim.
            let mut c = std::process::Command::new("ping");
            c.args(["-n", "31", "127.0.0.1"]);
            c
        } else {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.spawn().expect("spawn disposable kill-target child")
    }

    fn write_running_project_task(
        storage: &Path,
        project: &Path,
        session: &str,
        task_id: &str,
        child_pid: u32,
    ) {
        let paths = task_paths(storage, session, task_id).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            session.to_string(),
            "sleep 60".to_string(),
            project.to_path_buf(),
            Some(project.to_path_buf()),
            Some(30_000),
            true,
            true,
        );
        metadata.status = BgTaskStatus::Running;
        metadata.child_pid = Some(child_pid);
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, "still running\n").unwrap();
        fs::write(&paths.stderr, "").unwrap();
    }

    fn kill_request(task_id: &str, session: &str) -> RawRequest {
        RawRequest {
            id: "kill-project-filter".to_string(),
            command: "bash_kill".to_string(),
            lsp_hints: None,
            session_id: Some(session.to_string()),
            params: json!({ "params": { "task_id": task_id } }),
        }
    }

    #[test]
    fn bash_kill_replay_filters_same_session_by_project_root() {
        let _storage_env = crate::test_env::without_storage_override();
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let app = App::default_shared();
        let ctx_a = actor(&app, project_a.path(), storage.path());
        let ctx_b = actor(&app, project_b.path(), storage.path());
        let session = "shared-session";
        let task_id = "bash-2222222222222222";
        let mut kill_target = spawn_disposable_kill_target();
        write_running_project_task(
            storage.path(),
            project_a.path(),
            session,
            task_id,
            kill_target.id(),
        );

        let miss = serde_json::to_value(handle(&kill_request(task_id, session), &ctx_b)).unwrap();
        assert_eq!(
            miss["success"], false,
            "wrong project killed task: {miss:?}"
        );
        assert_eq!(miss["code"], "task_not_found");

        let killed = serde_json::to_value(handle(&kill_request(task_id, session), &ctx_a)).unwrap();
        assert_eq!(
            killed["success"], true,
            "owning project kill failed: {killed:?}"
        );
        assert_eq!(killed["status"], "killed");

        // Reap the disposable child: the product kill usually terminated it
        // already, so both calls are best-effort (kill on a dead child errors,
        // wait clears the zombie either way).
        let _ = kill_target.kill();
        let _ = kill_target.wait();
    }
}
