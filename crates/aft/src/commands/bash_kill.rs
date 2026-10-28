use crate::commands::bash_status::format_unknown_polled_task_message;
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
        .kill_typed(&task_id, req.session())
        .or_else(|message| {
            if !message.is_missing_task() {
                return Err(message);
            }
            {
                let config = ctx.config();
                let _ = if let Some(project_root) = config.project_root.as_deref() {
                    let harness = crate::bash_background::route_harness()
                        .or_else(|| config.harness.clone())
                        .map(|harness| harness.storage_segment());
                    let scope = crate::bash_background::registry::scope_for_spawn();
                    let _ = ctx.bash_background().recover_kill_task(
                        &storage_dir,
                        req.session(),
                        &task_id,
                        project_root,
                        harness.as_deref(),
                        scope.as_ref(),
                    );
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
            ctx.bash_background().kill_typed(&task_id, req.session())
        })
        .or_else(|message| {
            if !message.is_missing_task() {
                return Err(message);
            }
            let config = ctx.config();
            let Some(project_root) = config.project_root.as_deref() else {
                return Err(message);
            };
            ctx.bash_background()
                .kill_relaxed_typed(&task_id, project_root, &storage_dir)
        });

    let warning = crate::gh_shim_speech::service::revocation_pending(req.session(), &task_id);
    let mut warnings = Vec::new();
    if warning {
        warnings.push(crate::gh_shim_speech::REVOCATION_NOT_DURABLE.to_string());
    }
    let queued = ctx
        .bash_background()
        .queued_bot_writes(req.session(), &task_id);
    match queued {
        Some(count) if count > 0 => warnings.push(queued_writes_warning(count)),
        None => warnings.push("bot_writes_already_queued: writes already queued for this command may still post; the pending count could not be read.".to_string()),
        _ => {},
    }
    let warning_text = (!warnings.is_empty()).then(|| warnings.join("\n"));
    match result {
        Ok(snapshot) => {
            let mut data = json!(snapshot);
            if let Some(warning) = &warning_text {
                data["bot_write_revocation_warning"] = json!(warning);
            }
            if queued.is_some_and(|count| count > 0) {
                data["bot_writes_already_queued"] = json!(queued.unwrap());
            }
            Response::success(&req.id, data)
        }
        Err(message) if message.is_missing_task() => Response::error(
            &req.id,
            "task_not_found",
            format_unknown_polled_task_message(&task_id, req.host_tool_names().shell()),
        ),
        Err(message) => Response::error(
            &req.id,
            "kill_failed",
            if let Some(warning) = warning_text {
                format!("{message}\n{warning}")
            } else {
                message.to_string()
            },
        ),
    }
}

pub(crate) fn queued_writes_warning(count: u64) -> String {
    format!("bot_writes_already_queued: {count} writes already queued for this command may still post; cancellation cannot unsend them.")
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

    #[test]
    fn bash_kill_unknown_id_reply_is_one_line_naming_bash() {
        let _storage_env = crate::test_env::without_storage_override();
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = actor(&App::default_shared(), project.path(), storage.path());

        let reply = handle(&kill_request("noop", "kill-session"), &ctx);
        assert_eq!(reply.data["code"], "task_not_found", "{:?}", reply.data);
        let message = reply.data["message"].as_str().unwrap();
        assert_eq!(
            message,
            "background task not found: noop. This session has no record of it (a record can be lost across an AFT restart). To run a command, use `bash`."
        );
        assert!(!message.contains('\n'), "{message:?}");
    }
}
