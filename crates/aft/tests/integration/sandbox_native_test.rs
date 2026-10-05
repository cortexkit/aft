#![cfg(unix)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
fn landlock_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new(env!("CARGO_BIN_EXE_aft"))
            .args(["sandbox-launch", "--support"])
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

macro_rules! skip_if_landlock_absent {
    () => {
        #[cfg(target_os = "linux")]
        if !landlock_available() {
            eprintln!("Landlock integration skipped: mandatory ABI is unavailable");
            return;
        }
    };
}

fn configure_native_policy(
    aft: &mut AftProcess,
    project: &Path,
    storage: &Path,
    enabled: bool,
    write_allow: &[PathBuf],
    read_deny: &[PathBuf],
) -> Value {
    aft.send(
        &json!({
            "id": format!("configure-native-{enabled}"),
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "bash_permissions": true,
            "config": user_config(json!({
                "bash": { "background": true, "rewrite": true },
                "sandbox": {
                    "enabled": enabled,
                    "write_allow": write_allow,
                    "read_deny": read_deny,
                }
            })),
        })
        .to_string(),
    )
}

fn configure_native(aft: &mut AftProcess, project: &Path, storage: &Path, enabled: bool) -> Value {
    configure_native_policy(aft, project, storage, enabled, &[], &[])
}

fn foreground(aft: &mut AftProcess, id: &str, command: &str) -> Value {
    aft.send(
        &json!({
            "id": id,
            "method": "bash",
            "session_id": "native-sandbox-session",
            "params": {
                "command": command,
                "foreground_orchestrate": true,
                "permissions_requested": true,
                "compressed": false,
            },
        })
        .to_string(),
    )
}

/// Exercise production spawn planning with a private HOME, including the layout
/// used by background worktree managers. No fixture path refers to the real HOME.
struct CortexkitFloorFixture {
    aft: AftProcess,
    project: PathBuf,
    storage: PathBuf,
    home: PathBuf,
    connection: PathBuf,
    _root: tempfile::TempDir,
}

impl CortexkitFloorFixture {
    fn new() -> Self {
        Self::with_private_shim_image(false)
    }

    fn with_private_shim_image(copy_shim: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().canonicalize().unwrap().join("home");
        let data = home.join(".local/share/cortexkit");
        let project = data.join("alfonso/worktrees/own");
        let storage = data.join("aft");
        let cache = storage.join("cache");
        let connection = home.join("daemon/connection.json");
        for path in [&project, &storage, &cache, connection.parent().unwrap()] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(&connection, "daemon-token").unwrap();
        let shim_image = if copy_shim {
            let image = data.join("bin/ck-aft");
            std::fs::create_dir_all(image.parent().unwrap()).unwrap();
            // The write-denial probe must not risk corrupting the real test
            // executable if the sandbox regresses or is deliberately mutated.
            std::fs::copy(env!("CARGO_BIN_EXE_aft"), &image).unwrap();
            Some(image)
        } else {
            None
        };
        let mut aft = AftProcess::spawn_with_env(&[
            ("HOME", home.as_os_str()),
            ("XDG_DATA_HOME", home.join(".local/share").as_os_str()),
            ("XDG_STATE_HOME", home.join(".local/state").as_os_str()),
            ("XDG_CONFIG_HOME", home.join(".config").as_os_str()),
            ("XDG_CACHE_HOME", home.join(".cache").as_os_str()),
            ("AFT_CACHE_DIR", cache.as_os_str()),
            ("SUBC_CONNECTION_FILE", std::ffi::OsStr::new("")),
        ]);
        let configured = aft.send(
            &json!({
                "id": "configure-cortexkit-floor", "command": "configure",
                "harness": "opencode", "project_root": project, "storage_dir": storage,
                "bash_permissions": true,
                "config": user_config(json!({
                    "bash": { "background": true, "rewrite": true, "shell": "/bin/bash" },
                    "sandbox": { "enabled": true },
                    "subc": { "connection_file": "~/daemon/connection.json" },
                "github": { "shim": true },
                "gh_shim": { "binary_path": shim_image },
                    "git": { "co_author": "Sandbox Test <sandbox@example.invalid>" },
                })),
            })
            .to_string(),
        );
        assert_eq!(configured["success"], true, "{configured:?}");
        Self {
            aft,
            project,
            storage,
            home,
            connection,
            _root: root,
        }
    }

    fn denied_read(&mut self, path: PathBuf) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "private-sentinel").unwrap();
        // First prove the sandbox actually launched and captured a child, rather
        // than accepting a setup refusal as evidence of a denied file read.
        let response = foreground(
            &mut self.aft,
            "floor-read",
            &format!("printf child-ready; cat {}", quote(&path)),
        );
        assert_eq!(
            response["status"], "failed",
            "read was not denied: {response:?}"
        );
        let output = response["output"].as_str().unwrap();
        assert!(
            output.contains("child-ready"),
            "child did not launch: {response:?}"
        );
        assert!(
            !output.contains("private-sentinel"),
            "secret leaked: {response:?}"
        );
        assert!(
            output.contains("Operation not permitted") || output.contains("Permission denied"),
            "not a kernel denial: {response:?}"
        );
    }
}

#[test]
fn cortexkit_floor_connection_outside_data_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    f.denied_read(f.connection.clone());
}

#[test]
fn cortexkit_floor_default_connection_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    f.denied_read(
        f.home
            .join(".local/share/cortexkit/run/subc-connection.json"),
    );
}

#[test]
fn cortexkit_floor_other_module_store_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    f.denied_read(f.home.join(".local/share/cortexkit/another-module/store"));
}

#[test]
fn cortexkit_floor_aft_history_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    f.denied_read(f.storage.join("undo/private-snapshot"));
}

#[test]
fn cortexkit_floor_other_worktree_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    f.denied_read(
        f.home
            .join(".local/share/cortexkit/alfonso/worktrees/other/private"),
    );
}

#[test]
fn cortexkit_floor_state_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    f.denied_read(f.home.join(".local/state/cortexkit/gh-shim/manifest.json"));
}

#[test]
fn cortexkit_floor_other_task_io_is_denied() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    let other = foreground(&mut f.aft, "other-task", "printf other-task");
    assert_eq!(other["status"], "completed", "{other:?}");
    f.denied_read(PathBuf::from(other["output_path"].as_str().unwrap()));
}

#[test]
fn cortexkit_floor_own_worktree_is_readable_and_writable() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    let path = f.project.join("own-file");
    std::fs::write(&path, "before").unwrap();
    let response = foreground(
        &mut f.aft,
        "own-worktree",
        &format!(
            "cat {}; printf after > {}; cat {}",
            quote(&path),
            quote(&path),
            quote(&path)
        ),
    );
    assert_eq!(response["status"], "completed", "{response:?}");
    assert!(
        response["output"].as_str().unwrap().contains("beforeafter"),
        "{response:?}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
}

#[test]
fn cortexkit_floor_shim_and_managed_hooks_execute_read_only() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::with_private_shim_image(true);
    assert!(std::process::Command::new("git")
        .args(["init", "-q", "--template="])
        .current_dir(&f.project)
        .env("HOME", &f.home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success());
    let response = foreground(&mut f.aft, "governed-child", "gh --status && git init -q --template= && printf tracked > tracked && git add tracked && git -c user.name='AFT Test' -c user.email=aft@example.invalid commit -qm initial && git log -1 --format=%B");
    assert_eq!(response["status"], "completed", "{response:?}");
    let output = response["output"].as_str().unwrap();
    assert!(output.contains("rung"), "shim status missing: {response:?}");
    assert!(
        output.contains("Co-authored-by: Sandbox Test <sandbox@example.invalid>"),
        "managed hook did not run: {response:?}"
    );
    let denied = foreground(&mut f.aft, "governed-writes", "printf tampered >> \"$AFT_GH_SHIMS_DIR/gh\"; printf tampered >> \"$GIT_CONFIG_VALUE_0/prepare-commit-msg\"");
    assert_eq!(
        denied["status"], "failed",
        "managed executables were writable: {denied:?}"
    );
    assert_eq!(
        denied["output"]
            .as_str()
            .unwrap()
            .matches("Operation not permitted")
            .count()
            + denied["output"]
                .as_str()
                .unwrap()
                .matches("Permission denied")
                .count(),
        2,
        "{denied:?}"
    );
}

#[test]
fn cortexkit_floor_background_output_is_captured() {
    skip_if_landlock_absent!();
    let mut f = CortexkitFloorFixture::new();
    let launch = f.aft.send(&json!({
        "id": "floor-background", "method": "bash", "session_id": "native-sandbox-session",
        "params": { "command": "printf captured-output; printf captured-error >&2", "background": true, "permissions_requested": true, "compressed": false },
    }).to_string());
    assert_eq!(launch["success"], true, "{launch:?}");
    let task_id = launch["task_id"].as_str().unwrap();
    let terminal = wait_for_terminal(&mut f.aft, task_id, None);
    assert_eq!(terminal["status"], "completed", "{terminal:?}");
    let output = terminal["output_preview"].as_str().unwrap();
    assert!(
        output.contains("captured-output") && output.contains("captured-error"),
        "{terminal:?}"
    );
}

fn foreground_with_env(aft: &mut AftProcess, id: &str, command: &str, env: Value) -> Value {
    aft.send(
        &json!({
            "id": id,
            "method": "bash",
            "session_id": "native-sandbox-session",
            "params": {
                "command": command,
                "foreground_orchestrate": true,
                "permissions_requested": true,
                "permissions_granted": [command],
                "compressed": false,
                "env": env,
            },
        })
        .to_string(),
    )
}

fn status(aft: &mut AftProcess, task_id: &str, output_mode: Option<&str>) -> Value {
    let mut params = json!({ "task_id": task_id });
    if let Some(output_mode) = output_mode {
        params["output_mode"] = Value::String(output_mode.to_string());
    }
    aft.send(
        &json!({
            "id": format!("status-{task_id}"),
            "method": "bash_status",
            "session_id": "native-sandbox-session",
            "params": params,
        })
        .to_string(),
    )
}

fn wait_for_terminal(aft: &mut AftProcess, task_id: &str, output_mode: Option<&str>) -> Value {
    let started = Instant::now();
    loop {
        let response = status(aft, task_id, output_mode);
        if matches!(
            response["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out")
        ) {
            return response;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "native sandbox task did not finish: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn native_sandbox_enforces_writes_temp_cache_and_reports_scanner_findings() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let outside = home.join("outside");
    let extra_write = home.join("explicit-write-allow");
    let npm_cache = home.join(".npm");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&extra_write).unwrap();
    std::fs::create_dir_all(&npm_cache).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native_policy(
        &mut aft,
        &project,
        &storage,
        true,
        std::slice::from_ref(&extra_write),
        &[],
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let project_file = project.join("project-write.txt");
    let project_write = foreground(
        &mut aft,
        "native-project-write",
        &format!("printf project-ok > {}", quote(&project_file)),
    );
    assert_eq!(
        project_write["status"], "completed",
        "project write should succeed: {project_write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&project_file).unwrap(),
        "project-ok"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        project_write["output"]
            .as_str()
            .unwrap_or_default()
            .matches("sandbox-launch: unenforced=")
            .count(),
        1,
        "Linux enforcement warning must appear once in stderr capture: {project_write:?}"
    );

    let explicitly_allowed_file = extra_write.join("allowed.txt");
    let explicit_write = foreground(
        &mut aft,
        "native-explicit-write",
        &format!("printf explicit-ok > {}", quote(&explicitly_allowed_file)),
    );
    assert_eq!(
        explicit_write["status"], "completed",
        "explicit write: {explicit_write:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&explicitly_allowed_file).unwrap(),
        "explicit-ok"
    );

    let outside_file = outside.join("must-not-write.txt");
    let outside_write = foreground(
        &mut aft,
        "native-outside-write",
        &format!("printf denied > {}", quote(&outside_file)),
    );
    assert_eq!(
        outside_write["status"], "failed",
        "outside write should be denied: {outside_write:?}"
    );
    assert!(!outside_file.exists());

    let temp_path_file = project.join("task-temp-path.txt");
    let temp_probe = foreground(
        &mut aft,
        "native-temp-write",
        &format!(
            "printf '%s' \"$TMPDIR\" > {}; printf temp-ok > \"$TMPDIR/probe.txt\"",
            quote(&temp_path_file)
        ),
    );
    assert_eq!(
        temp_probe["status"], "completed",
        "temp probe: {temp_probe:?}"
    );
    let task_temp = PathBuf::from(std::fs::read_to_string(&temp_path_file).unwrap());
    let canonical_storage = storage.canonicalize().unwrap();
    assert!(
        task_temp.starts_with(&canonical_storage),
        "task temp must live in the task bundle: {}",
        task_temp.display()
    );
    assert_eq!(
        std::fs::read_to_string(task_temp.join("probe.txt")).unwrap(),
        "temp-ok"
    );

    let cache_file = npm_cache.join("native-cache-write.txt");
    let cache_write = foreground(
        &mut aft,
        "native-cache-write",
        &format!("printf cache-ok > {}", quote(&cache_file)),
    );
    assert_eq!(
        cache_write["status"], "completed",
        "cache write: {cache_write:?}"
    );
    assert_eq!(std::fs::read_to_string(&cache_file).unwrap(), "cache-ok");

    let scanner = foreground(&mut aft, "native-scanner", "echo native-scanner");
    assert_eq!(
        scanner["success"], true,
        "native scanner must not request permission: {scanner:?}"
    );
    let task_id = scanner["task_id"].as_str().expect("scanner task id");
    let scanner_status = status(&mut aft, task_id, None);
    assert!(
        scanner_status["scanner_report"]
            .as_array()
            .is_some_and(|report| !report.is_empty()),
        "scanner findings must be retained in task metadata: {scanner_status:?}"
    );

    let disabled = configure_native(&mut aft, &project, &storage, false);
    assert_eq!(
        disabled["success"], true,
        "disable configure failed: {disabled:?}"
    );
    let permission = foreground(&mut aft, "disabled-scanner", "echo needs-permission");
    assert_eq!(
        permission["success"], false,
        "disabled scanner response: {permission:?}"
    );
    assert_eq!(permission["code"], "permission_required");

    assert!(aft.shutdown().success());
}

#[test]
fn db_hints_native_sandbox_reads_allowed_schema_and_refuses_denied_schema() {
    skip_if_landlock_absent!();
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("storage");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    // Landlock read-deny rules attach to directories, not single files, so the
    // protected database lives in its own directory and that directory is denied.
    let private = project.join("private");
    std::fs::create_dir_all(&private).unwrap();
    let db = private.join("store.db");
    let created = std::process::Command::new("sqlite3")
        .arg(&db)
        .arg("CREATE TABLE tasks(id TEXT, kind TEXT)")
        .output()
        .unwrap();
    assert!(created.status.success());
    let mut aft = AftProcess::spawn();
    assert_eq!(
        configure_native(&mut aft, &project, &storage, true)["success"],
        true
    );
    let allowed = foreground(
        &mut aft,
        "schema-allowed",
        "sqlite3 private/store.db 'SELECT substrate FROM tasks'",
    );
    assert_eq!(allowed["status"], "failed", "{allowed}");
    assert!(
        allowed["output"]
            .as_str()
            .unwrap()
            .contains("tasks: id TEXT, kind TEXT"),
        "{allowed}"
    );

    // The command can report an error without reading the protected database.
    // Its probe must not gain access just because it runs after that command.
    let binary = project.join("sqlite3");
    let real_binary = which::which("sqlite3").unwrap();
    std::fs::write(&binary, format!("#!/bin/sh\nif [ \"$1\" = -readonly ]; then exec {} \"$@\"; fi\nprintf 'Error: in prepare, no such column: substrate\\n' >&2\nexit 1\n", quote(&real_binary))).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        configure_native_policy(
            &mut aft,
            &project,
            &storage,
            true,
            &[],
            std::slice::from_ref(&private)
        )["success"],
        true
    );
    let denied = foreground(
        &mut aft,
        "schema-denied",
        "./sqlite3 private/store.db 'SELECT substrate FROM tasks'",
    );
    assert_eq!(denied["status"], "failed", "{denied}");
    let output = denied["output"].as_str().unwrap();
    assert!(output.contains("no such column: substrate"), "{output}");
    assert!(!output.contains("[aft: no column"), "{output}");
    assert!(aft.shutdown().success());
}

#[test]
fn native_sandbox_filters_ambient_environment_but_disabled_mode_preserves_it() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    #[cfg(target_os = "macos")]
    let loader_hook = "/usr/lib/libSystem.B.dylib";
    #[cfg(target_os = "linux")]
    let loader_hook = "libc.so.6";
    #[cfg(target_os = "macos")]
    let dyld_hook = "/usr/lib/libSystem.B.dylib";
    #[cfg(target_os = "linux")]
    let dyld_hook = "/untrusted/loader.dylib";
    let ambient = [
        ("HOME", OsStr::new(&home)),
        ("TERM", OsStr::new("aft-test-term")),
        ("LD_PRELOAD", OsStr::new(loader_hook)),
        ("DYLD_INSERT_LIBRARIES", OsStr::new(dyld_hook)),
        ("BASH_ENV", OsStr::new("/untrusted/bash-env")),
        ("AWS_SECRET_ACCESS_KEY", OsStr::new("ambient-cloud-secret")),
        ("SUBC_MODULE_ID", OsStr::new("aft")),
        (
            "SUBC_LAUNCH_NONCE",
            OsStr::new("synthetic-module-launch-nonce"),
        ),
        (
            "SUBC_FUTURE_CREDENTIAL",
            OsStr::new("synthetic-future-secret"),
        ),
    ];
    let mut aft = AftProcess::spawn_with_env(&ambient);
    let configured = configure_native(&mut aft, &project, &storage, true);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let request_env = json!({
        "AFT_REQUEST_ENV_TEST": "request-value",
        "SUBC_MODULE_ID": "request-smuggled-module",
        "SUBC_LAUNCH_NONCE": "request-smuggled-nonce",
        "SUBC_REQUEST_CREDENTIAL": "request-smuggled-future-secret"
    });

    let sandboxed = foreground_with_env(
        &mut aft,
        "native-filtered-environment",
        "/usr/bin/env",
        request_env.clone(),
    );
    assert_eq!(
        sandboxed["status"], "completed",
        "sandboxed env: {sandboxed:?}"
    );
    let sandboxed_output = sandboxed["output"].as_str().unwrap_or_default();
    for key in [
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "AWS_SECRET_ACCESS_KEY",
        "SUBC_MODULE_ID",
        "SUBC_LAUNCH_NONCE",
        "SUBC_FUTURE_CREDENTIAL",
        "SUBC_REQUEST_CREDENTIAL",
    ] {
        assert!(
            !sandboxed_output
                .lines()
                .any(|line| line.starts_with(&format!("{key}="))),
            "ambient {key} leaked into sandboxed child: {sandboxed_output}"
        );
    }
    for expected in [
        format!("HOME={}", home.display()),
        "TERM=aft-test-term".to_string(),
        "AFT_REQUEST_ENV_TEST=request-value".to_string(),
    ] {
        assert!(
            sandboxed_output.lines().any(|line| line == expected),
            "allowlisted/request environment missing {expected}: {sandboxed_output}"
        );
    }
    assert!(
        sandboxed_output
            .lines()
            .any(|line| line.starts_with("PATH=") && line.len() > 5),
        "enriched PATH missing from sandboxed child: {sandboxed_output}"
    );

    let pty_launch = aft.send(
        &json!({
            "id": "native-filtered-pty-environment",
            "method": "bash",
            "session_id": "native-sandbox-session",
            "params": {
                "command": "/usr/bin/env | /usr/bin/grep -E '^(HOME|TERM|AFT_REQUEST_ENV_TEST|LD_PRELOAD|DYLD_INSERT_LIBRARIES|BASH_ENV|AWS_SECRET_ACCESS_KEY|SUBC_[^=]*)='",
                "pty": true,
                "permissions_requested": true,
                "compressed": false,
                "env": request_env.clone(),
            },
        })
        .to_string(),
    );
    assert_eq!(
        pty_launch["success"], true,
        "PTY launch failed: {pty_launch:?}"
    );
    let pty_task_id = pty_launch["task_id"].as_str().expect("PTY task id");
    let pty_terminal = wait_for_terminal(&mut aft, pty_task_id, Some("screen"));
    let pty_output = pty_terminal["pty_screen"].as_str().unwrap_or_default();
    for key in [
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "AWS_SECRET_ACCESS_KEY",
        "SUBC_MODULE_ID",
        "SUBC_LAUNCH_NONCE",
        "SUBC_FUTURE_CREDENTIAL",
        "SUBC_REQUEST_CREDENTIAL",
    ] {
        assert!(
            !pty_output
                .lines()
                .any(|line| line.trim_end().starts_with(&format!("{key}="))),
            "ambient {key} leaked into sandboxed PTY: {pty_terminal:?}"
        );
    }
    for expected in [
        "HOME=",
        "TERM=aft-test-term",
        "AFT_REQUEST_ENV_TEST=request-value",
    ] {
        assert!(
            pty_output.contains(expected),
            "sandboxed PTY environment missing {expected}: {pty_terminal:?}"
        );
    }

    let disabled = configure_native(&mut aft, &project, &storage, false);
    assert_eq!(disabled["success"], true, "disable failed: {disabled:?}");
    let unsandboxed = foreground_with_env(
        &mut aft,
        "disabled-inherited-environment",
        "/usr/bin/env",
        request_env,
    );
    assert_eq!(
        unsandboxed["status"], "completed",
        "unsandboxed env: {unsandboxed:?}"
    );
    let unsandboxed_output = unsandboxed["output"].as_str().unwrap_or_default();
    assert!(
        unsandboxed_output
            .lines()
            .all(|line| !line.starts_with("SUBC_")),
        "sandbox-disabled child exposed module or request subc credentials: {unsandboxed_output}"
    );
    for expected in [
        format!("LD_PRELOAD={loader_hook}"),
        "BASH_ENV=/untrusted/bash-env".to_string(),
        "AWS_SECRET_ACCESS_KEY=ambient-cloud-secret".to_string(),
        "AFT_REQUEST_ENV_TEST=request-value".to_string(),
    ] {
        assert!(
            unsandboxed_output.lines().any(|line| line == expected),
            "sandbox-disabled child changed inherited {expected}: {unsandboxed_output}"
        );
    }
    #[cfg(target_os = "linux")]
    assert!(
        unsandboxed_output
            .lines()
            .any(|line| line == format!("DYLD_INSERT_LIBRARIES={dyld_hook}")),
        "sandbox-disabled Linux child changed inherited DYLD_INSERT_LIBRARIES: {unsandboxed_output}"
    );

    assert!(aft.shutdown().success());
}

#[cfg(target_os = "macos")]
#[test]
fn native_sandbox_denies_credentials_allows_git_metadata_and_denies_hooks_on_macos() {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let ssh = home.join(".ssh");
    let secret = ssh.join("id_test");
    let configured_secret = home.join("extra-secret.txt");
    let git = project.join(".git");
    let git_config = git.join("config");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(&secret, "credential").unwrap();
    std::fs::write(&configured_secret, "configured-secret").unwrap();
    std::fs::write(&git_config, "safe").unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native_policy(
        &mut aft,
        &project,
        &storage,
        true,
        &[],
        std::slice::from_ref(&configured_secret),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let read = foreground(
        &mut aft,
        "native-secret-read",
        &format!("cat {}", quote(&secret)),
    );
    assert_eq!(
        read["status"], "failed",
        "secret read should fail: {read:?}"
    );
    assert!(!read["output"]
        .as_str()
        .unwrap_or_default()
        .contains("credential"));

    let configured_read = foreground(
        &mut aft,
        "native-configured-secret-read",
        &format!(
            "/bin/sh -c 'cat \"$1\"' native-read {}",
            quote(&configured_secret)
        ),
    );
    assert_eq!(
        configured_read["status"], "failed",
        "configured secret read should fail: {configured_read:?}"
    );

    let git_write = foreground(
        &mut aft,
        "native-git-write",
        &format!("printf corrupted > {}", quote(&git_config)),
    );
    assert_eq!(
        git_write["status"], "completed",
        "ordinary Git metadata write should pass: {git_write:?}"
    );
    assert_eq!(std::fs::read_to_string(&git_config).unwrap(), "corrupted");

    let hook = git.join("hooks/pre-commit");
    let hook_write = foreground(
        &mut aft,
        "native-hook-write",
        &format!(
            "mkdir -p {} && printf pwned > {}",
            quote(hook.parent().unwrap()),
            quote(&hook)
        ),
    );
    assert_eq!(
        hook_write["status"], "failed",
        "Git hook write should fail: {hook_write:?}"
    );
    assert!(!hook.exists());

    assert!(aft.shutdown().success());
}

#[cfg(target_os = "linux")]
#[test]
fn native_read_floor_splits_project_denies_and_skips_home_symlinks() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let private = project.join("private");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let ssh = home.join(".ssh");
    let secret = ssh.join("id_test");
    let shortcut = home.join("shortcut");
    for directory in [&private, &storage, &ssh] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(&secret, "credential").unwrap();
    std::fs::write(private.join("token"), "private").unwrap();
    std::os::unix::fs::symlink(&ssh, &shortcut).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native_policy(
        &mut aft,
        &project,
        &storage,
        true,
        &[],
        std::slice::from_ref(&private),
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    for (id, path) in [
        ("home-symlink-secret", shortcut.join("id_test")),
        ("project-read-deny", private.join("token")),
    ] {
        let read = foreground(&mut aft, id, &format!("cat {}", quote(&path)));
        assert_eq!(read["status"], "failed", "denied read succeeded: {read:?}");
    }

    let written = private.join("write-still-allowed");
    let write = foreground(
        &mut aft,
        "project-read-deny-write",
        &format!("printf allowed > {}", quote(&written)),
    );
    assert_eq!(
        write["status"], "completed",
        "write should remain allowed: {write:?}"
    );
    assert_eq!(std::fs::read_to_string(written).unwrap(), "allowed");

    let etc = foreground(&mut aft, "system-read", "cat /etc/hostname");
    assert_eq!(etc["status"], "completed", "system read failed: {etc:?}");
    assert!(aft.shutdown().success());
}

#[cfg(target_os = "linux")]
#[test]
fn linked_worktree_reads_shared_git_metadata_but_not_hooks() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().unwrap();
    let main = fixture.path().join("main");
    let worktree = fixture.path().join("worktree");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    for directory in [&main, &storage, &home] {
        std::fs::create_dir_all(directory).unwrap();
    }
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&main)
        .status()
        .unwrap()
        .success());
    std::fs::write(main.join("tracked"), "tracked").unwrap();
    assert!(Command::new("git")
        .args(["add", "tracked"])
        .current_dir(&main)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.name=AFT Test",
            "-c",
            "user.email=aft@example.invalid",
            "commit",
            "-qm",
            "initial",
        ])
        .current_dir(&main)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["worktree", "add", "-q"])
        .arg(&worktree)
        .arg("HEAD")
        .current_dir(&main)
        .status()
        .unwrap()
        .success());
    let hook = main.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "shared-hook-secret").unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native(&mut aft, &worktree, &storage, true);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let status = foreground(&mut aft, "linked-git-status", "git status --porcelain");
    assert_eq!(
        status["status"], "completed",
        "linked git status failed: {status:?}"
    );

    let config = foreground(
        &mut aft,
        "linked-git-config-read",
        &format!("cat {}", quote(&main.join(".git/config"))),
    );
    assert_eq!(
        config["status"], "completed",
        "shared Git config read failed: {config:?}"
    );
    let hook_read = foreground(
        &mut aft,
        "linked-hook-read",
        &format!("cat {}", quote(&hook)),
    );
    assert_eq!(
        hook_read["status"], "failed",
        "shared hook read succeeded: {hook_read:?}"
    );
    assert!(aft.shutdown().success());
}

#[cfg(target_os = "linux")]
#[test]
fn native_secret_floor_refuses_home_write_allow_and_home_project_root() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    for directory in [&project, &storage, &home.join(".ssh")] {
        std::fs::create_dir_all(directory).unwrap();
    }

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native_policy(
        &mut aft,
        &project,
        &storage,
        true,
        std::slice::from_ref(&home),
        &[],
    );
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let write_allow = foreground(&mut aft, "home-write-allow-refusal", "true");
    assert_eq!(write_allow["code"], "sandbox_unavailable");
    assert!(write_allow["message"]
        .as_str()
        .is_some_and(|message| message.contains("overlaps mandatory secret floor")));
    assert!(aft.shutdown().success());

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native(&mut aft, &home, &storage, true);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let project_root = foreground(&mut aft, "home-project-refusal", "true");
    assert_eq!(project_root["code"], "sandbox_unavailable");
    assert!(project_root["message"]
        .as_str()
        .is_some_and(|message| message.contains("overlaps mandatory secret floor")));
    assert!(aft.shutdown().success());
}

#[test]
fn native_sandbox_pty_denies_outside_write_and_renders_screen() {
    skip_if_landlock_absent!();
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let storage = fixture.path().join("artifacts");
    let home = fixture.path().join("home");
    let outside = home.join("outside");
    let outside_file = outside.join("pty-must-not-write.txt");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let mut aft = AftProcess::spawn_with_env(&[("HOME", OsStr::new(&home))]);
    let configured = configure_native(&mut aft, &project, &storage, true);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let launch = aft.send(
        &json!({
            "id": "native-pty",
            "method": "bash",
            "session_id": "native-sandbox-session",
            "params": {
                "command": format!(
                    "printf denied > {}; printf '\npty-screen-rendered\n'",
                    quote(&outside_file)
                ),
                "pty": true,
                "permissions_requested": true,
                "compressed": false,
                "pty_rows": 24,
                "pty_cols": 80,
            },
        })
        .to_string(),
    );
    assert_eq!(launch["success"], true, "PTY launch failed: {launch:?}");
    let task_id = launch["task_id"].as_str().expect("PTY task id");
    let terminal = wait_for_terminal(&mut aft, task_id, Some("screen"));
    assert!(!outside_file.exists());
    assert!(
        terminal["pty_screen"]
            .as_str()
            .unwrap_or_default()
            .contains("pty-screen-rendered"),
        "PTY screen should render after the denied write: {terminal:?}"
    );

    #[cfg(target_os = "linux")]
    {
        let output = terminal["pty_screen"].as_str().unwrap_or_default();
        assert_eq!(
            output.matches("sandbox-launch: unenforced=").count(),
            1,
            "Linux enforcement warning must be captured once: {terminal:?}"
        );
    }

    assert!(aft.shutdown().success());
}
