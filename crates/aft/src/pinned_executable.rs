pub use aft_base::pinned_executable::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::OnceLock;

#[cfg(test)]
#[allow(hidden_glob_reexports)]
mod tests {
    #[cfg(unix)]
    use super::*;

    // pinned_executable_unsandboxed_background_shim_outlives_daemon_pin uses --list
    // with this name to verify its copied aft test executable still runs.
    #[test]
    fn pinned_executable_cleanup_preserves_live_owner_and_unrelated_entries() {
        aft_base::pinned_executable::tests::pinned_executable_cleanup_preserves_live_owner_and_unrelated_entries();
    }

    #[test]
    fn pinned_executable_explicit_gh_binary_path_wins() {
        let mut config = crate::config::Config::default();
        let explicit = std::env::current_exe()
            .unwrap()
            .with_file_name("operator-aft");
        config.gh_shim.binary_path = Some(explicit.clone());
        assert_eq!(
            crate::agent_child_env::shim_binary(&config).unwrap(),
            explicit
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_unsandboxed_background_shim_outlives_daemon_pin() {
        use crate::bash_background::{
            persistence, registry::BgTaskRegistry, BashShell, HardKill, TaskSlot,
        };
        use crate::sandbox_spawn::SpawnPlan;
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        const CHILD: &str = "AFT_TEST_PIN_TASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // PINNED is process-global; exercise the normal task-launch path without
            // changing executable discovery for other tests in this process.
            let fixture = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args([
                "--exact",
                "pinned_executable::tests::pinned_executable_unsandboxed_background_shim_outlives_daemon_pin",
                "--nocapture",
            ]);
            for key in [
                "HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
                "AFT_STORAGE_DIR",
            ] {
                let directory = fixture.path().join(key);
                std::fs::create_dir(&directory).unwrap();
                child.env(key, directory);
            }
            let output = child.env(CHILD, "1").output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }

        let storage = PathBuf::from(std::env::var_os("AFT_STORAGE_DIR").unwrap());
        PINNED
            .set(prepare(&storage).unwrap())
            .ok()
            .expect("isolated pin");
        let pin = PINNED.get().unwrap();
        std::fs::create_dir(pin.directory.join("shims")).unwrap();
        std::os::unix::fs::symlink(&pin.executable, pin.directory.join("shims/gh")).unwrap();
        let registry = BgTaskRegistry::default();
        struct Shutdown<'a>(&'a BgTaskRegistry);
        impl Drop for Shutdown<'_> {
            fn drop(&mut self) {
                self.0.shutdown();
            }
        }
        let _shutdown = Shutdown(&registry);
        let mut tasks = Vec::new();
        for mode in ["background", "promoted", "pty", "host"] {
            let environment = HashMap::from([
                (
                    "PATH".to_owned(),
                    std::env::join_paths([
                        pin.directory.join("shims"),
                        storage.join("run/aft-pinned-99-99/shims"),
                        PathBuf::from("/usr/bin"),
                        PathBuf::from("/bin"),
                    ])
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ),
                (
                    crate::gh_shim_paths::GH_SHIMS_DIR_ENV.to_owned(),
                    pin.directory.join("shims").to_string_lossy().into_owned(),
                ),
                (
                    crate::gh_shim_paths::GH_SHIM_BINARY_ENV.to_owned(),
                    pin.executable.to_string_lossy().into_owned(),
                ),
            ]);
            let command = format!(
                ": > ready-{mode}; {}; \
                 printf '%s\\n' \"$(command -v gh)\" \"$AFT_GH_SHIMS_DIR\" \"$AFT_GH_SHIM_BINARY\" \"$PATH\" > resolved-{mode}; \
                 gh --list pinned_executable_cleanup_preserves_live_owner > output-{mode} 2>&1; \
                 shim_status=$?; \"$AFT_GH_SHIM_BINARY\" --list pinned_executable_cleanup_preserves_live_owner > binary-output-{mode} 2>&1; \
                 printf '%s:%s' \"$shim_status\" \"$?\" > status-{mode}.tmp; /bin/mv status-{mode}.tmp status-{mode}",
                crate::test_gate::gate("release", "/bin/sleep 0.02")
            );
            let plan = if mode == "host" {
                use crate::sandbox_spawn::{
                    AuthenticatedPrincipal, HostEscalationAttempt, RequestedSandboxTier,
                    SandboxTaskKind,
                };
                let mut config = crate::config::Config {
                    project_root: Some(storage.clone()),
                    storage_dir: Some(storage.clone()),
                    ..crate::config::Config::default()
                };
                config.sandbox.profile.head = crate::config::SandboxMode::Strict;
                let ctx = crate::context::AppContext::new(
                    Box::new(crate::parser::TreeSitterProvider::new()),
                    config,
                );
                let approved = environment
                    .iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect();
                let attempt = HostEscalationAttempt {
                    grant_id: crate::sandbox_spawn::mint_host_escalation_grant(
                        &ctx,
                        &AuthenticatedPrincipal::FirstParty,
                        command.as_bytes(),
                        &storage,
                        &storage,
                        Path::new("/bin/bash"),
                        &approved,
                        &storage,
                        "pin-lifetime",
                    )
                    .unwrap(),
                    command: command.as_bytes().to_vec(),
                    root: storage.clone(),
                    cwd: storage.clone(),
                    shell_path: PathBuf::from("/bin/bash"),
                    environment: approved,
                };
                let plan = crate::sandbox_spawn::resolve_sandbox_spawn(
                    &ctx,
                    &AuthenticatedPrincipal::FirstParty,
                    RequestedSandboxTier::Host,
                    SandboxTaskKind::BashBackground,
                    &storage,
                    Some(&attempt),
                );
                assert!(
                    plan.refusal_code().is_none(),
                    "host grant rejected: {plan:?}"
                );
                plan
            } else {
                SpawnPlan::Unsandboxed
            };
            let task_id = if mode == "pty" {
                registry.spawn_pty_with_shell(
                    plan,
                    &command,
                    BashShell::Bash,
                    PathBuf::from("/bin/bash"),
                    "pin-lifetime".into(),
                    storage.clone(),
                    environment,
                    HardKill::Default,
                    None,
                    storage.clone(),
                    8,
                    false,
                    false,
                    Some(storage.clone()),
                    24,
                    80,
                )
            } else {
                registry.spawn_with_shell_in_slot(
                    plan,
                    &command,
                    BashShell::Bash,
                    PathBuf::from("/bin/bash"),
                    "pin-lifetime".into(),
                    storage.clone(),
                    environment,
                    HardKill::Default,
                    None,
                    storage.clone(),
                    if mode == "promoted" {
                        TaskSlot::Foreground
                    } else {
                        TaskSlot::Background { max: 8 }
                    },
                    false,
                    false,
                    Some(storage.clone()),
                )
            }
            .unwrap();
            if mode == "promoted" {
                assert!(registry.promote(&task_id, "pin-lifetime").unwrap());
            }
            let session = persistence::session_tasks_dir(&storage, "pin-lifetime");
            let task = persistence::resolve_task_layout(&session, &task_id).unwrap();
            tasks.push((mode, task_id, task.paths.io_dir));
        }
        let wait_for_file = |name: &str| {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !storage.join(name).is_file() {
                assert!(Instant::now() < deadline, "task never wrote {name}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        for (mode, _, task_io) in &tasks {
            wait_for_file(&format!("ready-{mode}"));
            registry.cleanup_finished(Duration::ZERO);
            assert!(task_io.join(TASK_IMAGE_DIR).is_dir(), "{mode}");
        }
        pin.cleanup_at_exit();
        assert!(!pin.directory.exists());
        std::fs::write(storage.join("release"), b"go").unwrap();
        for (mode, task_id, task_io) in tasks {
            wait_for_file(&format!("status-{mode}"));
            assert_eq!(
                std::fs::read_to_string(storage.join(format!("status-{mode}"))).unwrap(),
                "0:0",
                "{mode}: {:?}",
                std::fs::read_to_string(storage.join(format!("output-{mode}")))
            );
            let image = task_io.join(TASK_IMAGE_DIR);
            assert_eq!(
                std::fs::read_to_string(storage.join(format!("resolved-{mode}"))).unwrap(),
                format!(
                    "{}\n{}\n{}\n{}:/usr/bin:/bin\n",
                    image.join("shims/gh").display(),
                    image.join("shims").display(),
                    image.join("aft").display(),
                    image.join("shims").display()
                ),
                "{mode}",
            );
            assert!(
                std::fs::read_to_string(storage.join(format!("output-{mode}")))
                    .unwrap()
                    .contains("1 test"),
                "{mode} did not execute the retained shim"
            );
            assert!(
                std::fs::read_to_string(storage.join(format!("binary-output-{mode}")))
                    .unwrap()
                    .contains("1 test"),
                "{mode} did not execute the retained hook binary"
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            while image.exists() {
                assert!(
                    Instant::now() < deadline,
                    "{mode} retained its startup image"
                );
                registry
                    .status(&task_id, "pin-lifetime", None, None, 0)
                    .unwrap();
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
