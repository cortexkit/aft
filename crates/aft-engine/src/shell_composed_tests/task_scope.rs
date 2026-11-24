#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_actor_created_after_daemon_attach_has_named_membership() {
    require_manager();
    let root = tempfile::tempdir().unwrap();
    let app = crate::context::App::default_shared();
    let first = crate::context::AppContext::from_app(app.clone(), crate::config::Config::default());
    assert!(!first.is_daemon_mode());
    app.set_subc_connection_file(root.path().join("connection.json"));
    let second = crate::context::AppContext::from_app(
        app,
        crate::config::Config {
            project_root: Some(root.path().join("second")),
            ..Default::default()
        },
    );
    assert!(second.is_daemon_mode());
    let registry = second.bash_background();
    let id = registry
        .spawn_with_shell_in_slot_typed(
            SpawnPlan::Unsandboxed,
            "echo ready; sleep 60",
            BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "second-root".into(),
            root.path().to_owned(),
            HashMap::from([(LINUX_SCOPE_ENV.into(), "1".into())]),
            HardKill::After(Duration::from_secs(90)),
            None,
            root.path().to_owned(),
            TaskSlot::Background { max: 8 },
            false,
            false,
            Some(root.path().to_owned()),
            true,
        )
        .unwrap();
    let _stop = StopOnDrop::new(id.clone());
    let task = registry.test_task(&id).unwrap();
    until(|| {
        std::fs::read_to_string(&task.paths().stdout)
            .unwrap_or_default()
            .contains("ready")
    });
    let pid = task.lock_state().unwrap().metadata().child_pid.unwrap();
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    registry.kill(&id, "second-root").unwrap();
    assert!(
        matches_membership(&id, &text),
        "new actor missed the shared daemon fact: {text}"
    );
}
use crate::bash_background::{BashShell, HardKill, TaskSlot};
use crate::sandbox_spawn::SpawnPlan;
use aft_shell::bash_background::registry::test_support::scopes::{
    require_manager, until, StopOnDrop,
};
use aft_shell::test_bridge::{
    task_scope::{matches_membership, LINUX_SCOPE_ENV},
    BgTaskRegistryTestExt as _, BgTaskTestExt as _,
};
use std::{collections::HashMap, path::PathBuf, time::Duration};

#[test]
fn task_scope_standalone_fallback_restart_context_is_not_daemon() {
    let root = tempfile::tempdir().unwrap();
    let ctx = crate::shell_test_fixtures::remote::restarted_context(root.path());
    assert!(!ctx.is_daemon_mode());
}
