use crate::bash_background::registry::BgTaskRegistry;
use crate::bash_background::{self, BashShell, HardKill, TaskSlot};
use crate::sandbox_spawn::SpawnPlan;
use aft_engine::test_bridge::bash_remote::registry_support::scopes::{
    require_manager, scope_stopped, until, StopOnDrop,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires a systemd user manager"]
fn task_scope_fixed_backstop_works_without_registry() {
    require_manager();
    let root = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.configure_task_scope_runtime(true, Duration::from_secs(1));
    let id = registry
        .spawn_with_shell_in_slot_typed(
            SpawnPlan::Unsandboxed,
            "echo alive > alive; sleep 60",
            BashShell::Bash,
            PathBuf::from("/bin/bash"),
            "task-scope".into(),
            root.path().into(),
            HashMap::new(),
            HardKill::Default,
            None,
            root.path().into(),
            TaskSlot::Background { max: 4 },
            true,
            false,
            Some(root.path().into()),
            true,
        )
        .unwrap();
    let _stop = StopOnDrop(id.clone());
    let scope = bash_background::task_scope::lookup(&id).unwrap().unwrap();
    until(|| root.path().join("alive").exists());
    assert_eq!(
        registry
            .hard_kill_deadline(&id, "task-scope")
            .unwrap()
            .limit_ms,
        1000,
        "reply ignored the earlier scope deadline"
    );
    let note =
        crate::commands::bash_orchestrate::kill_deadline_note(&registry, &id, "task-scope", true);
    assert!(
        !note.contains("so it is not killed while you keep waiting"),
        "{note}"
    );
    registry.detach();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !scope_stopped(&scope.cgroup().join("cgroup.events")) {
        assert!(
            Instant::now() < deadline,
            "fixed scope backstop did not stop the detached task"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
