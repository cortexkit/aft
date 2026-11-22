mod tests {
    use crate::bash_background::persistence::{write_task, BgMode, ExitMarker, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    use aft_engine::test_bridge::bash_remote::BgBuffer;
    use aft_engine::test_bridge::bash_remote::{registry_support::registry_cases::*, TaskRuntime};
    use aft_engine::test_bridge::bash_remote::{
        BgTaskRegistryTestExt as _, TaskRuntimeTestExt as _,
    };
    use std::{fs, path::PathBuf};

    #[test]
    fn approved_head_status_paging_does_not_renew_default_kill() {
        let (_dir, ctx, task) = crate::tools_test_support::wait_fixture(None);
        fs::write(&task.paths().stdout, "output").unwrap();
        fs::write(&task.paths().stderr, "").unwrap();
        let request = serde_json::from_value(serde_json::json!({"id": "head-page", "command": "bash_status", "session_id": task.session_id(), "worker_session": false, "params": {"task_id": task.task_id(), "output_offset": 0}})).unwrap();
        let response = crate::commands::bash_status::handle(&request, &ctx);
        assert!(response.success, "status failed: {:?}", response.data);
        assert_eq!(
            ctx.bash_background()
                .hard_kill_deadline(task.task_id(), task.session_id())
                .unwrap()
                .limit_ms,
            1_800_000
        );
    }

    #[test]
    fn approved_pty_offset_status_does_not_also_return_entire_raw_log() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::context::AppContext::from_app(
            crate::test_app(),
            crate::config::Config {
                project_root: Some(dir.path().into()),
                storage_dir: Some(dir.path().into()),
                ..Default::default()
            },
        );
        let (id, task) =
            insert_terminal_piped_task(ctx.bash_background(), &dir, "true", "", "", false);
        {
            let mut state = task.test_lock_state().unwrap();
            state.metadata_mut().mode = BgMode::Pty;
            *state.runtime_mut() = TaskRuntime::empty_pty();
            *state.buffer_mut() = BgBuffer::registered(&task.paths(), BgMode::Pty);
            write_task(&task.paths().json, &state.metadata()).unwrap();
        }
        fs::write(&task.paths().pty, vec![b'x'; 3 * 64 * 1024]).unwrap();
        let request = serde_json::from_value(serde_json::json!({"id": "pty-page", "command": "bash_status", "session_id": "session", "params": {"task_id": id, "output_offset": 0, "output_mode": "raw"}})).unwrap();
        reset_full_artifact_read_calls();
        let response = crate::commands::bash_status::handle(&request, &ctx);
        assert!(response.success, "status failed: {:?}", response.data);
        assert_eq!(response.data["pty_raw"].as_str().unwrap().len(), 64 * 1024);
        assert_eq!(response.data["output_next_offset"], 65536);
        assert_eq!(response.data["output_has_more"], true);
        assert_eq!(
            full_artifact_read_calls(),
            0,
            "paged status also replayed the whole PTY artifact"
        );
    }

    #[test]
    fn lifecycle_dropping_deferred_response_releases_wait_registration() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::context::AppContext::from_app(
            crate::test_app(),
            crate::config::Config {
                project_root: Some(dir.path().to_path_buf()),
                storage_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        );
        let task = lifecycle_running_fixture(ctx.bash_background(), &dir);
        let request = serde_json::from_value(serde_json::json!({"id": "wait-drop", "command": "bash", "session_id": "session", "params": {"wait": true}})).unwrap();
        let outcome = crate::commands::bash_orchestrate::build_bash_outcome(
            &request,
            &ctx,
            crate::protocol::Response::success(
                "wait-drop",
                serde_json::json!({"task_id": task.task_id(), "status": "running"}),
            ),
        );
        assert!(matches!(
            outcome,
            crate::response_finalize::DispatchOutcome::Deferred(_)
        ));
        assert_eq!(ctx.bash_background().active_wait_session_count(), 1);
        drop(outcome);
        assert_eq!(ctx.bash_background().active_wait_session_count(), 0);
        assert_eq!(ctx.bash_background().abort_inflight("session").unwrap(), 0);
    }

    #[test]
    fn pty_unknown_exit_marker_has_no_exit_code_and_preserves_real_kills() {
        let mut metadata = PersistedTask::starting(
            "bash-lost".to_string(),
            "session".to_string(),
            "interactive".to_string(),
            PathBuf::from("."),
            None,
            None,
            true,
            true,
        );
        metadata.mode = BgMode::Pty;
        let terminal = terminal_metadata_from_marker(metadata.clone(), ExitMarker::Unknown, None);
        assert_eq!(terminal.status, BgTaskStatus::FateUnknown);
        assert_eq!(terminal.exit_code, None);
        assert_eq!(
            crate::subc_format::format_bash_task_status_header(
                &terminal.task_id,
                "fate_unknown",
                terminal.exit_code,
                terminal.status_reason.as_deref(),
            ),
            "Task bash-lost: exit status unknown (AFT could not read how it ended)"
        );
        let killed =
            terminal_metadata_from_marker(metadata, ExitMarker::Killed, Some("bash_kill".into()));
        assert_eq!(killed.status, BgTaskStatus::Killed);
        assert_eq!(killed.exit_code, Some(137));
        assert_eq!(killed.status_reason.as_deref(), Some("bash_kill"));
        assert_eq!(
            crate::subc_format::format_bash_task_status_header(
                &killed.task_id,
                "killed",
                killed.exit_code,
                killed.status_reason.as_deref(),
            ),
            "Task bash-lost: killed (exit 137)"
        );
    }
}
