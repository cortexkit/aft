use crate::bash_background::{
    persistence::resolve_task_layout, spawn, BashShell, BgTaskStatus, HardKill, RemoteLaunch,
};
use crate::shell_test_fixtures::remote::{ensure_git_fixture, launch, terminal};
use aft_shell::bash_background::registry::test_support::memory::{
    persisted_cap, with_prepare_gate, MEMORY_PRESSURE_COMMAND,
};
#[cfg(target_os = "linux")]
use aft_shell::bash_background::registry::test_support::memory::{
    report_memory_runtime_skip, sampling_available_for_test, AttachFaultGuard,
};
use aft_shell::test_bridge::{
    remote_wire::{daemon, Script},
    restore_local_launch, BgTaskTestExt as _,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
pub(super) fn memory_request_context(
    project: &Path,
    storage: &Path,
    cap: Option<u64>,
) -> Arc<crate::context::AppContext> {
    let mut config = crate::config::Config::default();
    config.project_root = Some(project.into());
    config.storage_dir = Some(storage.into());
    config.bash.memory_limit_mib = cap;
    config.sandbox.profile.head = serde_json::from_str("\"off\"").unwrap();
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ))
}

#[cfg(unix)]
pub(super) fn spawn_memory_request(
    ctx: &crate::context::AppContext,
    request_id: &str,
    remote: Option<RemoteLaunch>,
) -> crate::protocol::Response {
    spawn_memory_command(ctx, request_id, "exit 0", false, remote)
}

#[cfg(unix)]
pub(super) fn spawn_memory_command(
    ctx: &crate::context::AppContext,
    request_id: &str,
    command: &str,
    pty: bool,
    remote: Option<RemoteLaunch>,
) -> crate::protocol::Response {
    spawn(
        request_id,
        "session",
        command,
        BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        HardKill::After(Duration::from_secs(30)),
        ctx,
        false,
        false,
        false,
        pty,
        24,
        80,
        Vec::new(),
        false,
        None,
        remote,
    )
}

#[cfg(unix)]
#[test]
fn memory_limit_interleaved_requests_keep_admitted_caps_across_reload() {
    for new_cap in [None, Some(128)] {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = memory_request_context(project.path(), storage.path(), Some(64));
        let original = ctx.config();
        let a_ctx = ctx.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request_a = std::thread::spawn(move || {
            let _pin = a_ctx.pin_config_to(original);
            with_prepare_gate(
                move || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
                },
                || spawn_memory_request(&a_ctx, "memory-request-a", None),
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        let mut reloaded = ctx.config().as_ref().clone();
        reloaded.bash.memory_limit_mib = new_cap;
        ctx.set_config(reloaded);
        let request_b = {
            let _pin = ctx.pin_config();
            spawn_memory_request(&ctx, "memory-request-b", None)
        };
        release_tx.send(()).unwrap();
        let request_a = request_a.join().unwrap();
        assert!(request_a.success, "{request_a:?}");
        assert!(request_b.success, "{request_b:?}");
        let cap_for = |response: &crate::protocol::Response| {
            let task = ctx
                .bash_background()
                .test_task(response.data["task_id"].as_str().unwrap())
                .unwrap();
            let state = task.lock_state().unwrap();
            persisted_cap(state.metadata())
        };
        assert_eq!(
            cap_for(&request_a),
            Some(64),
            "request A lost its admitted cap"
        );
        assert_eq!(
            cap_for(&request_b),
            new_cap,
            "request B inherited another request's cap"
        );
    }
}

#[tokio::test]
async fn memory_limit_delayed_fallback_keeps_submitted_cap_after_reload() {
    assert_delayed_fallback_cap(None).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn memory_limit_attach_eperm_delayed_fallback_after_reload_enforces_watcher() {
    assert_delayed_fallback_cap(Some(libc::EPERM)).await;
}

async fn assert_delayed_fallback_cap(attach_error: Option<i32>) {
    #[cfg(target_os = "linux")]
    if attach_error.is_some() && !sampling_available_for_test() {
        report_memory_runtime_skip(
            "memory_limit_attach_eperm_delayed_fallback_after_reload_enforces_watcher",
            "RSS sampling is unavailable for the forced fallback",
        );
        return;
    }
    let daemon = daemon(Script::GatedAcceptanceThenRefused, "exec-remote/v1").await;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    ensure_git_fixture(project.path());
    let ctx = memory_request_context(project.path(), storage.path(), Some(64));
    let submitted = {
        let _pin = ctx.pin_config();
        spawn_memory_command(
            &ctx,
            "memory-fallback-a",
            if attach_error.is_some() {
                MEMORY_PRESSURE_COMMAND
            } else {
                "exit 0"
            },
            false,
            Some(launch(daemon.connection().clone())),
        )
    };
    assert!(submitted.success, "{submitted:?}");
    let task_id = submitted.data["task_id"].as_str().unwrap().to_owned();
    #[cfg(target_os = "linux")]
    let attach_fault = attach_error.map(|errno| {
        AttachFaultGuard::for_task(task_id.clone(), storage.path().join("denied-cgroup"), errno)
    });
    let (saved_layout, saved_digest) = {
        let task = ctx.bash_background().test_task(&task_id).unwrap();
        let digest = task
            .test_state()
            .lock()
            .unwrap()
            .metadata()
            .remote
            .as_ref()
            .unwrap()
            .fallback_digest
            .clone()
            .unwrap();
        (
            resolve_task_layout(&task.paths().session_dir, &task_id).unwrap(),
            digest,
        )
    };

    let mut reloaded = ctx.config().as_ref().clone();
    reloaded.bash.memory_limit_mib = None;
    ctx.set_config(reloaded);
    let newer = {
        let _pin = ctx.pin_config();
        spawn_memory_request(&ctx, "memory-fallback-b", None)
    };
    assert!(newer.success, "{newer:?}");
    daemon.acceptance_gate().notify_one();
    let done = terminal(ctx.bash_background(), &task_id).await;
    if attach_error.is_some() {
        assert_eq!(done.info.status, BgTaskStatus::Failed, "{done:?}");
        assert!(
            done.info
                .status_reason
                .as_deref()
                .unwrap_or_default()
                .contains("64 MiB memory cap"),
            "{done:?}"
        );
        assert!(
            done.output_preview
                .contains("Linux process-group RSS watcher; kernel memory controller unavailable"),
            "{done:?}"
        );
    } else {
        assert_eq!(done.exit_code, Some(0), "{done:?}");
    }
    #[cfg(target_os = "linux")]
    if let Some(fault) = &attach_fault {
        assert_eq!(fault.attempts(), 1);
        assert!(!storage.path().join("denied-cgroup").exists());
    }
    let task = ctx.bash_background().test_task(&task_id).unwrap();
    let state = task.test_state().lock().unwrap();
    assert!(state.metadata().local_fallback_started);
    assert_eq!(
        persisted_cap(state.metadata()),
        Some(64),
        "fallback lost the cap from its submitted request"
    );
    drop(state);
    let (_, _, _, _, saved_cap, _) = restore_local_launch(&saved_layout, &saved_digest).unwrap();
    assert_eq!(
        saved_cap,
        Some(64),
        "private fallback snapshot lost its admitted cap"
    );
}
