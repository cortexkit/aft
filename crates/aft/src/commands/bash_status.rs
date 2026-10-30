use crate::bash_background::output::RUNNING_OUTPUT_PREVIEW_BYTES;
use crate::bash_background::persistence::BgMode;
use crate::bash_background::registry::BgTaskSnapshot;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

const PREVIEW_BYTES: usize = RUNNING_OUTPUT_PREVIEW_BYTES;

/// Guidance for the `bash` call paths (foreground wait, promotion) whose own
/// task disappeared. Tools that take a task id from the agent use the shorter
/// one-line replies below instead.
pub(crate) const UNKNOWN_TASK_GUIDANCE: &str = "No record of this task exists for this session. If this ID came from a bash tool result or completion notice, its record was lost (for example across an AFT restart); otherwise it is not a task ID. Either way, re-run the command instead of polling.";

pub(crate) fn format_unknown_task_message(task_id: &str) -> String {
    format!("background task not found: {task_id}. {UNKNOWN_TASK_GUIDANCE}")
}

/// One-line reply when `bash_status`, `bash_kill` or `bash_watch` is given a
/// task id this session does not know. The id may be made up, or the task's
/// record may have been lost (for example across an AFT restart); either way
/// the useful next step is to run the command with the command tool, named
/// here as the host serves it (`shell_tool`). The `background task not found:`
/// prefix stays so callers that match on it keep working.
pub(crate) fn format_unknown_polled_task_message(task_id: &str, shell_tool: &str) -> String {
    format!(
        "background task not found: {task_id}. This session has no record of it (a record can be lost across an AFT restart). To run a command, use `{shell_tool}`."
    )
}

/// One-line reply when `bash_write` is given a task id this session does not
/// know. Agents sometimes call `bash_write` as if it were the shell, with
/// invented ids, so the reply sends them to the command tool (`shell_tool`,
/// the host's served name for it) instead of explaining lost records.
pub(crate) fn format_unknown_write_task_message(task_id: &str, shell_tool: &str) -> String {
    format!(
        "background task not found: {task_id}. To run a command, use `{shell_tool}`; `bash_write` only types into a task that `{shell_tool}` started."
    )
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
    /// Raw transport cursor: pages contain at most 64 KiB. output_next_offset
    /// advances by bytes actually returned; output_has_more reports whether
    /// more captured bytes remain past that offset. PTY byte paging omits
    /// full-history screen replay; request a screen separately without offsets.
    #[serde(default)]
    output_offset: Option<u64>,
    /// The stderr page has independent stderr_next_offset/stderr_has_more fields.
    #[serde(default)]
    stderr_offset: Option<u64>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    handle_with_observation_only(req, ctx, false)
}

/// `observation_only` is transport-owned, never read from tool arguments.
pub(crate) fn handle_with_observation_only(
    req: &RawRequest,
    ctx: &AppContext,
    observation_only: bool,
) -> Response {
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
    let config = ctx.config();
    let project_root = config.project_root.as_deref();
    let store_unavailable = ctx.database_runtime_unavailable_reason();
    // A delegated worker reading a task's status is waiting on it (its
    // `bash_watch` polls this): keep the task's default hard kill at least one
    // worker wait limit away. See `BgTaskRegistry::renew_hard_kill`.
    if !observation_only && req.worker_session() && store_unavailable.is_none() {
        ctx.bash_background().renew_hard_kill(
            &task_id,
            req.session(),
            std::time::Duration::from_millis(
                crate::commands::bash_orchestrate::worker_wait_max_ms(ctx),
            ),
        );
    }
    let erased = if observation_only {
        ctx.bash_background()
            .observed_erased_watch_reference(&task_id)
    } else {
        ctx.bash_background().has_erased_watch_reference(&task_id)
    };
    if erased {
        return if observation_only {
            Response::error(&req.id, "task_not_found", "The viewed task was not found.")
        } else {
            Response::error(&req.id, "task_erased", format_erased_task_message(&task_id))
        };
    }
    // Settled: a kill in flight is waited out (bounded) so callers that stop
    // polling at the first non-running status see its terminal outcome.
    let mut disk_paths = None;
    let own_snapshot = if observation_only {
        ctx.bash_background()
            .observation_status(
                &task_id,
                req.session(),
                project_root,
                &storage_dir,
                PREVIEW_BYTES,
            )
            .map(|(snapshot, paths)| {
                disk_paths = Some(paths);
                snapshot
            })
    } else if store_unavailable.is_some() {
        ctx.bash_background()
            .terminal_status_from_disk(&task_id, req.session(), project_root, &storage_dir)
            .map(|(snapshot, paths)| {
                disk_paths = Some(paths);
                snapshot
            })
    } else {
        ctx.bash_background().status_settled_owned(
            &task_id,
            req.session(),
            project_root,
            &storage_dir,
            PREVIEW_BYTES,
        )
    };
    let mut worker_owner = None;
    let snapshot = own_snapshot
        .or_else(|| {
            // An observation-only read is restricted to the request's session;
            // ordinary reads may also resolve a delegated worker's task.
            if observation_only {
                return None;
            }
            worker_owner = ctx
                .bash_background()
                .readable_worker_task_owner(&task_id, &storage_dir);
            let owner = worker_owner.as_deref()?;
            if observation_only {
                ctx.bash_background()
                    .observation_status(&task_id, owner, None, &storage_dir, PREVIEW_BYTES)
                    .map(|(snapshot, paths)| {
                        disk_paths = Some(paths);
                        snapshot
                    })
            } else if store_unavailable.is_some() {
                ctx.bash_background()
                    .terminal_status_from_disk(&task_id, owner, None, &storage_dir)
                    .map(|(snapshot, paths)| {
                        disk_paths = Some(paths);
                        snapshot
                    })
            } else {
                ctx.bash_background().status_settled_owned(
                    &task_id,
                    owner,
                    None,
                    &storage_dir,
                    PREVIEW_BYTES,
                )
            }
        })
        .or_else(|| {
            // Standalone unstamped tasks retain their existing same-project fallback.
            // Scoped calls must not search other sessions after an exact miss.
            if !observation_only
                && crate::access_policy::current_scope().is_none()
                && store_unavailable.is_none()
            {
                ctx.bash_background().status_settled(
                    &task_id,
                    req.session(),
                    project_root,
                    Some(&storage_dir),
                    PREVIEW_BYTES,
                )
            } else {
                None
            }
        });
    let task_session = worker_owner.as_deref().unwrap_or(req.session());
    match snapshot {
        Some(mut snapshot) => {
            let inputs_captured = snapshot.remote_inputs_captured;
            // The capture warning asks the caller to wait for queued or running,
            // so it is added only while the task is not terminal; a terminal
            // task's reply is returned without it.
            let in_flight = !snapshot.info.status.is_terminal();
            // A byte-paged read returns only its page below. Rendering the PTY
            // screen would replay the whole PTY log on every page, so the
            // screen is skipped; unless raw-only output was asked for, a note
            // in its place says how to request the screen.
            let pty_raw = if output_offset.is_some() {
                if snapshot.info.mode == BgMode::Pty
                    && !matches!(output_mode.as_deref(), Some("raw"))
                {
                    snapshot.pty_screen = Some("[PTY screen omitted while paging raw bytes; request screen mode without output_offset.]".into());
                }
                None
            } else {
                maybe_render_pty_screen(
                    ctx,
                    task_session,
                    &task_id,
                    &mut snapshot,
                    output_mode.as_deref(),
                    disk_paths.as_ref(),
                )
            };
            if snapshot.info.mode == BgMode::Pty && snapshot.info.status.is_terminal() {
                ctx.bash_background()
                    .append_db_hint(&task_id, &mut snapshot.output_preview);
            }
            let response = if let Some(refusal) = &snapshot.remote_refusal {
                let mut extra = json!(snapshot);
                if !observation_only {
                    ctx.bash_background()
                        .attach_terminal_view(&mut extra, task_session);
                }
                Response::error_with_data(&req.id, refusal.code, &refusal.message, extra)
            } else if snapshot.sandbox_native
                && snapshot.sandbox_unavailable
                && snapshot.exit_code
                    == Some(i64::from(
                        crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE,
                    ))
            {
                Response::error_with_data(
                    &req.id,
                    "sandbox_unavailable",
                    "native sandbox failed before the command could run; select sandbox.profile off to disable native sandboxing",
                    json!({
                        "task_id": snapshot.info.task_id,
                        "exit_code": snapshot.exit_code,
                        "output_preview": snapshot.output_preview,
                    }),
                )
            } else {
                let mode = snapshot.info.mode.clone();
                let mut data = json!(snapshot);
                if let Some(reason) = &store_unavailable {
                    data["store_unavailable"] = json!(reason);
                    data["output_preview"] = json!(format!(
                        "[Store unavailable: {reason}; status and output read from the task bundle]\n{}",
                        data["output_preview"].as_str().unwrap_or_default(),
                    ));
                }
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
                    match read_status_artifact_page(
                        ctx,
                        disk_paths.as_ref(),
                        &task_id,
                        task_session,
                        artifact,
                        offset,
                    ) {
                        Ok(page) => {
                            data["output_page_text"] =
                                json!(ctx.bash_background().masked_status_page(
                                    &task_id,
                                    artifact,
                                    offset.min(page.next_offset),
                                    &page.bytes
                                ));
                            if mode == BgMode::Pty
                                && matches!(output_mode.as_deref(), Some("raw" | "both"))
                            {
                                data["pty_raw"] = json!(String::from_utf8_lossy(&page.bytes));
                            }
                            data["output_chunk_base64"] =
                                json!(base64::engine::general_purpose::STANDARD.encode(page.bytes));
                            data["output_next_offset"] = json!(page.next_offset);
                            data["output_has_more"] = json!(page.has_more);
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
                        match read_status_artifact_page(
                            ctx,
                            disk_paths.as_ref(),
                            &task_id,
                            task_session,
                            crate::bash_background::persistence::TaskArtifact::Stderr,
                            offset,
                        ) {
                            Ok(page) => {
                                data["stderr_page_text"] =
                                    json!(ctx.bash_background().masked_status_page(
                                        &task_id,
                                        crate::bash_background::persistence::TaskArtifact::Stderr,
                                        offset.min(page.next_offset),
                                        &page.bytes
                                    ));
                                data["stderr_chunk_base64"] =
                                    json!(base64::engine::general_purpose::STANDARD
                                        .encode(page.bytes));
                                data["stderr_next_offset"] = json!(page.next_offset);
                                data["stderr_has_more"] = json!(page.has_more);
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
                if !observation_only {
                    ctx.bash_background()
                        .attach_terminal_view(&mut data, task_session);
                }
                Response::success(&req.id, data)
            };
            if inputs_captured == Some(false) && in_flight {
                super::bash_orchestrate::with_capture_warning(&response)
            } else {
                response
            }
        }
        None => {
            if observation_only {
                return Response::error(
                    &req.id,
                    "task_not_found",
                    "The viewed task was not found.",
                );
            }
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
            if let Some(reason) = &store_unavailable {
                return Response::error(&req.id, "database_unavailable", format!(
                    "Store unavailable: {reason}; no readable finished task bundle for {task_id}. Running tasks require the store to recover.",
                ));
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
                    format_unknown_polled_task_message(&task_id, req.host_tool_names().shell()),
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
    disk_paths: Option<&crate::bash_background::persistence::TaskPaths>,
) -> Option<Vec<u8>> {
    if snapshot.info.mode != BgMode::Pty {
        return None;
    }
    let raw = if let Some(paths) = disk_paths {
        crate::bash_background::persistence::open_task_artifact(
            paths,
            crate::bash_background::persistence::TaskArtifact::Pty,
        )
        .and_then(|mut file| file.read_all())
        .map_err(|error| error.to_string())
    } else {
        ctx.bash_background().read_artifact(
            task_id,
            session_id,
            crate::bash_background::persistence::TaskArtifact::Pty,
        )
    };
    match raw {
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

fn read_status_artifact_page(
    ctx: &AppContext,
    disk_paths: Option<&crate::bash_background::persistence::TaskPaths>,
    task_id: &str,
    session: &str,
    artifact: crate::bash_background::persistence::TaskArtifact,
    offset: u64,
) -> Result<crate::bash_background::registry::ArtifactPage, String> {
    let Some(paths) = disk_paths else {
        return ctx
            .bash_background()
            .read_artifact_page(task_id, session, artifact, offset);
    };
    let read = || -> std::io::Result<_> {
        let (mut file, len) =
            crate::bash_background::persistence::sample_task_artifact(paths, artifact)?;
        let start = offset.min(len);
        let requested = len
            .saturating_sub(start)
            .min(crate::bash_background::registry::ARTIFACT_PAGE_BYTES as u64);
        let bytes = file.read_range(start, requested)?;
        let next_offset = start + bytes.len() as u64;
        let has_more = bytes.len() as u64 == requested && !bytes.is_empty() && next_offset < len;
        Ok(crate::bash_background::registry::ArtifactPage {
            bytes,
            next_offset,
            has_more,
        })
    };
    read().map_err(|error| format!("artifact_refused: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        format_erased_task_message, format_unknown_polled_task_message,
        format_unknown_task_message, format_unknown_write_task_message,
    };
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
    const FAILED_TASK: &str = "bash-0000000000000005";
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
            let failed = create_task_layout(&task_storage, SESSION, FAILED_TASK).unwrap();
            let mut failed_metadata = PersistedTask::starting(
                FAILED_TASK.into(),
                SESSION.into(),
                "git status --short".into(),
                project.clone(),
                Some(project.clone()),
                None,
                true,
                false,
            );
            failed_metadata.harness = Some(harness.storage_segment());
            failed_metadata.mark_terminal(
                BgTaskStatus::Failed,
                None,
                Some("failed to spawn background bash command: No such file or directory".into()),
            );
            write_task_at(&failed, &failed_metadata).unwrap();
            // Bypass the GC grace period, using real artifacts rather than a
            // mocked reader, so the future record is first in the task sweep.
            let old = filetime::FileTime::from_system_time(
                SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60),
            );
            for paths in [&future.paths, &running, &delivered, &failed.paths] {
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
    fn restart_finished_status_survives_unavailable_store_without_replay() {
        let fixture = RestoreFixture::new(Harness::Opencode);
        fixture.ctx.finish_database_runtime_error(
            "storage_requires_newer_reader: database reader floor".into(),
            false,
        );
        assert!(fixture
            .ctx
            .database_runtime_refusal("status", "bash_status")
            .is_none());
        let response = fixture.status(COMPLETED_TASK, SESSION);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "completed");
        assert_eq!(response.data["exit_code"], 0);
        assert!(response.data["store_unavailable"]
            .as_str()
            .unwrap()
            .contains("storage_requires_newer_reader"));
        let output = response.data["output_preview"].as_str().unwrap();
        assert!(output.contains("stdout before restart"), "{output}");
        assert!(output.contains("stderr before restart"), "{output}");
        assert!(fixture.ctx.bash_background().list(0).is_empty());
        assert_eq!(
            fs::read(&fixture.future.json).unwrap(),
            fixture.future_bytes
        );
        assert!(!fixture.status(RUNNING_TASK, SESSION).success);
        fs::write(&fixture.running.exit, "7\n").unwrap();
        let exited = fixture.status(RUNNING_TASK, SESSION);
        assert!(exited.success, "{exited:?}");
        assert_eq!(exited.data["status"], "failed");
        assert_eq!(exited.data["exit_code"], 7);
        assert!(fixture.ctx.bash_background().list(0).is_empty());
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "chunk", "command": "bash_status", "session_id": SESSION,
            "params": { "task_id": RUNNING_TASK, "output_offset": 0, "stderr_offset": 0 },
        }))
        .unwrap();
        let chunk = super::handle(&request, &fixture.ctx);
        assert!(chunk.success, "{chunk:?}");
        assert_eq!(
            chunk.data["output_next_offset"],
            "stdout before restart\n".len()
        );
        assert_eq!(
            chunk.data["stderr_next_offset"],
            "stderr before restart\n".len()
        );
        assert!(!fixture.status(COMPLETED_TASK, "other-session").success);
        let refused = fixture.status(FUTURE_TASK, SESSION);
        assert!(!refused.success);
        assert_eq!(refused.data["code"], crate::persisted_format::CODE);
    }

    /// With the store unavailable, status reads the task bundle on disk. Those
    /// byte pages must stay bounded like registered-task pages, and continue
    /// at the offset actually returned.
    #[test]
    fn approved_store_unavailable_status_pages_are_bounded() {
        use base64::Engine as _;
        let fixture = RestoreFixture::new(Harness::Opencode);
        fixture.ctx.finish_database_runtime_error(
            "storage_requires_newer_reader: database reader floor".into(),
            false,
        );
        fs::write(&fixture.running.exit, "0\n").unwrap();
        let output = b"abcdefgh".repeat(3 * 8 * 1024 + 1);
        fs::write(&fixture.running.stdout, &output).unwrap();
        let page = |offset: u64| {
            let request: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "disk-page", "command": "bash_status", "session_id": SESSION,
                "params": { "task_id": RUNNING_TASK, "output_offset": offset, "stderr_offset": 0 },
            }))
            .unwrap();
            let response = super::handle(&request, &fixture.ctx);
            assert!(response.success, "{response:?}");
            assert!(response.data["store_unavailable"].is_string());
            response
        };
        let mut cursor = 0;
        let mut collected = Vec::new();
        loop {
            let response = page(cursor);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(response.data["output_chunk_base64"].as_str().unwrap())
                .unwrap();
            assert!(
                bytes.len() <= 64 * 1024,
                "one page held {} bytes",
                bytes.len()
            );
            let next = response.data["output_next_offset"].as_u64().unwrap();
            assert_eq!(next, cursor + bytes.len() as u64);
            let has_more = response.data["output_has_more"].as_bool().unwrap();
            assert_eq!(has_more, (next as usize) < output.len());
            assert_eq!(response.data["stderr_has_more"], false);
            collected.extend(bytes);
            cursor = next;
            if !has_more {
                break;
            }
        }
        assert_eq!(collected, output);
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
    fn bash_status_returns_failed_task_reason_in_structured_data() {
        let fixture = RestoreFixture::new(Harness::Pi);

        let response = fixture.status(FAILED_TASK, SESSION);

        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["status"], "failed");
        assert_eq!(
            response.data["status_reason"],
            "failed to spawn background bash command: No such file or directory"
        );
    }

    #[test]
    fn unknown_task_messages_are_one_line_and_name_the_command_tool() {
        assert_eq!(
            format_unknown_polled_task_message("bash-unknown", "bash"),
            "background task not found: bash-unknown. This session has no record of it (a record can be lost across an AFT restart). To run a command, use `bash`."
        );
        assert_eq!(
            format_unknown_write_task_message("x", "bash"),
            "background task not found: x. To run a command, use `bash`; `bash_write` only types into a task that `bash` started."
        );
        // A host that serves the command tool under another name gets that
        // name, so the redirect points at a tool the agent can actually call.
        assert_eq!(
            format_unknown_write_task_message("x", "shell"),
            "background task not found: x. To run a command, use `shell`; `bash_write` only types into a task that `shell` started."
        );
        // The `bash` call paths whose own task vanished keep the rerun advice.
        assert!(format_unknown_task_message("bash-unknown")
            .ends_with("re-run the command instead of polling."));
    }

    #[test]
    fn bash_status_unknown_id_reply_is_one_line_naming_bash() {
        let fixture = RestoreFixture::new(Harness::Opencode);
        let response = fixture.status("noop", SESSION);
        assert!(!response.success, "{:?}", response.data);
        assert_eq!(response.data["code"], "task_not_found");
        let message = response.data["message"].as_str().unwrap();
        assert_eq!(
            message,
            "background task not found: noop. This session has no record of it (a record can be lost across an AFT restart). To run a command, use `bash`."
        );
        assert!(!message.contains('\n'), "{message:?}");

        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": "served-name-status", "command": "bash_status", "session_id": SESSION,
            "host_tool_names": { "bash": "shell" },
            "params": { "task_id": "noop" },
        }))
        .unwrap();
        let response = super::handle(&request, &fixture.ctx);
        assert_eq!(response.data["code"], "task_not_found");
        assert!(
            response.data["message"]
                .as_str()
                .unwrap()
                .ends_with("To run a command, use `shell`."),
            "{:?}",
            response.data
        );
    }

    #[test]
    fn erased_task_message_steers_agents_to_stop_polling() {
        let message = format_erased_task_message("bash-erased");
        assert!(message.contains("background task row was erased: bash-erased"));
        assert!(message.contains("stop polling this ID"));
        assert!(!message.contains(super::UNKNOWN_TASK_GUIDANCE));
    }

    const VIEW_SESSION: &str = "view-session";
    const VIEW_TASK: &str = "bash-0000000000000031";

    struct ViewFixture {
        ctx: AppContext,
        pin: String,
        stored: serde_json::Value,
        _writing: crate::db::rich_ui::TestViewWritingGuard,
        _root: tempfile::TempDir,
    }

    impl ViewFixture {
        /// A terminal pinned task on disk with its row and view record in
        /// aft.db, as a restart recovery plus terminal upsert would leave it.
        fn terminal() -> Self {
            Self::with_status(true)
        }

        fn with_status(terminal: bool) -> Self {
            let manifest = crate::db::rich_ui::test_view_manifest_for_test();
            let pin = manifest.pins[0].clone();
            let writing = crate::db::rich_ui::enable_test_view_writing_for_test(manifest.clone());
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            fs::create_dir(&project).unwrap();
            let storage = root.path().join("storage");
            let ctx = AppContext::new(
                Box::new(TreeSitterProvider::new()),
                Config {
                    storage_dir: Some(storage.clone()),
                    project_root: Some(project.clone()),
                    harness: Some(Harness::Opencode),
                    ..Config::default()
                },
            );
            let db = std::sync::Arc::new(std::sync::Mutex::new(
                crate::db::open(&root.path().join("aft.db")).unwrap(),
            ));
            ctx.set_db(std::sync::Arc::clone(&db));
            ctx.bash_background()
                .set_db_pool(std::sync::Arc::clone(&db));
            ctx.bash_background().set_harness(Harness::Opencode);
            let task_storage = storage.join(Harness::Opencode.storage_segment());
            let layout = create_task_layout(&task_storage, VIEW_SESSION, VIEW_TASK).unwrap();
            let mut metadata = crate::db::rich_ui::terminal_task_metadata_for_test(Some(&pin));
            metadata.session_id = VIEW_SESSION.into();
            metadata.task_id = VIEW_TASK.into();
            metadata.harness = Some(Harness::Opencode.storage_segment());
            metadata.workdir = project.clone();
            metadata.project_root = Some(project.clone());
            if !terminal {
                metadata.status = BgTaskStatus::Running;
                metadata.finished_at = None;
                metadata.duration_ms = None;
                metadata.exit_code = None;
                metadata.completion_preview = None;
                // Read-only restore probes: no timeout or process group can
                // stand in for a live child, so point at this process.
                metadata.child_pid = Some(std::process::id());
            }
            write_task_at(&layout, &metadata).unwrap();
            fs::write(&layout.paths.stdout, "done\n").unwrap();
            fs::write(&layout.paths.stderr, "").unwrap();
            let row = crate::db::rich_ui::task_row_for_test(
                &metadata,
                &Harness::Opencode.storage_segment(),
                if terminal { "completed" } else { "running" },
                terminal.then_some(7),
                true,
            );
            // The terminal upsert writes the record through the shared
            // wrapper, the same path production uses; a running upsert
            // writes no record.
            let stored = {
                let conn = db.lock().unwrap();
                crate::db::rich_ui::upsert_task_row_with_view(&conn, &row, &metadata).unwrap();
                crate::db::rich_ui::view_record_json(
                    &conn,
                    &Harness::Opencode.storage_segment(),
                    VIEW_SESSION,
                    VIEW_TASK,
                )
                .unwrap()
                .unwrap_or(serde_json::Value::Null)
            };
            Self {
                ctx,
                pin,
                stored,
                _writing: writing,
                _root: root,
            }
        }

        fn status(&self, task_id: &str) -> Response {
            let request: RawRequest = serde_json::from_value(serde_json::json!({
                "id": "view-status", "command": "bash_status", "session_id": VIEW_SESSION,
                "params": { "task_id": task_id },
            }))
            .unwrap();
            super::handle(&request, &self.ctx)
        }
    }

    /// A terminal pinned `bash_status` on a tool-result/v1 route carries the
    /// stored record in `structuredContent.view`, with the same model-visible
    /// content bytes as the legacy reply. A legacy route never receives it.
    #[test]
    fn terminal_bash_status_carries_the_stored_record_on_tool_result_routes() {
        let fixture = ViewFixture::terminal();
        assert!(fixture.stored.is_object(), "record must be stored");
        // A legacy-envelope route never receives the record.
        let legacy = fixture.status(VIEW_TASK);
        assert!(legacy.success, "{legacy:?}");
        assert_eq!(legacy.data["status"], "completed");
        assert!(legacy.data.get("view").is_none());
        // A tool-result/v1 route receives the stored record verbatim.
        let _v1 = crate::subc::result_body::enter(true);
        let response = fixture.status(VIEW_TASK);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["view"], fixture.stored);
        assert_eq!(response.data["view"]["task_id"], VIEW_TASK);
        assert_eq!(response.data["view"]["schema_pin"], fixture.pin);
        // The extra member leaves the model-visible content bytes unchanged.
        let context = crate::subc_format::FormatContext::default();
        let with_view =
            crate::subc_format::format_response_with_context("bash_status", &response, &context);
        let plain = Response::success(&response.id, {
            let mut data = response.data.clone();
            data.as_object_mut().unwrap().remove("view");
            data
        });
        let without_view =
            crate::subc_format::format_response_with_context("bash_status", &plain, &context);
        assert_eq!(with_view, without_view);
    }

    /// A still-running `bash_status` carries no record, even on a
    /// tool-result/v1 route with writing enabled.
    #[test]
    fn running_bash_status_carries_no_view_record() {
        let fixture = ViewFixture::with_status(false);
        assert!(fixture.stored.is_null());
        let _v1 = crate::subc::result_body::enter(true);
        let response = fixture.status(VIEW_TASK);
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["status"], "running");
        assert!(response.data.get("view").is_none());
    }
}
