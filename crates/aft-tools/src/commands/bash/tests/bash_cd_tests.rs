use super::*;
use crate::bash_rewrite::dispatch::{take_route_record, DispatchRoute};

fn local_context(project: &Path, storage: &Path) -> AppContext {
    let ctx = spawn_test_context(project, storage);
    ctx.update_config(|config| {
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        config.experimental_bash_rewrite = true;
    });
    ctx
}

#[test]
fn leading_cd_to_the_absolute_project_root_enables_the_grep_rewrite() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("needle.txt"), "find this needle\n").unwrap();
    let ctx = local_context(project.path(), storage.path());
    let command = format!("cd {} && grep needle needle.txt", project.path().display());
    let request = spawn_test_request("literal-cd-rewrite", &command, false);
    let response = handle(&request, &ctx);
    assert!(response.success, "{response:?}");
    assert!(matches!(
        take_route_record(&request.id).unwrap().route,
        DispatchRoute::Rewritten { rule_id, .. } if rule_id == "grep"
    ));
    assert_eq!(request.params["params"]["command"], command);
    assert!(request.params["params"].get("workdir").is_none());
}

#[test]
fn leading_cd_expands_the_inherited_home_and_enables_the_grep_rewrite() {
    const CHILD: &str = "AFT_TEST_BASH_CD_HOME_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Run with an isolated environment instead of changing HOME for other
        // tests that may be resolving paths at the same time.
        let home = tempfile::tempdir().unwrap();
        let test = concat!(
            module_path!(),
            "::leading_cd_expands_the_inherited_home_and_enables_the_grep_rewrite"
        );
        let name = test.split_once("::").unwrap().1;
        for value in [Some(home.path()), Some(Path::new("")), None] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", name]).env(CHILD, "1");
            if let Some(value) = value {
                child.env("HOME", value);
            } else {
                child.env_remove("HOME");
            }
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "HOME={value:?}: {stdout}\n{stderr}"
            );
            assert!(stdout.contains("1 passed"), "{stdout}");
        }
        return;
    }

    let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) else {
        let cwd = std::env::current_dir().unwrap();
        assert!(infer_cd_workdir("cd ~ && pwd", &cwd, false).is_none());
        assert!(infer_cd_workdir("cd ~/ && pwd", &cwd, false).is_none());
        return;
    };
    let home = PathBuf::from(home);
    let project = home.join("Work/Projects/repo");
    std::fs::create_dir_all(&project).unwrap();
    let storage = tempfile::tempdir().unwrap();
    for (id, dir, root) in [
        ("tilde-project", "~/Work/Projects/repo", project.as_path()),
        (
            "tilde-double-slash",
            "~//Work/Projects/repo",
            project.as_path(),
        ),
        ("tilde-home", "~", home.as_path()),
    ] {
        std::fs::write(root.join("needle.txt"), "find this needle\n").unwrap();
        let ctx = local_context(root, storage.path());
        let command = format!("cd {dir} && grep needle needle.txt");
        let request = spawn_test_request(id, &command, false);
        let response = handle(&request, &ctx);
        assert!(response.success, "{command}: {response:?}");
        assert!(
            matches!(
                take_route_record(id).unwrap().route,
                DispatchRoute::Rewritten { rule_id, .. } if rule_id == "grep"
            ),
            "{command}"
        );
        assert_eq!(request.params["params"]["command"], command);
    }
}

#[test]
fn leading_cd_does_not_change_the_blocked_home_override_refusal() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = local_context(project.path(), storage.path());
    for command in ["grep needle needle.txt", "cd ~/ && grep needle needle.txt"] {
        let mut request = spawn_test_request("tilde-home-override", command, false);
        request.params["params"]["env"]["HOME"] = json!(elsewhere.path());
        let response = handle(&request, &ctx);
        assert!(!response.success);
        assert_eq!(
            response.data,
            json!({
                "code": "blocked_env_var",
                "message": "bash env contains blocked variable: HOME",
            })
        );
    }
}

#[test]
fn leading_cd_starts_native_subdirectory_commands_there_and_keeps_cargo_compression() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let dir = project.path().join("nested");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("needle.txt"), "find this nested needle\n").unwrap();
    let ctx = local_context(project.path(), storage.path());
    let request = spawn_test_request(
        "literal-cd-native",
        &format!("cd {} && grep needle needle.txt", dir.display()),
        false,
    );
    let (snapshot, record) = run_native(&ctx, project.path(), &request);
    assert_eq!(snapshot.exit_code, Some(0));
    assert!(snapshot.output_preview.contains("find this nested needle"));
    assert_eq!(record.workdir, dir);
    assert_eq!(record.command, "grep needle needle.txt");

    let line = format!("cd {} && cargo test", dir.display());
    let (command, workdir) = infer_cd_workdir(&line, project.path(), false).unwrap();
    assert_eq!(workdir, dir);
    assert_eq!(command, "cargo test");
    let passing = (0..48)
        .map(|i| format!("test cd_compressor_case_{i} ... ok\n"))
        .collect::<String>();
    let output = format!("running 48 tests\n{passing}\ntest result: ok. 48 passed; 0 failed\n");
    let compressed =
        crate::compress::compress_with_registry(command, &output, &ctx.filter_registry());
    assert!(compressed
        .text
        .contains("test result: ok. 48 passed; 0 failed"));
    assert!(!compressed.text.contains("cd_compressor_case_0"));
}

#[test]
fn leading_cd_keeps_the_shell_line_for_missing_directory_semicolon_and_explicit_workdir() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let dir = project.path().join("nested");
    std::fs::create_dir(&dir).unwrap();
    let ctx = local_context(project.path(), storage.path());
    for (id, command, explicit, code) in [
        (
            "literal-cd-missing",
            "cd no-such-directory && printf cd-ran",
            false,
            1,
        ),
        ("literal-cd-semicolon", "cd nested; printf cd-ran", false, 0),
        ("literal-cd-explicit", "cd nested && printf cd-ran", true, 0),
    ] {
        let mut request = spawn_test_request(id, command, false);
        if explicit {
            request.params["params"]["workdir"] = json!(project.path());
        }
        let (snapshot, record) = run_native(&ctx, project.path(), &request);
        assert_eq!(snapshot.exit_code, Some(code), "{command}: {snapshot:?}");
        assert_eq!(
            snapshot.output_preview.contains("cd-ran"),
            code == 0,
            "{command}"
        );
        assert_eq!(record.workdir, project.path(), "{command}");
        assert_eq!(record.command, command);
    }
}

#[test]
fn leading_cd_outside_project_gets_the_same_permission_refusal_as_explicit_workdir() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    // The permission scanner treats the OS temp directory as an allowed target.
    let outside = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let ctx = local_context(project.path(), storage.path());
    ctx.update_config(|config| config.bash_permissions = true);
    let rest = "printf not-authorized > cd-policy-marker";
    let inferred = spawn_test_request(
        "literal-cd-outside",
        &format!("cd {} && {rest}", outside.path().display()),
        false,
    );
    let mut explicit = spawn_test_request("literal-cd-outside-explicit", rest, false);
    explicit.params["params"]["workdir"] = json!(outside.path());
    let inferred = handle(&inferred, &ctx);
    let explicit = handle(&explicit, &ctx);
    assert!(!inferred.success && !explicit.success);
    assert_eq!(explicit.data["code"], ERROR_PERMISSION_REQUIRED);
    assert_eq!(inferred.data, explicit.data);
    assert!(inferred.data["asks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|ask| ask["kind"] == "external_directory"));
    assert!(!outside.path().join("cd-policy-marker").exists());
}

#[test]
fn leading_cd_declines_nonliteral_words_and_cdpath_lookups() {
    let project = tempfile::tempdir().unwrap();
    for dir in ["nested", "'nested'", "$ROOT", "nested;", "-", "~otheruser"] {
        std::fs::create_dir(project.path().join(dir)).unwrap();
    }
    for dir in ["'nested'", "$ROOT", "nested;", "-", "~otheruser"] {
        assert!(infer_cd_workdir(&format!("cd {dir} && pwd"), project.path(), false).is_none());
    }
    assert!(infer_cd_workdir("cd nested && pwd", project.path(), true).is_none());
    assert!(infer_cd_workdir("cd ./nested && pwd", project.path(), true).is_some());
}

fn run_native(
    ctx: &AppContext,
    project: &Path,
    request: &RawRequest,
) -> (
    crate::bash_background::registry::BgTaskSnapshot,
    crate::bash_background::persistence::PersistedTask,
) {
    let response = handle(request, ctx);
    assert!(response.success, "bash spawn failed: {response:?}");
    let task_id = response.data["task_id"].as_str().expect("native task ID");
    let storage = crate::bash_background::task_storage_dir(ctx);
    let started = std::time::Instant::now();
    loop {
        let snapshot = ctx
            .bash_background()
            .status(
                task_id,
                "sandbox-spawn-test",
                Some(project),
                Some(&storage),
                4096,
            )
            .expect("spawned task should be visible");
        if snapshot.info.status.is_terminal() {
            return (snapshot, read_record(ctx, task_id));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{snapshot:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}
