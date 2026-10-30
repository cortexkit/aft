use super::*;
use crate::bash_background::persistence::{with_task_io_fault, TaskIoFault};

#[test]
fn windows_launchers_use_injected_production_retry_policies() {
    fn contains_call(node: tree_sitter::Node<'_>, source: &str, callee: &str) -> bool {
        if node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .is_some_and(|function| function.utf8_text(source.as_bytes()).ok() == Some(callee))
        {
            return true;
        }
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .any(|child| contains_call(child, source, callee));
        found
    }
    fn function_uses(node: tree_sitter::Node<'_>, source: &str, name: &str, callee: &str) -> bool {
        if node.kind() == "function_item"
            && node
                .child_by_field_name("name")
                .is_some_and(|field| field.utf8_text(source.as_bytes()).ok() == Some(name))
        {
            return contains_call(node, source, callee);
        }
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .any(|child| function_uses(child, source, name, callee));
        found
    }
    // Win32 launchers cannot execute on Unix. Inspect actual call expressions
    // in their production bodies; comments and test adapters cannot satisfy this fence.
    for (source, function, callee) in [
        (
            include_str!("registry.rs"),
            "spawn_detached_child",
            "crate::windows_shell::spawn_piped_candidates",
        ),
        (
            include_str!("pty_process.rs"),
            "spawn_pty_for_command",
            "crate::windows_shell::spawn_pty_candidates",
        ),
    ] {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            function_uses(tree.root_node(), source, function, callee),
            "{function} bypasses the tested policy"
        );
    }
}

fn launch(
    registry: &BgTaskRegistry,
    storage: &Path,
    pty: bool,
    cwd: &Path,
) -> Result<String, String> {
    let shell = super::super::resolve_shell_path(pty, super::super::BashShell::Bash).unwrap();
    if pty {
        registry.spawn_pty_with_shell(
            SpawnPlan::Unsandboxed,
            "exit 0",
            super::super::BashShell::Bash,
            shell,
            "launch-test".into(),
            cwd.into(),
            HashMap::new(),
            super::super::HardKill::Default,
            None,
            storage.into(),
            4,
            true,
            false,
            Some(storage.into()),
            24,
            80,
        )
    } else {
        registry.spawn_with_shell_in_slot(
            SpawnPlan::Unsandboxed,
            "exit 0",
            super::super::BashShell::Bash,
            shell,
            "launch-test".into(),
            cwd.into(),
            HashMap::new(),
            super::super::HardKill::Default,
            None,
            storage.into(),
            super::super::TaskSlot::Foreground,
            true,
            false,
            Some(storage.into()),
        )
    }
}

fn assert_failed_handle_setup_leaves_no_layout(pty: bool) {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let result = with_task_io_fault(TaskIoFault::OutputHandlesEnospc, || {
        launch(&registry, storage.path(), pty, storage.path())
    });
    assert!(result.unwrap_err().contains("No space left on device"));
    let session = session_tasks_dir(storage.path(), "launch-test");
    assert_eq!(
        fs::read_dir(session).unwrap().count(),
        0,
        "uncommitted task layout leaked"
    );
    assert!(registry.list(0).is_empty());
}

#[test]
fn precommit_piped_handle_failure_removes_layout() {
    assert_failed_handle_setup_leaves_no_layout(false);
}

#[test]
fn precommit_pty_handle_failure_removes_layout() {
    assert_failed_handle_setup_leaves_no_layout(true);
}

#[cfg(unix)]
#[test]
fn committed_spawn_failure_preserves_visible_record() {
    use crate::bash_background::{with_spawn_receipt, SpawnReceipt, StartupOutcome};
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let receipt = Arc::new(SpawnReceipt::new(Instant::now() + Duration::from_secs(60)));
    let result = with_spawn_receipt(receipt.clone(), || {
        launch(
            &registry,
            storage.path(),
            false,
            &storage.path().join("missing-cwd"),
        )
    });
    assert!(result.is_err());
    let StartupOutcome::Task(task_id) = receipt.expire() else {
        panic!("startup receipt was not committed");
    };
    let session = session_tasks_dir(storage.path(), "launch-test");
    let resolved =
        resolve_task_layout(&session, &task_id).expect("visible failure record was deleted");
    assert_eq!(
        read_task_at(&resolved).unwrap().status,
        BgTaskStatus::Failed
    );
}

fn with_live_task(run: impl FnOnce(&crate::context::AppContext, &str)) {
    let _storage_env = crate::test_env::without_storage_override();
    let storage = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config {
        project_root: Some(storage.path().into()),
        storage_dir: Some(storage.path().into()),
        ..Default::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let request = serde_json::from_value(serde_json::json!({
        "id": "typed-error-spawn", "command": "bash", "session_id": "typed-error",
        "params": { "command": if cfg!(windows) { "ping -n 31 127.0.0.1" } else { "sleep 30" } }
    }))
    .unwrap();
    let launched = crate::commands::bash::handle(&request, &ctx);
    assert!(launched.success, "{:?}", launched.data);
    let id = launched.data["task_id"].as_str().unwrap().to_string();
    struct KillOnDrop<'a>(&'a crate::context::AppContext, String);
    impl Drop for KillOnDrop<'_> {
        fn drop(&mut self) {
            let _ = self.0.bash_background().kill(&self.1, "typed-error");
        }
    }
    let _cleanup = KillOnDrop(&ctx, id.clone());
    run(&ctx, &id);
}

#[test]
fn kill_persistence_failure_is_not_task_absence() {
    with_live_task(|ctx, id| {
        let req = serde_json::from_value(serde_json::json!({
            "id": "typed-kill", "command": "bash_kill", "session_id": "typed-error", "params": { "task_id": id }
        })).unwrap();
        let reply = with_task_io_fault(TaskIoFault::RecordWriteNotFound, || {
            crate::commands::bash_kill::handle(&req, ctx)
        });
        assert_eq!(reply.data["code"], "kill_failed", "{:?}", reply.data);
        assert!(reply.data["message"]
            .as_str()
            .unwrap()
            .contains("failed to persist"));
    });
}

#[test]
fn promote_persistence_failure_is_not_task_absence() {
    with_live_task(|ctx, id| {
        let req = serde_json::from_value(serde_json::json!({
            "id": "typed-promote", "command": "bash_promote", "session_id": "typed-error", "params": { "task_id": id }
        })).unwrap();
        let reply = with_task_io_fault(TaskIoFault::RecordWriteNotFound, || {
            crate::commands::bash_promote::handle(&req, ctx)
        });
        assert_eq!(reply.data["code"], "execution_failed", "{:?}", reply.data);
        assert!(reply.data["message"]
            .as_str()
            .unwrap()
            .contains("failed to promote"));
    });
}

#[test]
fn detach_persistence_failure_is_not_task_absence() {
    with_live_task(|ctx, id| {
        let detached = with_task_io_fault(TaskIoFault::RecordWriteNotFound, || {
            crate::commands::bash_orchestrate::detach_wait_mode_bash(
                ctx,
                id,
                "typed-error",
                "typed-detach",
                false,
                true,
            )
        });
        assert_eq!(
            detached.data["code"], "execution_failed",
            "{:?}",
            detached.data
        );
    });
}

#[test]
fn spawn_storage_error_text_cannot_impersonate_slot_refusal() {
    let _storage_env = crate::test_env::without_storage_override();
    let storage = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config {
        project_root: Some(storage.path().into()),
        storage_dir: Some(storage.path().into()),
        ..Default::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let req = serde_json::from_value(serde_json::json!({
        "id": "typed-spawn", "command": "bash", "session_id": "typed-error", "params": { "command": "exit 0" }
    })).unwrap();
    let reply = with_task_io_fault(TaskIoFault::OutputHandlesMisleadingText, || {
        crate::commands::bash::handle(&req, &ctx)
    });
    assert_eq!(reply.data["code"], "execution_failed", "{:?}", reply.data);
    assert!(reply.data["message"]
        .as_str()
        .unwrap()
        .contains("storage limit exceeded"));
}
