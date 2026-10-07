use crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES;
use crate::bash_background::persistence::BgMode;
use crate::bash_background::registry::BgTaskSnapshot;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

const PREVIEW_BYTES: usize = RUNNING_OUTPUT_PREVIEW_BYTES;

pub(crate) const UNKNOWN_TASK_GUIDANCE: &str = "No record of this task exists for this session. If this ID came from a bash tool result or completion notice, its record was lost (for example across an AFT restart); otherwise it is not a task ID. Either way, re-run the command instead of polling.";

pub(crate) fn format_unknown_task_message(task_id: &str) -> String {
    format!("background task not found: {task_id}. {UNKNOWN_TASK_GUIDANCE}")
}

pub(crate) fn format_erased_task_message(task_id: &str) -> String {
    format!(
        "background task row was erased: {task_id}. A recently armed watch still references this task; stop polling this ID and treat the missing row as a terminal storage failure."
    )
}

#[derive(Debug, Deserialize)]
struct BashStatusParams {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    output_mode: Option<String>,
    #[serde(default)]
    output_offset: Option<u64>,
    #[serde(default)]
    stderr_offset: Option<u64>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashStatusParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_status: invalid params: {e}"),
            );
        }
    };

    let output_mode = params.output_mode.clone();
    let output_offset = params.output_offset;
    let stderr_offset = params.stderr_offset;
    let Some(task_id) = params.task_id else {
        return Response::error(&req.id, "invalid_request", "bash_status: missing task_id");
    };

    if let Some(output_mode) = output_mode.as_deref() {
        if !matches!(output_mode, "screen" | "raw" | "both") {
            return Response::error(
                &req.id,
                "invalid_request",
                "bash_status: output_mode must be one of screen, raw, or both",
            );
        }
    }

    let storage_dir = crate::bash_background::task_storage_dir(ctx);
    // A delegated worker reading a task's status is waiting on it (its
    // `bash_watch` polls this): keep the task's default hard kill at least one
    // worker wait limit away. See `BgTaskRegistry::renew_hard_kill`.
    if req.worker_session() {
        ctx.bash_background().renew_hard_kill(
            &task_id,
            req.session(),
            std::time::Duration::from_millis(
                crate::commands::bash_orchestrate::worker_wait_max_ms(ctx),
            ),
        );
    }
    if ctx.bash_background().has_erased_watch_reference(&task_id) {
        return Response::error(&req.id, "task_erased", format_erased_task_message(&task_id));
    }
    // Settled: a kill in flight is waited out (bounded) so callers that stop
    // polling at the first non-running status see its terminal outcome.
    match ctx.bash_background().status_settled(
        &task_id,
        req.session(),
        ctx.config().project_root.as_deref(),
        Some(&storage_dir),
        PREVIEW_BYTES,
    ) {
        Some(mut snapshot) => {
            let pty_raw = maybe_render_pty_screen(
                ctx,
                req.session(),
                &task_id,
                &mut snapshot,
                output_mode.as_deref(),
            );
            if snapshot.info.mode == BgMode::Pty && snapshot.info.status.is_terminal() {
                ctx.bash_background()
                    .append_db_hint(&task_id, &mut snapshot.output_preview);
            }
            if snapshot.sandbox_native
                && snapshot.sandbox_unavailable
                && snapshot.exit_code == Some(crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE)
            {
                Response::error_with_data(
                    &req.id,
                    "sandbox_unavailable",
                    "native sandbox failed before the command could run; set sandbox.enabled=false to disable native sandboxing",
                    json!({
                        "task_id": snapshot.info.task_id,
                        "exit_code": snapshot.exit_code,
                        "output_preview": snapshot.output_preview,
                    }),
                )
            } else {
                let mode = snapshot.info.mode.clone();
                let mut data = json!(snapshot);
                if matches!(output_mode.as_deref(), Some("raw" | "both")) {
                    if let Some(raw) = pty_raw {
                        data["pty_raw"] = json!(String::from_utf8_lossy(&raw));
                    }
                }
                if let Some(offset) = output_offset {
                    let artifact = if mode == BgMode::Pty {
                        crate::bash_background::persistence::TaskArtifact::Pty
                    } else {
                        crate::bash_background::persistence::TaskArtifact::Stdout
                    };
                    match ctx.bash_background().read_artifact_range(
                        &task_id,
                        req.session(),
                        artifact,
                        offset,
                    ) {
                        Ok((bytes, next)) => {
                            data["output_chunk_base64"] =
                                json!(base64::engine::general_purpose::STANDARD.encode(bytes));
                            data["output_next_offset"] = json!(next);
                        }
                        Err(error) => {
                            return Response::error(
                                &req.id,
                                "artifact_refused",
                                format!("bash_status: task output refused: {error}"),
                            );
                        }
                    }
                }
                if mode == BgMode::Pipes {
                    if let Some(offset) = stderr_offset {
                        match ctx.bash_background().read_artifact_range(
                            &task_id,
                            req.session(),
                            crate::bash_background::persistence::TaskArtifact::Stderr,
                            offset,
                        ) {
                            Ok((bytes, next)) => {
                                data["stderr_chunk_base64"] =
                                    json!(base64::engine::general_purpose::STANDARD.encode(bytes));
                                data["stderr_next_offset"] = json!(next);
                            }
                            Err(error) => {
                                return Response::error(
                                    &req.id,
                                    "artifact_refused",
                                    format!("bash_status: task stderr refused: {error}"),
                                );
                            }
                        }
                    }
                }
                Response::success(&req.id, data)
            }
        }
        None => {
            // Replay records a refusal before returning no snapshot. Consult
            // both supported layouts so this task names the reader mismatch
            // rather than pretending its record was lost.
            let session_dir =
                crate::bash_background::persistence::session_tasks_dir(&storage_dir, req.session());
            if crate::bash_background::persistence::validate_task_id(&task_id).is_ok() {
                for path in [
                    session_dir
                        .join(&task_id)
                        .join("control")
                        .join("metadata.json"),
                    session_dir.join(format!("{task_id}.json")),
                ] {
                    if let Some(refusal) = crate::persisted_format::refusal_covering(
                        crate::persisted_format::PersistedStore::BashTask,
                        &path,
                    ) {
                        return Response::error(
                            &req.id,
                            crate::persisted_format::CODE,
                            refusal.to_string(),
                        );
                    }
                }
            }
            let unadopted = ctx.config().project_root.as_deref().and_then(|root| {
                ctx.bash_background()
                    .unadopted_task_message(&task_id, req.session(), root)
            });
            match unadopted {
                Some(message) => Response::error(&req.id, "task_not_adopted", message),
                None => Response::error(
                    &req.id,
                    "task_not_found",
                    format_unknown_task_message(&task_id),
                ),
            }
        }
    }
}

fn maybe_render_pty_screen(
    ctx: &AppContext,
    session_id: &str,
    task_id: &str,
    snapshot: &mut BgTaskSnapshot,
    output_mode: Option<&str>,
) -> Option<Vec<u8>> {
    if snapshot.info.mode != BgMode::Pty {
        return None;
    }
    match ctx.bash_background().read_artifact(
        task_id,
        session_id,
        crate::bash_background::persistence::TaskArtifact::Pty,
    ) {
        Ok(raw) => {
            if !matches!(output_mode, Some("raw")) {
                let rows = snapshot.pty_rows.unwrap_or(24);
                let cols = snapshot.pty_cols.unwrap_or(80);
                snapshot.pty_screen = Some(crate::pty_render::render_screen(&raw, rows, cols));
            }
            Some(raw)
        }
        Err(error) => {
            snapshot.pty_screen = Some(format!(
                "[PTY screen unavailable: failed to read raw output: {error}]"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{format_erased_task_message, format_unknown_task_message};
    use crate::bash_background::persistence::{
        create_task_layout, write_task_at, PersistedTask, TaskPaths,
    };
    use crate::bash_background::BgTaskStatus;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::harness::Harness;
    use crate::parser::TreeSitterProvider;
    use crate::persisted_format::{PersistedStore, UnsupportedPersistedFormat};
    use crate::protocol::{RawRequest, Response};
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    const FUTURE_TASK: &str = "bash-0000000000000001";
    const RUNNING_TASK: &str = "bash-0000000000000002";
    const COMPLETED_TASK: &str = "bash-0000000000000003";
    const DELIVERED_TASK: &str = "bash-0000000000000004";
    const SESSION: &str = "restore-session";
    const FUTURE_VERSION: u64 = crate::bash_background::persistence::SCHEMA_VERSION as u64 + 1;

    struct RestoreFixture {
        root: tempfile::TempDir,
        ctx: AppContext,
        future: TaskPaths,
        future_bytes: Vec<u8>,
        running: TaskPaths,
        delivered: TaskPaths,
    }

    impl RestoreFixture {
        fn new(harness: Harness) -> Self {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            fs::create_dir(&project).unwrap();
            let storage = root.path().join("storage");
            let ctx = AppContext::new(
                Box::new(TreeSitterProvider::new()),
                Config {
                    storage_dir: Some(storage.clone()),
                    project_root: Some(project.clone()),
                    harness: Some(harness.clone()),
                    ..Config::default()
                },
            );
            ctx.bash_background().set_harness(harness.clone());
            let task_storage = storage.join(harness.storage_segment());
            let future = create_task_layout(&task_storage, SESSION, FUTURE_TASK).unwrap();
            // The version header must suffice: an older reader cannot assume
            // the rest of a newer record still has its own payload shape.
            let future_bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": FUTURE_VERSION,
                "task_id": FUTURE_TASK,
                "future_payload": { "do_not_touch": true },
            }))
            .unwrap();
            fs::write(&future.paths.json, &future_bytes).unwrap();
            let normal = |task_id: &str, terminal: bool, delivered: bool| {
                let task = create_task_layout(&task_storage, SESSION, task_id).unwrap();
                let mut metadata = PersistedTask::starting(
                    task_id.into(),
                    SESSION.into(),
                    "printf output".into(),
                    project.clone(),
                    Some(project.clone()),
                    None,
                    true,
                    false,
                );
                metadata.harness = Some(harness.storage_segment());
                if terminal {
                    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
                    metadata.completion_delivered = delivered;
                } else {
                    // Read-only restore probes; no timeout or process group can
                    // signal the test process that stands in for a live child.
                    metadata.status = BgTaskStatus::Running;
                    metadata.child_pid = Some(std::process::id());
                }
                write_task_at(&task, &metadata).unwrap();
                fs::write(&task.paths.stdout, "stdout before restart\n").unwrap();
                fs::write(&task.paths.stderr, "stderr before restart\n").unwrap();
                task.paths
            };
            let running = normal(RUNNING_TASK, false, false);
            normal(COMPLETED_TASK, true, false);
            let delivered = normal(DELIVERED_TASK, true, true);
            // Bypass the GC grace period, using real artifacts rather than a
            // mocked reader, so the future record is first in the task sweep.
            let old = filetime::FileTime::from_system_time(
                SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60),
            );
            for paths in [&future.paths, &running, &delivered] {
                filetime::set_file_mtime(&paths.json, old).unwrap();
            }
            Self {
                root,
                ctx,
                future: future.paths,
                future_bytes,
                running,
                delivered,
            }
        }

        fn task_storage(&self) -> std::path::PathBuf {
            crate::bash_background::task_storage_dir(&self.ctx)
        }

        fn storage(&self) -> std::path::PathBuf {
            self.root.path().join("storage")
        }

        fn status(&self, task_id: &str, session: &str) -> Response {
            let request: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "restored-status", "command": "bash_status", "session_id": session,
                "params": { "task_id": task_id },
            }))
            .unwrap();
            super::handle(&request, &self.ctx)
        }

        fn assert_output(&self) {
            for (task_id, status) in [(RUNNING_TASK, "running"), (COMPLETED_TASK, "completed")] {
                let response = self.status(task_id, SESSION);
                assert!(response.success, "{:?}", response.data);
                assert_eq!(response.data["status"], status);
                let output = response.data["output_preview"].as_str().unwrap();
                assert!(output.contains("stdout before restart"), "{output:?}");
                assert!(output.contains("stderr before restart"), "{output:?}");
            }
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .open(&self.running.stdout)
                .unwrap()
                .write_all(b"stdout after restart\n")
                .unwrap();
            let response = self.status(RUNNING_TASK, SESSION);
            assert!(
                response.data["output_preview"]
                    .as_str()
                    .unwrap()
                    .contains("stdout after restart"),
                "{:?}",
                response.data
            );
        }
    }

    impl Drop for RestoreFixture {
        fn drop(&mut self) {
            self.ctx.bash_background().detach();
        }
    }

    #[test]
    fn future_bash_task_does_not_abort_gc_or_restore() {
        for harness in [Harness::Opencode, Harness::Pi, Harness::Runner] {
            let fixture = RestoreFixture::new(harness);
            assert_eq!(
                fixture
                    .ctx
                    .bash_background()
                    .maybe_gc_persisted(&fixture.task_storage())
                    .unwrap(),
                1
            );
            assert!(
                !fixture.delivered.dir.exists(),
                "GC never reached the task after the refusal"
            );
            fixture.assert_output();
        }
    }

    #[test]
    fn future_bash_task_restore_preserves_output_even_after_failed_gc() {
        for harness in [Harness::Opencode, Harness::Pi, Harness::Runner] {
            let fixture = RestoreFixture::new(harness);
            // A GC error was detached from replay even before per-record skip
            // handling. Exercise restore regardless of that sweep's result.
            let _ = fixture
                .ctx
                .bash_background()
                .maybe_gc_persisted(&fixture.task_storage());
            fixture.assert_output();
        }
    }

    #[test]
    fn future_bash_task_is_skipped_byte_identical() {
        let fixture = RestoreFixture::new(Harness::Opencode);
        fixture
            .ctx
            .bash_background()
            .replay_session(&fixture.task_storage(), SESSION)
            .unwrap();
        fixture
            .ctx
            .bash_background()
            .maybe_gc_persisted(&fixture.task_storage())
            .unwrap();
        assert_eq!(
            fs::read(&fixture.future.json).unwrap(),
            fixture.future_bytes
        );
        assert!(!fixture
            .task_storage()
            .join("bash-tasks-quarantine")
            .exists());
        let refusal = crate::persisted_format::refusal_covering(
            PersistedStore::BashTask,
            &fixture.future.json,
        )
        .unwrap();
        assert_eq!(refusal.found, FUTURE_VERSION);
        assert!(refusal.to_string().contains(FUTURE_TASK));
    }

    #[test]
    fn future_bash_task_refusal_is_task_scoped() {
        let fixture = RestoreFixture::new(Harness::Opencode);
        let response = fixture.status(FUTURE_TASK, SESSION);
        assert!(!response.success);
        assert_eq!(response.data["code"], crate::persisted_format::CODE);
        let message = response.data["message"].as_str().unwrap();
        assert!(
            message.contains(FUTURE_TASK)
                && message.contains(&format!("format version {FUTURE_VERSION}")),
            "{message}"
        );
        let health = fixture.ctx.build_status_snapshot();
        assert!(!health["degraded_reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "storage_requires_newer_reader:bash_task"
            )));
        assert!(
            health["storage_refusals"]
                .as_array()
                .unwrap()
                .iter()
                .any(|refusal| { refusal["path"] == fixture.future.json.display().to_string() }),
            "{health}"
        );
        assert_eq!(
            fixture
                .status("bash-000000000000ffff", "other-session")
                .data["code"],
            "task_not_found"
        );
        fixture.assert_output();

        let floor = UnsupportedPersistedFormat::floor(
            PersistedStore::BashTask,
            fixture.storage().join(crate::reader_floor::FLOOR_FILE),
            FUTURE_VERSION,
        );
        crate::persisted_format::record(&floor, &fixture.storage());
        let health = fixture.ctx.build_status_snapshot();
        assert!(health["degraded_reasons"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(
                "storage_requires_newer_reader:bash_task"
            )));
    }

    #[test]
    fn future_bash_task_refusal_clears_after_removal() {
        for remove in [false, true] {
            let fixture = RestoreFixture::new(Harness::Runner);
            fixture.status(FUTURE_TASK, SESSION);
            assert!(crate::persisted_format::refusal_covering(
                PersistedStore::BashTask,
                &fixture.future.json
            )
            .is_some());
            // Cover both a removed metadata file and a removed whole session;
            // neither will be re-read by task discovery on the next sweep.
            let path: &Path = if remove {
                &fixture.future.session_dir
            } else {
                &fixture.future.json
            };
            if remove {
                fs::remove_dir_all(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
            fixture
                .ctx
                .bash_background()
                .maybe_gc_persisted(&fixture.task_storage())
                .unwrap();
            assert!(crate::persisted_format::refusal_covering(
                PersistedStore::BashTask,
                &fixture.future.json
            )
            .is_none());
            assert!(crate::persisted_format::refusals_under(&fixture.storage()).is_empty());
            let health = fixture.ctx.build_status_snapshot();
            assert!(health["storage_refusals"].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn unknown_task_message_steers_agents_to_rerun() {
        assert_eq!(
            format_unknown_task_message("bash-unknown"),
            "background task not found: bash-unknown. No record of this task exists for this session. If this ID came from a bash tool result or completion notice, its record was lost (for example across an AFT restart); otherwise it is not a task ID. Either way, re-run the command instead of polling."
        );
    }

    #[test]
    fn erased_task_message_steers_agents_to_stop_polling() {
        let message = format_erased_task_message("bash-erased");
        assert!(message.contains("background task row was erased: bash-erased"));
        assert!(message.contains("stop polling this ID"));
        assert!(!message.contains(super::UNKNOWN_TASK_GUIDANCE));
    }
}
