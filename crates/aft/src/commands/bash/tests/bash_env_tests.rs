//! Tests for where a configured `bash.env` value is applied: the project
//! trust gate, tier precedence, per-call and in-command precedence, the
//! control variables AFT writes itself, and the runon restriction.

#![cfg(unix)]

use std::collections::BTreeMap;

use serde_json::json;

use super::*;

fn env_map(names: &[(&str, &str)]) -> BTreeMap<String, String> {
    names
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

fn context(project: &Path, storage: &Path) -> AppContext {
    AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            experimental_bash_background: true,
            sandbox: crate::config::SandboxConfig {
                profile: crate::config::SandboxRoles {
                    head: crate::config::SandboxMode::Off,
                    worker: crate::config::SandboxMode::Off,
                },
                ..crate::config::SandboxConfig::default()
            },
            ..crate::config::Config::default()
        },
    )
}

/// A context whose resolved `bash.env` maps are exactly `user` and `project`.
fn context_with_env(
    project: &Path,
    storage: &Path,
    user: &[(&str, &str)],
    project_env: &[(&str, &str)],
) -> AppContext {
    let mut config = context(project, storage).config().as_ref().clone();
    config.bash.env = env_map(user);
    config.bash.project_env = env_map(project_env);
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

/// Run one background command through the real `handle` path and return its
/// output, so the child process proves what it received.
fn run_output(ctx: &AppContext, project: &Path, request: &RawRequest) -> String {
    bash_output_for_test(ctx, project, request, false)
}

fn trust(storage: &Path, project: &Path) {
    crate::compress::trust::trust_project(storage, project).unwrap();
}

fn untrust(storage: &Path, project: &Path) {
    crate::compress::trust::untrust_project(storage, project).unwrap();
}

fn request(id: &str, command: &str) -> RawRequest {
    spawn_test_request(id, command, true)
}

fn provider_context(
    project: &Path,
    storage: &Path,
    kind: crate::config::ShellEnvProviderKind,
    command: Vec<String>,
) -> AppContext {
    let mut config = context(project, storage).config().as_ref().clone();
    config.git.co_author = "off".into();
    config.shell.env_provider = Some(crate::config::ShellEnvProvider {
        kind,
        command: Some(command),
        timeout_ms: 2000,
        watch: vec![],
    });
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

fn fake_export(project: &Path, export: serde_json::Value) -> Vec<String> {
    let path = project.join("provider.json");
    std::fs::write(&path, serde_json::to_vec(&export).unwrap()).unwrap();
    vec![
        "sh".into(),
        "-c".into(),
        "cat \"$1\"".into(),
        "provider".into(),
        path.to_string_lossy().into_owned(),
    ]
}

#[test]
fn shell_env_provider_custom_end_to_end_and_failure_fallback() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = provider_context(
        project.path(),
        storage.path(),
        crate::config::ShellEnvProviderKind::Custom,
        fake_export(
            project.path(),
            json!({"GIT_AUTHOR_NAME":"Project Author", "GIT_COMMITTER_EMAIL":"project@example.org", "REMOVE": null, "LD_PRELOAD":"bad", "GIT_SSH_COMMAND":"bad", "SUBC_DAEMON_PID":"bad"}),
        ),
    );
    let mut call = request("provider-custom", "printf '%s|%s|%s|%s|%s' \"$GIT_AUTHOR_NAME\" \"$GIT_COMMITTER_EMAIL\" \"${REMOVE-unset}\" \"${LD_PRELOAD-unset}\" \"${GIT_SSH_COMMAND-unset}\"");
    call.params["params"]["env"] =
        json!({"REMOVE":"base", "AFT_INTERNAL_SHELL_ENV_REMOVALS":"[\"GIT_AUTHOR_NAME\"]"});
    assert_eq!(
        run_output(&ctx, project.path(), &call),
        "Project Author|project@example.org|unset|unset|unset"
    );
    std::fs::write(project.path().join("provider.json"), "bad JSON").unwrap();
    let fallback = request("provider-fallback", "printf shell-still-runs");
    let response = handle(&fallback, &ctx);
    assert!(response.success, "{response:?}");
    assert_eq!(response.data["_bash_env_provider_failure"], "Project environment not loaded (invalid provider output: not a JSON object); this command used AFT's default PATH.");
    assert_eq!(
        run_output(
            &ctx,
            project.path(),
            &request("provider-fallback-output", "printf shell-still-runs")
        ),
        "shell-still-runs"
    );
}

#[test]
fn shell_env_provider_mise_end_to_end() {
    use std::os::unix::fs::PermissionsExt;
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let nested = project.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let real = std::env::var("AFT_TEST_MISE").ok();
    let command = if let Some(mise) = &real {
        std::fs::write(
            project.path().join("mise.toml"),
            "[tools]\nnode = '20'\n[env]\nGIT_AUTHOR_NAME = 'Parent Author'\n",
        )
        .unwrap();
        std::fs::write(
            nested.join("mise.toml"),
            "[env]\nPROVIDER_NESTED = 'nested value'\n",
        )
        .unwrap();
        for path in [project.path().join("mise.toml"), nested.join("mise.toml")] {
            let output = std::process::Command::new(mise)
                .args(["trust"])
                .arg(path)
                .env("MISE_PARANOID", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        vec![mise.clone(), "env".into(), "--json".into()]
    } else {
        let bin = project.path().join("node20/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let node = bin.join("node");
        std::fs::write(&node, "#!/bin/sh\nprintf 'v20.0.0\\n'\n").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake_export(
            project.path(),
            json!({"PATH":format!("{}:{}", bin.display(), crate::effective_path::effective_path().to_string_lossy()), "GIT_AUTHOR_NAME":"Parent Author", "PROVIDER_NESTED":"nested value"}),
        )
    };
    let ctx = provider_context(
        project.path(),
        storage.path(),
        crate::config::ShellEnvProviderKind::Mise,
        command,
    );
    let mut call = request(
        "provider-mise",
        "node --version; printf '%s|%s' \"$GIT_AUTHOR_NAME\" \"$PROVIDER_NESTED\"",
    );
    call.params["params"]["workdir"] = json!(nested);
    call.params["params"]["env"] = json!({"MISE_PARANOID":"1"});
    let output = run_output(&ctx, project.path(), &call);
    assert!(output.starts_with("v20."), "{output}");
    assert!(output.ends_with("Parent Author|nested value"), "{output}");
    if real.is_some() {
        let baseline = run_output(
            &context(project.path(), storage.path()),
            project.path(),
            &request("provider-mise-baseline", "node --version"),
        );
        assert!(
            baseline.starts_with("v22."),
            "Linux gate must supply daemon Node 22: {baseline}"
        );
    }
}

#[test]
fn shell_env_provider_direnv_end_to_end() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let real = std::env::var("AFT_TEST_DIRENV").ok();
    let command = if let Some(direnv) = &real {
        std::fs::write(
            project.path().join(".envrc"),
            "unset REMOVE\nexport GIT_AUTHOR_NAME='Direnv Author'\n",
        )
        .unwrap();
        let output = std::process::Command::new(direnv)
            .arg("allow")
            .arg(project.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        vec![direnv.clone(), "export".into(), "json".into()]
    } else {
        fake_export(
            project.path(),
            json!({"REMOVE":null, "GIT_AUTHOR_NAME":"Direnv Author"}),
        )
    };
    let ctx = provider_context(
        project.path(),
        storage.path(),
        crate::config::ShellEnvProviderKind::Direnv,
        command,
    );
    let mut call = request("provider-direnv", "printf '%s|%s|%s' \"${REMOVE-unset}\" \"$GIT_AUTHOR_NAME\" \"${AFT_INTERNAL_SHELL_ENV_REMOVALS-unset}\"");
    call.params["params"]["env"] = json!({"REMOVE":"base"});
    assert_eq!(
        run_output(&ctx, project.path(), &call),
        "unset|Direnv Author|unset"
    );
}

#[test]
fn shell_env_provider_untrusted_managers_fall_back_to_default_path() {
    for (kind, binary_env, config_file, content, stderr, reason) in [
        (
            crate::config::ShellEnvProviderKind::Mise,
            "AFT_TEST_MISE",
            "mise.toml",
            "[env]\nGIT_AUTHOR_NAME='Untrusted'\n",
            "mise ERROR Config files are not trusted.",
            "mise: config not trusted, run 'mise trust'",
        ),
        (
            crate::config::ShellEnvProviderKind::Direnv,
            "AFT_TEST_DIRENV",
            ".envrc",
            "export GIT_AUTHOR_NAME=Untrusted\n",
            ".envrc is blocked. Run direnv allow",
            "direnv: .envrc is blocked, run 'direnv allow'",
        ),
    ] {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(config_file), content).unwrap();
        let command = if let Ok(binary) = std::env::var(binary_env) {
            let args = if kind == crate::config::ShellEnvProviderKind::Mise {
                ["env", "--json"]
            } else {
                ["export", "json"]
            };
            vec![binary, args[0].into(), args[1].into()]
        } else {
            vec![
                "sh".into(),
                "-c".into(),
                format!("printf '%s\\n' '{}' >&2; exit 1", stderr),
            ]
        };
        let ctx = provider_context(project.path(), storage.path(), kind, command);
        let mut call = request("provider-untrusted", "printf '%s' \"$PATH\"");
        call.params["params"]["env"] = json!({"MISE_PARANOID":"1"});
        let response = handle(&call, &ctx);
        assert!(response.success, "{response:?}");
        assert_eq!(
            response.data["_bash_env_provider_failure"],
            format!(
                "Project environment not loaded ({reason}); this command used AFT's default PATH."
            )
        );
        let actual = run_output(&ctx, project.path(), &call);
        let default = run_output(
            &context(project.path(), storage.path()),
            project.path(),
            &request("provider-default-path", "printf '%s' \"$PATH\""),
        );
        assert_eq!(actual, default);
    }
}

#[test]
fn an_untrusted_project_bash_env_is_ignored_until_the_project_is_trusted() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        project.path(),
        storage.path(),
        &[],
        &[("BASHENV_TEST_PROJECT", "from-project")],
    );

    let command = "printf '%s' \"${BASHENV_TEST_PROJECT-unset}\"";
    assert_eq!(
        run_output(&ctx, project.path(), &request("untrusted", command)),
        "unset"
    );

    trust(storage.path(), project.path());
    assert_eq!(
        run_output(&ctx, project.path(), &request("trusted", command)),
        "from-project"
    );

    // Trust is read per command, so untrusting takes effect without a reload.
    untrust(storage.path(), project.path());
    assert_eq!(
        run_output(&ctx, project.path(), &request("untrusted-again", command)),
        "unset"
    );
}

#[test]
fn a_linked_worktree_inherits_trust_from_its_main_checkout() {
    let main = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    // Git pairs a linked worktree both ways: the worktree's `.git` names the
    // entry, and the entry's `gitdir` names the worktree back. Trust is
    // inherited only from a pair that agrees, so the fixture writes both.
    let git_dir = main.path().join(".git").join("worktrees").join("linked");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(
        worktree.path().join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    std::fs::write(
        git_dir.join("gitdir"),
        format!("{}\n", worktree.path().join(".git").display()),
    )
    .unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        worktree.path(),
        storage.path(),
        &[],
        &[("BASHENV_TEST_WORKTREE", "from-worktree")],
    );
    let command = "printf '%s' \"${BASHENV_TEST_WORKTREE-unset}\"";
    assert_eq!(
        run_output(
            &ctx,
            worktree.path(),
            &request("worktree-untrusted", command)
        ),
        "unset"
    );

    trust(storage.path(), main.path());
    assert_eq!(
        run_output(&ctx, worktree.path(), &request("worktree-trusted", command)),
        "from-worktree"
    );
}

#[test]
fn a_user_value_wins_over_a_project_value_for_the_same_name() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        project.path(),
        storage.path(),
        &[("BASHENV_TEST_SHARED", "from-user")],
        &[("BASHENV_TEST_SHARED", "from-project")],
    );
    trust(storage.path(), project.path());
    assert_eq!(
        run_output(
            &ctx,
            project.path(),
            &request("shared", "printf '%s' \"$BASHENV_TEST_SHARED\"")
        ),
        "from-user"
    );
    assert_eq!(
        ctx.config().bash.shadowed_project_env(true),
        vec!["BASHENV_TEST_SHARED".to_string()]
    );
}

#[test]
fn a_per_call_value_and_the_command_itself_win_over_configuration() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        project.path(),
        storage.path(),
        &[("BASHENV_TEST_ORDER", "from-config")],
        &[("BASHENV_TEST_PROJECT", "from-project")],
    );
    trust(storage.path(), project.path());

    let mut per_call = request("per-call", "printf '%s' \"$BASHENV_TEST_ORDER\"");
    per_call.params["params"]["env"] = json!({ "BASHENV_TEST_ORDER": "from-call" });
    assert_eq!(
        run_output(&ctx, project.path(), &per_call),
        "from-call",
        "a per-call value must win over configured values"
    );

    assert_eq!(
        run_output(
            &ctx,
            project.path(),
            &request(
                "in-command",
                "BASHENV_TEST_ORDER=from-command; export BASHENV_TEST_ORDER; printenv BASHENV_TEST_ORDER"
            )
        ),
        "from-command\n",
        "the command's own export must win over configured values"
    );
}

#[test]
fn the_worker_test_thread_budget_survives_a_configured_value() {
    let _environment = without_test_thread_environment();
    let container = tempfile::tempdir().unwrap();
    let project = container.path().join("worktree");
    std::fs::create_dir(&project).unwrap();
    let budget_path = container.path().join(".cargo/alfonso-test-threads");
    std::fs::create_dir_all(budget_path.parent().unwrap()).unwrap();
    std::fs::write(&budget_path, "8\n").unwrap();
    let storage = tempfile::tempdir().unwrap();
    // The budget and both thread names are AFT's own controls; a configured
    // value must not defeat them.
    let ctx = context_with_env(
        &project,
        storage.path(),
        &[("RUST_TEST_THREADS", "64"), ("NEXTEST_TEST_THREADS", "64")],
        &[],
    );
    let mut request = worker_thread_output_request("budget");
    request.params["params"]["workdir"] = json!(project);
    let output = bash_output_for_test(&ctx, &project, &request, true);
    assert_eq!(
        output, "8|8",
        "configured test-thread values must not defeat the budget"
    );
}

#[test]
fn a_refused_per_call_name_is_rejected_before_the_launch() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(project.path(), storage.path());
    // Module-control names (`AFT_*`, `SUBC_*`) are allowed per call: the
    // product strips them from the child, so refusing them only breaks callers
    // that pass them on purpose. Everything else stays refused, including a
    // non-ASCII name, which must be refused rather than panic on a byte cut.
    for name in ["GIT_CONFIG_COUNT", "NODE_OPTIONS", "PATH"] {
        let mut request = request("refused", "printf hi");
        request.params["params"]["env"] = json!({ name: "1" });
        let response = handle(&request, &ctx);
        assert!(!response.success, "{name} must be refused: {response:?}");
        assert_eq!(response.data["code"], "blocked_env_var", "{name}");
    }
}

#[test]
fn a_runon_job_receives_only_user_names_listed_in_runon_env() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        project.path(),
        storage.path(),
        &[("TZ", "UTC"), ("BASHENV_TEST_USER_ONLY", "user-value")],
        &[("BASHENV_TEST_PROJECT", "project-value")],
    );
    let mut config = ctx.config().as_ref().clone();
    config.bash.runon_env = vec!["TZ".to_string(), "BASHENV_TEST_PROJECT".to_string()];
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    trust(storage.path(), project.path());

    let plan = crate::commands::bash_remote_env::plan(ctx.config().as_ref());
    assert_eq!(
        plan.forwarded.get("TZ").map(String::as_str),
        Some("UTC"),
        "a listed user name is sent"
    );
    assert!(
        !plan.forwarded.contains_key("BASHENV_TEST_USER_ONLY"),
        "an unlisted user name is not sent"
    );
    assert!(
        !plan.forwarded.contains_key("BASHENV_TEST_PROJECT"),
        "a project value is never sent, even when its name is listed"
    );
    let mut withheld = plan.withheld.clone();
    withheld.sort();
    assert_eq!(
        withheld,
        vec![
            "BASHENV_TEST_PROJECT".to_string(),
            "BASHENV_TEST_USER_ONLY".to_string()
        ]
    );

    let disclosure = crate::commands::bash_remote_env::Disclosure::build(
        &ctx.config().bash.configured_env(true),
        &[],
        false,
        &plan,
    );
    let text = disclosure.lines.join("\n");
    assert!(text.contains("does not receive"), "{text}");
    assert!(text.contains("BASHENV_TEST_USER_ONLY"), "{text}");
    assert!(text.contains("BASHENV_TEST_PROJECT"), "{text}");
    assert!(!text.contains("user-value"), "{text}");
    assert!(!text.contains("project-value"), "{text}");
}

#[test]
fn the_disclosure_lists_tiers_once_and_never_a_value() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        project.path(),
        storage.path(),
        &[("BASHENV_TEST_USER", "user-secret-value")],
        &[("BASHENV_TEST_PROJECT", "project-secret-value")],
    );
    trust(storage.path(), project.path());

    let command = "printf done";
    let first = handle(&request("disclosure-first", command), &ctx);
    assert!(first.success, "{:?}", first.data);
    let text = first.data["_bash_env_keys"]["lines"].to_string();
    assert!(text.contains("BASHENV_TEST_USER (user)"), "{text}");
    assert!(text.contains("BASHENV_TEST_PROJECT (project)"), "{text}");
    assert!(!text.contains("user-secret-value"), "{text}");
    assert!(!text.contains("project-secret-value"), "{text}");

    let second = handle(&request("disclosure-second", command), &ctx);
    assert!(second.success, "{:?}", second.data);
    assert!(
        second.data.get("_bash_env_keys").is_none(),
        "the notice is once per session: {:?}",
        second.data
    );
}

#[test]
fn an_untrusted_project_gets_no_value_and_the_notice_says_why() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_env(
        project.path(),
        storage.path(),
        &[],
        &[("BASHENV_TEST_PROJECT", "from-project")],
    );
    let response = handle(
        &request(
            "untrusted-notice",
            "printf '%s' \"${BASHENV_TEST_PROJECT-unset}\"",
        ),
        &ctx,
    );
    assert!(response.success, "{:?}", response.data);
    let text = response.data["_bash_env_keys"]["lines"].to_string();
    assert!(
        text.contains("Project bash.env ignored: project not trusted"),
        "{text}"
    );
    assert!(!text.contains("from-project"), "{text}");
}
