#[cfg(all(test, unix))]
mod spawn_reply_tests {
    use aft_engine::test_bridge::bash_remote::successful_spawn_response;
    use serde_json::json;

    #[test]
    fn spawn_reply_reports_observed_terminal_state() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            ..crate::config::Config::default()
        };
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let request = serde_json::from_value(json!({
            "id": "spawn-reply-test",
            "command": "bash",
            "session_id": "spawn-reply-session",
            "params": { "command": "sleep 30" },
        }))
        .unwrap();
        let launched = crate::commands::bash::handle(&request, &ctx);
        assert!(launched.success, "{:?}", launched.data);
        let task_id = launched.data["task_id"].as_str().unwrap();
        assert_eq!(launched.data["status"], "running");
        ctx.bash_background()
            .kill(task_id, "spawn-reply-session")
            .unwrap();
        // Finish the task before constructing the reply, rather than relying on
        // a fast child to win a race with spawn registration.
        let reply = successful_spawn_response("reply", "spawn-reply-session", task_id, false, &ctx);
        assert_eq!(reply.data["status"], "killed");
        assert_eq!(reply.data["task_id"], task_id);
        assert_eq!(reply.data["mode"], "pipes");

        let terminal =
            match crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, reply) {
                crate::response_finalize::DispatchOutcome::Immediate(response) => response,
                crate::response_finalize::DispatchOutcome::Deferred(mut pending) => {
                    (pending.poll)(&ctx).expect("task already terminal")
                }
            };
        assert_eq!(terminal.data["status"], "killed");
        assert!(terminal.data["output"].is_string(), "{terminal:?}");
    }
}
