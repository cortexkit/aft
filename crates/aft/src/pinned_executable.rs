pub use aft_base::pinned_executable::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::io;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_executable_unpinned_path_is_current_exe() {
        assert!(PINNED.get().is_none());
        assert_eq!(path().unwrap(), std::env::current_exe().unwrap());
    }

    #[test]
    fn pinned_executable_unpinned_shims_use_storage() {
        assert!(PINNED.get().is_none());
        let storage = tempfile::tempdir().unwrap();
        assert_eq!(
            shims_dir(storage.path()),
            storage.path().join(crate::gh_shim_paths::SHIMS_DIR_NAME)
        );
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

    #[test]
    fn pinned_executable_cleanup_preserves_live_owner_and_unrelated_entries() {
        let storage = tempfile::tempdir().unwrap();
        let run = storage.path();
        let pid = std::process::id();
        let start = crate::process_identity::process_start_time_ms(pid).unwrap();
        let live = run.join(format!("aft-pinned-{pid}-{start}"));
        let reused = run.join(format!("aft-pinned-{pid}-{}", start - 1));
        let unrelated = run.join("operator-state");
        for directory in [&live, &reused, &unrelated] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join("aft"), b"image").unwrap();
        }
        cleanup(run).unwrap();
        assert!(live.join("aft").exists());
        assert!(!reused.exists(), "a reused PID must not retain an old pin");
        assert!(unrelated.join("aft").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_cleanup_preserves_another_live_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let storage = tempfile::tempdir().unwrap();
        let pid = child.id();
        let start = crate::process_identity::process_start_time_ms(pid).unwrap();
        let directory = storage.path().join(format!("aft-pinned-{pid}-{start}"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("aft"), b"live image").unwrap();
        let result = cleanup(storage.path());
        let retained = directory.join("aft").exists();
        let alive = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
        assert!(
            alive && retained,
            "cleanup removed a live process's executable"
        );
        cleanup(storage.path()).unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn pinned_executable_cleanup_lease_overrides_mismatched_process_clock() {
        let storage = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let start = crate::process_identity::process_start_time_ms(pid).unwrap();
        let directory = storage
            .path()
            .join(format!("aft-pinned-{pid}-{}", start - 1));
        std::fs::create_dir(&directory).unwrap();
        let lease = owner_lock(&directory).unwrap();
        cleanup(storage.path()).unwrap();
        assert!(
            directory.exists(),
            "a held lease must override an apparent PID reuse"
        );
        drop(lease);
        cleanup(storage.path()).unwrap();
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_task_image_and_shim_outlive_daemon_pin() {
        let storage = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let pin = prepare(storage.path()).unwrap();
        std::fs::create_dir(pin.directory.join("shims")).unwrap();
        std::os::unix::fs::symlink(&pin.executable, pin.directory.join("shims/gh")).unwrap();
        let image = pin.task_image(task.path()).unwrap();
        pin.cleanup_at_exit();
        assert!(!pin.directory.exists());
        assert_eq!(
            std::fs::read_link(image.join("shims/gh")).unwrap(),
            image.join("aft")
        );
        let output = std::process::Command::new(image.join("shims/gh"))
            .args(["--list", "pinned_executable_cleanup_preserves_live_owner"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 test"));
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

    #[cfg(windows)]
    #[test]
    fn pinned_executable_windows_task_wrapper_outlives_daemon_pin() {
        let storage = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let pin = prepare(storage.path()).unwrap();
        std::fs::create_dir(pin.directory.join("shims")).unwrap();
        std::fs::write(
            pin.directory.join("shims/gh.cmd"),
            crate::gh_shim_paths::windows_gh_cmd(&pin.executable),
        )
        .unwrap();
        let image = pin.task_image(task.path()).unwrap();
        pin.cleanup_at_exit();
        assert!(!pin.directory.exists());
        assert_eq!(
            std::fs::read(image.join("shims/gh.cmd")).unwrap(),
            crate::gh_shim_paths::windows_gh_cmd(&image.join("aft.exe"))
        );
        assert!(image.join("aft.exe").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_missing_pin_is_repaired_from_complete_held_image() {
        use sha2::{Digest, Sha256};
        use std::io::Seek;
        let storage = tempfile::tempdir().unwrap();
        let pin = prepare(storage.path()).unwrap();
        let digest = |path: &Path| {
            let mut hash = Sha256::new();
            io::copy(&mut std::fs::File::open(path).unwrap(), &mut hash).unwrap();
            hash.finalize()
        };
        let original = digest(&pin.executable);
        pin.state
            .lock()
            .unwrap()
            .source
            .seek(io::SeekFrom::End(0))
            .unwrap();
        std::fs::remove_dir_all(&pin.directory).unwrap();
        assert_eq!(pin.path().unwrap(), pin.executable);
        assert_eq!(original, digest(&pin.executable));
        assert!(pin.directory.join(OWNER_LOCK).exists());
        pin.cleanup_at_exit();
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_cleanup_does_not_follow_symlinks() {
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let name = format!("aft-pinned-{}-1", std::process::id());
        let link = storage.path().join(name);
        std::fs::write(outside.path().join("aft"), b"keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        cleanup(storage.path()).unwrap();
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(outside.path().join("aft")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_refuses_redirected_run_directory() {
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), storage.path().join("run")).unwrap();
        assert!(prepare(storage.path()).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_cross_volume_fallback_preserves_signed_bytes_and_executes() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let storage = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut source = std::fs::File::open(&executable).unwrap();
        let destination = storage.path().join("aft");
        materialize(&executable, &mut source, &destination, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        })
        .unwrap();
        let digest = |path: &Path| {
            let mut file = std::fs::File::open(path).unwrap();
            let mut hash = Sha256::new();
            io::copy(&mut file, &mut hash).unwrap();
            hash.finalize()
        };
        assert_eq!(digest(&executable), digest(&destination));
        let copied = std::fs::metadata(&destination).unwrap();
        assert_ne!(source.metadata().unwrap().ino(), copied.ino());
        assert_eq!(copied.permissions().mode() & 0o777, 0o500);
        let output = std::process::Command::new(&destination)
            .args(["--list", "pinned_executable_cleanup_preserves_live_owner"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 test"));
    }

    /// The daemon's own executable keeps exactly one name: its pin is a separate
    /// file, so macOS cannot report the daemon by a pin or task-image path.
    #[cfg(target_os = "macos")]
    #[test]
    fn pinned_executable_macos_pin_is_a_separate_file_from_the_running_image() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let storage = tempfile::tempdir().unwrap();
        let executable = storage.path().join("deploy");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let mut source = std::fs::File::open(&executable).unwrap();
        let destination = storage.path().join("aft");
        materialize(&executable, &mut source, &destination, link_image).unwrap();
        let held = source.metadata().unwrap();
        let pinned = std::fs::metadata(&destination).unwrap();
        assert_ne!(held.ino(), pinned.ino(), "the pin is a hard link");
        assert_eq!(held.nlink(), 1, "the running image gained a second name");
        assert_eq!(pinned.permissions().mode() & 0o777, 0o500);
        assert_eq!(
            std::fs::read(&executable).unwrap(),
            std::fs::read(&destination).unwrap()
        );
        let output = std::process::Command::new(&destination)
            .args(["--list", "pinned_executable_cleanup_preserves_live_owner"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_hard_link_holds_original_inode_after_replacement() {
        use std::os::unix::fs::MetadataExt;
        let storage = tempfile::tempdir().unwrap();
        let source_path = storage.path().join("deploy");
        let destination = storage.path().join("aft");
        std::fs::write(&source_path, b"original image").unwrap();
        let mut source = std::fs::File::open(&source_path).unwrap();
        materialize(&source_path, &mut source, &destination, |from, to| {
            std::fs::hard_link(from, to)
        })
        .unwrap();
        assert_eq!(
            source.metadata().unwrap().ino(),
            destination.metadata().unwrap().ino()
        );
        let replacement = storage.path().join("new-build");
        std::fs::write(&replacement, b"replacement image").unwrap();
        std::fs::rename(replacement, &source_path).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original image");
        assert_eq!(std::fs::read(&source_path).unwrap(), b"replacement image");
    }
}
