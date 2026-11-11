use super::*;
use std::time::{Duration, Instant};

struct StopProjectTasksOnDrop<'a> {
    ctx: &'a crate::context::AppContext,
    project_root: &'a Path,
}

impl Drop for StopProjectTasksOnDrop<'_> {
    fn drop(&mut self) {
        self.ctx
            .bash_background()
            .kill_running_tasks_for_root(self.project_root);
    }
}

fn exercise(kind: &str, shim_enabled: bool, service_available: bool) {
    exercise_profile(
        kind,
        crate::config::SandboxMode::Strict,
        shim_enabled,
        service_available,
    );
}

/// Launch one jailed worker command under `mode` and check whether the read
/// broker reached it. The broker is expected exactly when a daemon scope
/// service is available, the shim is enabled, and the profile protects the
/// shared task-temp root: strict anywhere, relaxed only on macOS. Every
/// absence must be logged at warn level with its reason.
fn exercise_profile(
    kind: &str,
    mode: crate::config::SandboxMode,
    shim_enabled: bool,
    service_available: bool,
) {
    use crate::config::SandboxMode;
    use crate::sandbox_spawn::{with_spawn_plan_for_test, with_worker_jail, SpawnPlan};
    let profile_allows_broker = match mode {
        SandboxMode::Strict => true,
        SandboxMode::Relaxed => cfg!(target_os = "macos"),
        SandboxMode::Off => false,
    };
    let broker_expected = service_available && shim_enabled && profile_allows_broker;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let temp = project.path().join("000000000001");
    std::fs::create_dir(&temp).unwrap();
    let marker = project.path().join("command-ran");
    let release = project.path().join("release");
    let launcher_profile = project.path().join("observed-profile.json");
    // This fixture observes the production spawn/PTY adapters, not OS isolation.
    // A real native jail is covered separately by the unchanged security suite.
    std::fs::write(
        project.path().join("sandbox-launch"),
        format!(
        "printf '%s' \"$2\" > '{}'; while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"\n",
        launcher_profile.display(),
    ),
    )
    .unwrap();
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        vec![project.path().to_owned()],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        vec![],
        temp.clone(),
    )
    .unwrap();
    profile.mode = mode;
    let plan = SpawnPlan::launcher_for_test(profile, PathBuf::from("/bin/sh"));
    let ctx = spawn_test_context(project.path(), storage.path());
    ctx.update_config(|config| {
        config.github.shim = shim_enabled;
        config.sandbox.profile.worker = mode;
        config.git.co_author = "off".into();
    });
    // Assertions can unwind before a response exposes the task ID, so teardown
    // must stop every command this fixture launched under its temporary root.
    let _cleanup = StopProjectTasksOnDrop {
        ctx: &ctx,
        project_root: project.path(),
    };
    let mut command = if broker_expected {
        format!("test -S \"$AFT_GH_READ_SOCKET\" && test -r \"$AFT_GH_READ_CAPABILITY_FILE\" && test -d \"$AFT_GH_SHIM_STATE_DIR\" && test -z \"${{AFT_GH_SHIM_TICKET+x}}\" && printf BROKER_PROVIDED > '{}'", marker.display())
    } else {
        format!("test -z \"${{AFT_GH_READ_SOCKET+x}}\" && test -z \"${{AFT_GH_READ_CAPABILITY_FILE+x}}\" && test -z \"${{AFT_GH_SHIM_TICKET+x}}\" && test -z \"${{AFT_GH_SHIM_STATE_DIR+x}}\" && printf BROKER_ABSENT > '{}'", marker.display())
    };
    if kind == "promoted" {
        command.push_str(&format!(
            "; {}",
            crate::test_gate::gate(&format!("'{}'", release.display()), "sleep .02")
        ));
    }
    let mut request =
        spawn_test_request(&format!("optional-{kind}"), &command, kind == "background");
    if kind == "pty" {
        request.params["pty"] = json!(true);
    }
    let run = || {
        let (response, logs) = crate::logging::capture_log_lines(|| {
            with_worker_jail(true, || {
                with_spawn_plan_for_test(plan, || handle(&request, &ctx))
            })
        });
        if !broker_expected {
            let reason = if mode == SandboxMode::Off {
                "reason=sandbox profile off runs the worker shell without the worker jail"
            } else if !profile_allows_broker {
                "reason=sandbox profile relaxed on Linux does not protect the shared task-temp root"
            } else if !shim_enabled {
                "shim is disabled"
            } else {
                "no daemon scope service"
            };
            assert!(
                logs.iter().any(|line| line.starts_with("WARN ")
                    && line.contains("gh_shim worker_read unavailable")
                    && line.contains("command continues without broker authority")
                    && line.contains(reason)),
                "{logs:?}"
            );
        }
        assert!(
            response.success,
            "{kind}: optional broker must not prevent startup: {response:?}"
        );
        let task = response.data["task_id"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let expected = if broker_expected {
            "BROKER_PROVIDED"
        } else {
            "BROKER_ABSENT"
        };
        while std::fs::read_to_string(&marker).ok().as_deref() != Some(expected) {
            assert!(
                Instant::now() < deadline,
                "{kind}: child did not run: {:?}",
                ctx.bash_background()
                    .status(task, "sandbox-spawn-test", None, None, 4096)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), expected);
        let observed: crate::sandbox_profile::SandboxProfile =
            serde_json::from_slice(&std::fs::read(&launcher_profile).unwrap()).unwrap();
        assert_eq!(
            observed.mode, mode,
            "broker absence must not downgrade confinement"
        );
        if broker_expected {
            assert!(temp.join(".gh-read-capability").is_file());
            assert!(temp.join(".gh-shim").is_dir());
        } else {
            assert!(!temp.join(".gh-read-capability").exists());
            assert!(!temp.with_file_name("000000000001.gh-read.sock").exists());
        }
        if kind == "promoted" {
            assert!(ctx
                .bash_background()
                .promote(task, "sandbox-spawn-test")
                .unwrap());
            assert!(
                !temp.join(".gh-read-capability").exists(),
                "promotion must not mint a grant"
            );
            std::fs::write(&release, "release").unwrap();
        }
        stop_spawned_test_task(&ctx, &response);
    };
    if service_available {
        crate::gh_shim_worker_broker::with_available_service_for_test(project.path(), run);
    } else {
        crate::gh_shim_worker_broker::without_service_for_test(run);
    }
}

#[test]
fn optional_worker_broker_foreground_without_service_runs() {
    exercise("foreground", true, false);
}
#[test]
fn optional_worker_broker_background_without_service_runs() {
    exercise("background", true, false);
}
#[test]
fn optional_worker_broker_pty_without_service_runs() {
    exercise("pty", true, false);
}
#[test]
fn optional_worker_broker_promoted_without_service_runs() {
    exercise("promoted", true, false);
}
#[test]
fn optional_worker_broker_disabled_shim_runs() {
    exercise("foreground", false, false);
}
#[tokio::test]
async fn optional_worker_broker_spawn_with_service_is_still_provided() {
    let _state = crate::test_env::gh_shim_state_guard();
    exercise("foreground", true, true);
}
/// Relaxed macOS workers get the broker. Relaxed Linux workers do not, because
/// relaxed Linux leaves the shared task-temp root open to other workers; the
/// command still runs and the skip is logged with that reason.
#[tokio::test]
async fn optional_worker_broker_relaxed_worker_is_provided_on_macos_only() {
    let _state = crate::test_env::gh_shim_state_guard();
    exercise_profile(
        "foreground",
        crate::config::SandboxMode::Relaxed,
        true,
        true,
    );
}
#[tokio::test]
async fn optional_worker_broker_relaxed_background_worker_is_provided_on_macos_only() {
    let _state = crate::test_env::gh_shim_state_guard();
    exercise_profile(
        "background",
        crate::config::SandboxMode::Relaxed,
        true,
        true,
    );
}
#[test]
fn optional_worker_broker_relaxed_without_service_runs() {
    exercise_profile(
        "foreground",
        crate::config::SandboxMode::Relaxed,
        true,
        false,
    );
}
/// An off worker has no worker jail, so it runs without a broker even when the
/// broker service is available, and logs why at warn level.
#[tokio::test]
async fn optional_worker_broker_off_worker_is_withheld_and_logged() {
    let _state = crate::test_env::gh_shim_state_guard();
    exercise_profile("foreground", crate::config::SandboxMode::Off, true, true);
}
