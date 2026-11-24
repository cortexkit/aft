#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    process::Command,
};

#[cfg(unix)]
struct Env(&'static str, Option<OsString>);
#[cfg(unix)]
impl Env {
    fn set(key: &'static str, value: Option<&OsStr>) -> Self {
        let old = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self(key, old)
    }
}
#[cfg(unix)]
impl Drop for Env {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

#[test]
fn empty_home_uses_userprofile_for_config_and_parent_exclusion() {
    aft_github::gh_shim::composed_test_driver::empty_home_uses_userprofile_for_config_and_parent_exclusion(
        concat!(module_path!(), "::empty_home_uses_userprofile_for_config_and_parent_exclusion").trim_start_matches("aft_engine::"),
        |profile| crate::parent_search::discover(profile, 1).is_none(),
    );
}

#[test]
fn audit_worker_marker_is_injected_from_daemon_jail_state_not_inherited() {
    let _lock = crate::test_env::process_env_lock();
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.github.shim = false;
    config.git.co_author = "off".into();
    let mut environment = std::collections::HashMap::new();
    crate::sandbox_spawn::with_worker_jail(true, || {
        crate::agent_child_env::inject(&config, root.path(), &mut environment, None)
    })
    .unwrap();
    assert_eq!(
        environment
            .get(crate::gh_shim_paths::GH_SHIM_WORKER_ENV)
            .map(String::as_str),
        Some("1")
    );
    crate::sandbox_spawn::with_worker_jail(false, || {
        crate::agent_child_env::inject(&config, root.path(), &mut environment, None)
    })
    .unwrap();
    assert!(!environment.contains_key(crate::gh_shim_paths::GH_SHIM_WORKER_ENV));
}

#[cfg(unix)]
#[test]
fn audit_head_child_adapters_do_not_inherit_worker_markers() {
    let _lock = crate::test_env::process_env_lock();
    let _marker = Env::set(
        crate::gh_shim_paths::GH_SHIM_WORKER_ENV,
        Some(OsStr::new("1")),
    );
    let mut child = Command::new("/bin/sh");
    child.args(["-c", "printf '%s' \"${AFT_GH_SHIM_WORKER-unset}\""]);
    crate::agent_child_env::apply_to_command(&mut child, &std::collections::HashMap::new());
    assert_eq!(child.output().unwrap().stdout, b"unset");
    let root = tempfile::tempdir().unwrap();
    let (pty, _) = crate::sandbox_spawn::pty_command_for_plan(
        &crate::sandbox_spawn::SpawnPlan::Unsandboxed,
        OsStr::new("/bin/sh"),
        &[],
        &root.path().join("task"),
        root.path(),
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert!(pty
        .iter_full_env_as_str()
        .all(|(key, _)| key != crate::gh_shim_paths::GH_SHIM_WORKER_ENV));
}

/// A ticket issued to a background bash command spawned for an agent session
/// relays as that session. The exact plexus body is checked, and plexus's
/// expected request and reply come from its own fixture file.
#[cfg(unix)]
#[test]
fn agent_session_bash_ticket_relays_to_that_session_with_exact_bodies() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config {
        project_root: Some(project.path().to_path_buf()),
        storage_dir: Some(storage.path().to_path_buf()),
        experimental_bash_background: true,
        ..crate::config::Config::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    config.github.shim = true;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let seen = project.path().join("seen-ticket");
    let command = format!(
        "printf %s \"$AFT_GH_SHIM_TICKET\" > '{}'; sleep 2",
        seen.display()
    );
    let response = crate::bash_background::spawn(
        "req-relay",
        "ses-agent",
        &command,
        crate::bash_background::BashShell::Bash,
        std::path::PathBuf::from("/bin/sh"),
        None,
        None,
        crate::bash_background::HardKill::After(std::time::Duration::from_millis(30_000)),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    assert!(response.success, "spawn failed: {:?}", response.data);
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let started = std::time::Instant::now();
    let ticket = loop {
        if let Ok(text) = std::fs::read_to_string(&seen) {
            if !text.is_empty() {
                break text;
            }
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    aft_github::gh_shim_relay::composed_test_driver::assert_spawned_ticket_relays(
        &ticket, &task_id,
    );

    // A ticket from the shared default session is never issued.
    let default = crate::bash_background::spawn(
        "req-default",
        crate::protocol::DEFAULT_SESSION_ID,
        &format!(
            "printf %s \"${{AFT_GH_SHIM_TICKET:-none}}\" > '{}'",
            project.path().join("default-ticket").display()
        ),
        crate::bash_background::BashShell::Bash,
        std::path::PathBuf::from("/bin/sh"),
        None,
        None,
        crate::bash_background::HardKill::After(std::time::Duration::from_millis(30_000)),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    assert!(default.success);
    let default_path = project.path().join("default-ticket");
    let started = std::time::Instant::now();
    while !default_path.exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(std::fs::read_to_string(&default_path).unwrap(), "none");

    let _ = ctx.bash_background().kill(&task_id, "ses-agent");
}
