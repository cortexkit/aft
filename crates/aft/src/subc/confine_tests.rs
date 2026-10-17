use super::*;
use crate::context::confinement;

struct Fixture {
    repo: tempfile::TempDir,
    storage: tempfile::TempDir,
    pin: PathBuf,
    live: PathBuf,
    ctx: AppContext,
    route: RouteIdentity,
}

impl Fixture {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path().canonicalize().unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=AFT",
                "-c",
                "user.email=aft@example.invalid",
                "commit",
                "-qm",
                "fixture",
                "--allow-empty",
            ],
            vec!["worktree", "add", "-q", "--detach", "pin", "HEAD"],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let pin = root.join("pin");
        let live = root.join("live");
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join("sentinel.rs"), "fn outside_secret() {}\n").unwrap();
        std::fs::write(pin.join("only.rs"), "fn pinned_token() {}\n").unwrap();
        std::os::unix::fs::symlink("../live", pin.join("escape")).unwrap();
        std::os::unix::fs::symlink("../live/sentinel.rs", pin.join("escape-file")).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = context(&pin, storage.path(), None);
        ctx.update_config(|config| config.restrict_to_project_root = false);
        let route = identity(&pin, "designer", 7, true);
        catalog(
            json!({"preset":"worker", "params":{"confine":"root"}}),
            &route,
            &ctx,
        )
        .unwrap();
        Self {
            repo,
            storage,
            pin,
            live,
            ctx,
            route,
        }
    }

    fn escapes(&self) -> Vec<(&'static str, String)> {
        vec![
            ("parent", "../live/sentinel.rs".into()),
            ("directory-symlink", "escape/sentinel.rs".into()),
            ("file-symlink", "escape-file".into()),
            (
                "absolute",
                self.live.join("sentinel.rs").display().to_string(),
            ),
            ("dot-parent", "./../live/sentinel.rs".into()),
            ("missing-relative", "sentinel.rs".into()),
        ]
    }

    fn call(&self, name: &str, args: Value) -> Response {
        call(&self.ctx, &self.route, name, args)
    }
}

fn arguments(name: &str, input: &str) -> Value {
    match name {
        "read" => json!({"filePath": input}),
        "outline" => json!({"target": input}),
        "grep" => json!({"pattern":"outside_secret|pinned_token", "path":input}),
        "glob" => json!({"pattern":"**/*.rs", "path":input}),
        _ => unreachable!(),
    }
}

#[test]
fn confined_escape_matrix() {
    let f = Fixture::new();
    for name in ["read", "grep", "glob", "outline"] {
        for (route, input) in f.escapes().into_iter().chain([
            ("issue", "issue://1".into()),
            ("pr", "pr://1".into()),
            ("url", "https://example.invalid/private".into()),
            ("dot-symlink", "./escape/sentinel.rs".into()),
        ]) {
            let response = f.call(name, arguments(name, &input));
            assert_eq!(
                response.data["code"], "path_outside_confined_root",
                "{name} {route}: {response:?}"
            );
            assert!(response.data["message"].as_str().unwrap().contains(&input));
            let text = serde_json::to_string(&response).unwrap();
            assert!(!text.contains("outside_secret"));
            if route.contains("symlink") {
                assert!(!text.contains(&f.live.display().to_string()));
            }
            assert!(response.data.get("ask").is_none());
        }
        let response = f.call(name, arguments(name, "./only.rs"));
        assert_ne!(
            response.data["code"], "path_outside_confined_root",
            "{name}: {response:?}"
        );
        assert!(response.success, "{name}: {response:?}");
    }
}

#[test]
fn confined_never_asks() {
    let f = Fixture::new();
    let before = confinement::ask_calls();
    for name in ["read", "grep", "glob", "outline"] {
        let response = f.call(name, arguments(name, "../live/sentinel.rs"));
        assert_eq!(response.data["code"], "path_outside_confined_root");
    }
    assert_eq!(
        confinement::ask_calls(),
        before,
        "confined access reached the consent ask path"
    );
}

#[test]
fn confined_keeps_in_root_denial_floors() {
    let f = Fixture::new();
    f.ctx
        .update_config(|config| config.sandbox.read_deny = vec![f.pin.join("only.rs")]);
    let response = f.call("read", arguments("read", "only.rs"));
    assert!(!response.success, "{response:?}");
    assert!(!serde_json::to_string(&response)
        .unwrap()
        .contains("pinned_token"));
}

/// A pinned tree holding a `sandbox.read_deny` file and a denied directory
/// beside readable files.
fn denied_tree() -> Fixture {
    let f = Fixture::new();
    std::fs::write(
        f.pin.join("secret.rs"),
        "fn pinned_token() {} // denied_bytes\n",
    )
    .unwrap();
    std::fs::create_dir(f.pin.join("vault")).unwrap();
    std::fs::write(
        f.pin.join("vault/inner.rs"),
        "fn pinned_token() {} // denied_bytes\n",
    )
    .unwrap();
    std::fs::create_dir(f.pin.join("open")).unwrap();
    std::fs::write(f.pin.join("open/kept.rs"), "fn pinned_token() {}\n").unwrap();
    f.ctx.update_config(|config| {
        config.sandbox.read_deny = vec![f.pin.join("secret.rs"), f.pin.join("vault")]
    });
    f
}

fn directory_walks() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("grep", "grep", arguments("grep", ".")),
        ("glob", "glob", arguments("glob", ".")),
        ("outline-directory", "outline", json!({"target":"."})),
        (
            "outline-files",
            "outline",
            json!({"target":".", "files":true}),
        ),
    ]
}

#[test]
fn confined_directory_walks_skip_denied_files_without_reading_them() {
    let f = denied_tree();
    let watches = [
        crate::bounded_io::tests::watch_opens(&f.pin.join("secret.rs")),
        crate::bounded_io::tests::watch_opens(&f.pin.join("vault/inner.rs")),
    ];
    let kept = crate::bounded_io::tests::watch_opens(&f.pin.join("open/kept.rs"));
    for (case, name, args) in directory_walks() {
        let response = f.call(name, args);
        assert!(response.success, "{case}: {response:?}");
        let native = serde_json::to_string(&response).unwrap();
        let text = crate::subc_format::format_response(name, &response, false);
        for forbidden in ["secret.rs", "vault", "denied_bytes"] {
            assert!(!native.contains(forbidden), "{case}: {native}");
            assert!(!text.contains(forbidden), "{case}: {text}");
        }
        // The walk still ran: readable files beside the denied ones are listed.
        assert!(native.contains("kept.rs"), "{case}: {native}");
        // Both denied paths are counted, in the native reply and its text.
        assert_eq!(response.data["skipped_denied"], 2, "{case}: {native}");
        assert!(
            text.contains("2 path(s) under the walked directory are denied by the read policy"),
            "{case}: {text}"
        );
    }
    for watch in &watches {
        assert_eq!(watch.opens.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    // The open counter does see the walks reading an admitted file.
    assert!(kept.opens.load(std::sync::atomic::Ordering::Relaxed) > 0);
}

#[test]
fn confined_directory_walk_golden_vectors() {
    let f = denied_tree();
    for (case, name, args) in directory_walks() {
        let response = f.call(name, args);
        assert!(response.success, "{case}: {response:?}");
        let text = crate::subc_format::format_response(name, &response, false);
        vector(
            &f,
            &format!("deny-{case}"),
            json!({"native":serde_json::to_value(response).unwrap(),"text":text}),
        );
    }
}

#[test]
fn confined_walks_honor_only_the_pinned_trees_own_ignore_files() {
    let f = Fixture::new();
    // Rules outside the pinned tree must not hide `only.rs`.
    std::fs::write(f.repo.path().join(".gitignore"), "only.rs\n").unwrap();
    std::fs::write(f.pin.join(".gitignore"), "tree_ignored.rs\n").unwrap();
    std::fs::create_dir(f.pin.join("nested")).unwrap();
    std::fs::write(f.pin.join("nested/.gitignore"), "nested_ignored.rs\n").unwrap();
    for path in [
        "tree_ignored.rs",
        "nested/nested_ignored.rs",
        "nested/kept.rs",
    ] {
        std::fs::write(f.pin.join(path), "fn pinned_token() {}\n").unwrap();
    }
    for (case, name, args) in directory_walks() {
        let response = f.call(name, args);
        assert!(response.success, "{case}: {response:?}");
        let native = serde_json::to_string(&response).unwrap();
        for forbidden in ["tree_ignored.rs", "nested_ignored.rs"] {
            assert!(!native.contains(forbidden), "{case}: {native}");
        }
        for expected in ["only.rs", "kept.rs"] {
            assert!(native.contains(expected), "{case}: {native}");
        }
        assert!(response.data.get("skipped_denied").is_none(), "{case}");
    }
}

#[test]
fn confined_call_time_tool_check() {
    let f = Fixture::new();
    for name in [
        "bash",
        "bash_watch",
        "bash_status",
        "bash_write",
        "bash_kill",
        "powershell",
        "search",
        "zoom",
        "callgraph",
        "inspect",
        "write",
        "edit",
        "apply_patch",
        "delete",
        "move",
        "import",
        "safety",
        "ast_search",
        "ast_replace",
        "status",
        "conflicts",
        "configure",
    ] {
        let response = f.call(name, json!({}));
        assert_eq!(
            response.data["code"], "tool_not_offered_confined",
            "{name}: {response:?}"
        );
    }
    // Looking up the worker key is independent of a caller's claimed preset.
    assert!(admit_confined(&f.ctx, &f.route, "bash").is_err());
}

#[test]
fn confined_restart_and_fail_closed() {
    let f = Fixture::new();
    drop(f.ctx);
    let restarted = context(&f.pin, f.storage.path(), None);
    assert!(confined(&restarted, &f.route).unwrap());
    let response = call(
        &restarted,
        &f.route,
        "read",
        arguments("read", "../live/sentinel.rs"),
    );
    assert_eq!(response.data["code"], "path_outside_confined_root");
    let answer = catalog(json!({"preset":"worker"}), &f.route, &restarted).unwrap();
    assert_eq!(answer["tools"].as_array().unwrap().len(), 4);
    let db = restarted.db().unwrap();
    db.lock()
        .unwrap()
        .execute("UPDATE remote_exec_policies SET params='broken'", [])
        .unwrap();
    // `restarted` already read the fence and keeps it without reading again.
    // A process that first meets the unreadable row must refuse the call.
    assert_eq!(
        call(
            &restarted,
            &f.route,
            "read",
            arguments("read", "../live/sentinel.rs")
        )
        .data["code"],
        "path_outside_confined_root"
    );
    let unreadable = context(&f.pin, f.storage.path(), None);
    assert_eq!(
        call(&unreadable, &f.route, "read", arguments("read", "only.rs")).data["code"],
        "confine_policy_unavailable"
    );
}

#[test]
fn confined_walks_ignore_ready_parent_index() {
    let f = Fixture::new();
    std::fs::write(f.pin.join("ghost.rs"), "fn pinned_token() {}\n").unwrap();
    std::fs::remove_file(f.pin.join("only.rs")).unwrap();
    let index = crate::search_index::SearchIndex::build(f.repo.path());
    assert!(index.ready);
    std::fs::remove_file(f.pin.join("ghost.rs")).unwrap();
    std::fs::write(f.pin.join("only.rs"), "fn pinned_token() {}\n").unwrap();
    *f.ctx.search_index().write().unwrap() = Some(index);
    std::fs::write(f.live.join("ignore-rules"), "only.rs\n").unwrap();
    for name in [".gitignore", ".ignore", ".aftignore"] {
        std::os::unix::fs::symlink("../live/ignore-rules", f.pin.join(name)).unwrap();
    }
    for name in ["grep", "glob"] {
        let response = f.call(name, arguments(name, "."));
        assert!(response.success, "{response:?}");
        assert_eq!(
            response.data["fallback"], "filesystem",
            "{name}: {response:?}"
        );
        assert_eq!(response.data["complete"], true);
        assert!(response.data.get("walk_truncated").is_none());
        assert!(!response.data["text"].as_str().unwrap().contains("(walk)"));
        let files = if name == "grep" {
            response.data["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["file"].as_str().unwrap())
                .collect::<Vec<_>>()
        } else {
            response.data["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file.as_str().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(files, [f.pin.join("only.rs").to_str().unwrap()]);
        let text = serde_json::to_string(&response).unwrap();
        for forbidden in ["sentinel.rs", "outside_secret", "ghost.rs", "escape-file"] {
            assert!(!text.contains(forbidden), "{name}: {text}");
        }
    }
}

#[test]
fn confined_catalog_validation_and_monotonic_freeze() {
    let f = Fixture::new();
    for value in [json!("other"), json!(null), json!(false), json!({})] {
        assert_eq!(
            catalog(
                json!({"preset":"worker", "params":{"confine":value}}),
                &f.route,
                &f.ctx
            )
            .unwrap_err()
            .code,
            "invalid_confine"
        );
    }
    for (preset, scoped) in [("head", true), ("reader", true), ("worker", false)] {
        let route = identity(&f.pin, "unscoped", 7, scoped);
        assert_eq!(
            catalog(
                json!({"preset":preset,"params":{"confine":"root"}}),
                &route,
                &f.ctx
            )
            .unwrap_err()
            .code,
            "confine_requires_scoped_worker"
        );
    }
    assert!(tool_provider::describe()["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("tool-catalog-confine/v1")));
    catalog(
        json!({"preset":"worker","params":{"remote_exec":{"enabled":false}}}),
        &f.route,
        &f.ctx,
    )
    .unwrap();
    assert!(confined(&f.ctx, &f.route).unwrap());
    let other = identity(&f.pin, "other-worker", 7, true);
    catalog(
        json!({"preset":"worker","params":{"remote_exec":{"enabled":false}}}),
        &other,
        &f.ctx,
    )
    .unwrap();
    assert!(!confined(&f.ctx, &other).unwrap());
    catalog(
        json!({"preset":"worker","params":{"confine":"root"}}),
        &other,
        &f.ctx,
    )
    .unwrap();
    assert!(confined(&f.ctx, &other).unwrap());
    let unconfined = identity(&f.pin, "ordinary-worker", 7, true);
    assert!(!confined(&f.ctx, &unconfined).unwrap());
    assert!(call(&f.ctx, &unconfined, "read", arguments("read", "only.rs")).success);
    assert!(admit_confined(&f.ctx, &unconfined, "bash").is_ok());
}

#[test]
fn confined_epoch_refetch_cannot_remove_fence() {
    let f = Fixture::new();
    for (epoch, params) in [
        (8, json!({"remote_exec":{"enabled":true}})),
        (9, json!({"siblings":[f.live]})),
    ] {
        let route = identity(&f.pin, "designer", epoch, true);
        let answer = catalog(json!({"preset":"worker","params":params}), &route, &f.ctx).unwrap();
        assert_eq!(answer["tools"].as_array().unwrap().len(), 4, "{answer}");
        assert!(confined(&f.ctx, &route).unwrap());
        for name in ["bash", "write"] {
            assert_eq!(
                call(&f.ctx, &route, name, json!({})).data["code"],
                "tool_not_offered_confined"
            );
        }
        assert_eq!(
            call(
                &f.ctx,
                &route,
                "read",
                arguments("read", "../live/sentinel.rs")
            )
            .data["code"],
            "path_outside_confined_root"
        );
    }
    let route = identity(&f.pin, "designer", 10, true);
    let error = catalog(
        json!({"preset":"worker","params":{"confine":"none"}}),
        &route,
        &f.ctx,
    )
    .unwrap_err();
    assert_eq!(error.code, "invalid_confine");
    assert!(confined(&f.ctx, &route).unwrap());
}

fn with_outline_outside_ignores(f: impl FnOnce(&Fixture)) {
    let _lock = crate::test_env::process_env_lock();
    struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
    let home = tempfile::tempdir().unwrap();
    let _restore = RestoreEnv(
        ["HOME", "USERPROFILE", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|name| {
                let previous = std::env::var_os(name);
                std::env::set_var(name, home.path());
                (name, previous)
            })
            .collect(),
    );
    std::fs::write(home.path().join("excludes"), "global.rs\n").unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        format!(
            "[core]\nexcludesFile = {}\n",
            home.path().join("excludes").display()
        ),
    )
    .unwrap();
    let fixture = Fixture::new();
    std::fs::write(fixture.repo.path().join(".gitignore"), "ancestor.rs\n").unwrap();
    std::fs::write(fixture.repo.path().join(".git/info/exclude"), "info.rs\n").unwrap();
    std::fs::write(fixture.live.join("ignores"), "symlink.rs\n").unwrap();
    std::os::unix::fs::symlink("../live/ignores", fixture.pin.join(".gitignore")).unwrap();
    std::fs::create_dir(fixture.pin.join("nested")).unwrap();
    std::os::unix::fs::symlink("../../live/ignores", fixture.pin.join("nested/.gitignore"))
        .unwrap();
    for path in [
        "global.rs",
        "ancestor.rs",
        "info.rs",
        "symlink.rs",
        "nested/symlink.rs",
    ] {
        std::fs::write(fixture.pin.join(path), "fn pinned_token() {}\n").unwrap();
    }
    f(&fixture);
}

#[test]
fn confined_outline_files_ignores_no_outside_rules() {
    with_outline_outside_ignores(|f| {
        let response = f.call("outline", json!({"target":".", "files":true}));
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["complete"], true, "{response:?}");
        let files = response.data["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            [
                ".git",
                "ancestor.rs",
                "global.rs",
                "info.rs",
                "nested/symlink.rs",
                "only.rs",
                "symlink.rs"
            ],
            "outside ignore rules changed the pinned tree: {response:?}"
        );
    });
}

#[test]
fn confined_outline_test_includes_do_not_probe_unvalidated_paths() {
    let f = Fixture::new();
    std::fs::write(
        f.pin.join("lib.rs"),
        "pub fn product() {}\n#[cfg(test)]\nmod tests;\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("../live/sentinel.rs", f.pin.join("tests.rs")).unwrap();
    let present = f.call("outline", json!({"target":["lib.rs"]}));
    assert!(present.success, "{present:?}");
    std::fs::remove_file(f.live.join("sentinel.rs")).unwrap();
    let absent = f.call("outline", json!({"target":["lib.rs"]}));
    assert!(absent.success, "{absent:?}");
    assert_eq!(
        present.data, absent.data,
        "outside file existence affected the outline"
    );
}

#[test]
fn confined_security_golden_vectors() {
    let f = Fixture::new();
    let route = identity(&f.pin, "designer", 8, true);
    catalog(
        json!({"preset":"worker","params":{"siblings":[f.live]}}),
        &route,
        &f.ctx,
    )
    .unwrap();
    vector(
        &f,
        "epoch-refetch-bash-refusal",
        serde_json::to_value(call(&f.ctx, &route, "bash", json!({}))).unwrap(),
    );
    vector(
        &f,
        "epoch-refetch-invalid-confine",
        serde_json::to_value(
            catalog(
                json!({"preset":"worker","params":{"confine":"none"}}),
                &route,
                &f.ctx,
            )
            .unwrap_err(),
        )
        .unwrap(),
    );
    with_outline_outside_ignores(|f| {
        for (case, target) in [
            ("outside-ignore-files", "."),
            ("symlink-ignore-target", "nested"),
        ] {
            let response = f.call("outline", json!({"target":target,"files":true}));
            assert!(response.success, "{response:?}");
            vector(
                f,
                &format!("outline-{case}"),
                serde_json::to_value(response).unwrap(),
            );
        }
    });
}

fn vector(f: &Fixture, name: &str, value: Value) {
    fn stable(value: Value, root: &str) -> Value {
        match value {
            Value::String(text) => Value::String(text.replace(root, "<fixture>")),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| stable(item, root)).collect())
            }
            Value::Object(items) => Value::Object(
                items
                    .into_iter()
                    .map(|(key, value)| {
                        let value = if key == "search_ms" {
                            json!(0.0)
                        } else {
                            stable(value, root)
                        };
                        (key, value)
                    })
                    .collect(),
            ),
            value => value,
        }
    }
    let value = stable(value, f.pin.parent().unwrap().to_str().unwrap());
    let bytes = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/confine")
        .join(format!("{name}.json"));
    if std::env::var_os("AFT_REGENERATE_CONFINE_VECTORS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        bytes,
        "vector drift: {}",
        path.display()
    );
}

#[test]
fn confined_handler_golden_vectors() {
    let f = Fixture::new();
    let request = json!({"op":"tool.catalog", "preset":"worker", "params":{"confine":"root"}});
    vector(&f, "catalog-request", request.clone());
    vector(
        &f,
        "catalog-reply",
        catalog(request, &f.route, &f.ctx).unwrap(),
    );
    for name in ["read", "grep", "glob", "outline"] {
        for (route, input) in f.escapes() {
            let response = f.call(name, arguments(name, &input));
            assert_eq!(response.data["code"], "path_outside_confined_root");
            vector(
                &f,
                &format!("{name}-{route}"),
                serde_json::to_value(response).unwrap(),
            );
        }
    }
    for name in ["grep", "glob"] {
        let response =
            crate::grep_executor::with_walk_limit(0, || f.call(name, arguments(name, ".")));
        assert_eq!(response.data["complete"], false, "{name}: {response:?}");
        assert_eq!(
            response.data["walk_truncated"], true,
            "{name}: {response:?}"
        );
        let text = crate::subc_format::format_response(name, &response, false);
        assert!(text.contains("(walk)"), "{name}: {text}");
        vector(
            &f,
            &format!("{name}-walk"),
            json!({"native":serde_json::to_value(response).unwrap(),"text":text}),
        );
    }
}

/// Records a finished bash task owned by the fixture's "designer" session,
/// with its output stored outside the pinned root, and returns its paths.
fn owned_task_output(f: &Fixture) -> crate::bash_background::persistence::TaskPaths {
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    let paths = task_paths(f.storage.path(), "designer", "bash-0123456789abcdef").unwrap();
    let mut metadata = PersistedTask::starting(
        "bash-0123456789abcdef".into(),
        "designer".into(),
        "fixture".into(),
        f.pin.clone(),
        Some(f.pin.clone()),
        None,
        false,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    std::fs::write(&paths.stdout, "fn output() {}\n").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    f.ctx
        .bash_background()
        .replay_session(f.storage.path(), "designer")
        .unwrap();
    assert!(f
        .ctx
        .bash_background()
        .is_session_owned_artifact_path("designer", &paths.stdout));
    paths
}

#[test]
fn confined_grants_and_owned_outputs_cannot_escape() {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    let f = Fixture::new();
    let scope = access_grants::policy_scope_key(&f.route);
    let db = f.ctx.db().unwrap();
    crate::db::access_grants::insert(
        &db.lock().unwrap(),
        &scope,
        &AccessGrant {
            path: f.live.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    let paths = owned_task_output(&f);
    for name in ["read", "grep", "glob", "outline"] {
        for input in [f.live.join("sentinel.rs"), paths.stdout.clone()] {
            let response = f.call(name, arguments(name, input.to_str().unwrap()));
            assert!(
                response.data["message"]
                    .as_str()
                    .unwrap()
                    .contains(input.to_str().unwrap()),
                "{name}: {response:?}"
            );
            assert_eq!(
                response.data["code"], "path_outside_confined_root",
                "{name}: {response:?}"
            );
        }
    }
}

/// An unconfined glob that names a session's own bash output answers from the
/// task record (source "task-output"), and those files live outside the
/// pinned root. A confined glob must take its confined walk before that
/// shortcut, so even when it carries the owning session it never lists them.
#[test]
fn confined_glob_with_explicit_session_never_returns_task_output() {
    let f = Fixture::new();
    let paths = owned_task_output(&f);
    // Unconfined, a glob naming the owning session's output file is answered
    // from the task record instead of a directory walk.
    let (files, source, _) = crate::commands::glob::glob_root_files_for_test(
        &f.ctx,
        &f.pin,
        &paths.stdout,
        "**/*",
        16,
        Some("designer"),
    );
    assert_eq!(source, "task-output");
    assert_eq!(files, vec![paths.stdout.clone()]);

    // Confined, the same discovery neither answers from the task record nor
    // starts a walk at the outside file: it finds nothing and visits nothing.
    let opens = crate::bounded_io::tests::watch_opens(&paths.stdout);
    let (files, source, visited) =
        confinement::with_root(&f.ctx, "confine-glob-task-output", || {
            Ok(crate::commands::glob::glob_root_files_for_test(
                &f.ctx,
                &f.pin,
                &paths.stdout,
                "**/*",
                16,
                Some("designer"),
            ))
        })
        .unwrap();
    assert_eq!(source, "fallback");
    assert!(files.is_empty(), "{files:?}");
    assert_eq!(visited, 0);
    assert_eq!(opens.opens.load(std::sync::atomic::Ordering::Relaxed), 0);

    // Routed as the owning session, naming the output file or its folder.
    let outputs = [&paths.stdout, &paths.stderr, &paths.exit, &paths.json];
    for input in [&paths.stdout, &paths.dir] {
        let response = f.call(
            "glob",
            json!({"pattern":"**/*", "path": input.to_str().unwrap()}),
        );
        assert_eq!(
            response.data["code"], "path_outside_confined_root",
            "{response:?}"
        );
        let text = response.data.to_string();
        for output in outputs {
            let name = output.file_name().unwrap().to_str().unwrap();
            assert!(
                input == output || !text.contains(name),
                "{output:?} in {text}"
            );
        }
    }
}

/// Every confined walk checks its own starting path, not only the entries
/// under it, against the same gate: inside the confined root and admitted by
/// the read policy. The request's earlier path check normally refuses such a
/// path first; these calls skip it, so only the walks' own check stands.
#[test]
fn confined_walks_refuse_a_starting_path_outside_the_fence() {
    use std::sync::atomic::Ordering::Relaxed;
    let f = denied_tree();
    let sentinel = f.live.join("sentinel.rs");
    let pattern = "outside_secret|pinned_token";
    let glob = |start: &Path| {
        crate::commands::glob::glob_root_files_for_test(&f.ctx, &f.pin, start, "**/*", 16, None)
    };
    let grep = |start: &Path| crate::grep_executor::grep_root_for_test(&f.ctx, start, pattern);
    let outline = |start: &Path, files_mode: bool| {
        crate::commands::outline::discover_outline_paths_for_test(start, files_mode)
    };

    // Unconfined, each walk does reach the outside tree.
    let sentinel_text = sentinel.display().to_string();
    assert_eq!(glob(&f.live).0, vec![sentinel.clone()]);
    assert_eq!(glob(&sentinel).0, vec![sentinel.clone()]);
    assert_eq!(grep(&f.live).total_matches, 1);
    assert_eq!(grep(&sentinel).total_matches, 1);
    for files_mode in [false, true] {
        assert!(outline(&f.live, files_mode).contains(&sentinel_text));
    }

    let watches = [
        crate::bounded_io::tests::watch_opens(&sentinel),
        crate::bounded_io::tests::watch_opens(&f.pin.join("secret.rs")),
        crate::bounded_io::tests::watch_opens(&f.pin.join("vault/inner.rs")),
    ];
    confinement::with_root(&f.ctx, "confine-walk-start", || {
        // Outside the root; a link inside the root to outside; and in-root
        // paths the read policy denies (a file and a directory).
        let directories = [f.live.clone(), f.pin.join("escape"), f.pin.join("vault")];
        let files = [
            sentinel.clone(),
            f.pin.join("escape-file"),
            f.pin.join("secret.rs"),
        ];
        for start in directories.iter().chain(&files) {
            let refusal = confinement::check_walk_root("confine-walk-start", start).unwrap_err();
            assert_eq!(refusal.data["code"], "path_outside_confined_root");
            let (found, _, visited) = glob(start);
            assert!(
                found.is_empty() && visited == 0,
                "glob {start:?}: {found:?}"
            );
            let result = grep(start);
            assert_eq!(
                (result.total_matches, result.files_searched),
                (0, 0),
                "grep {start:?}"
            );
        }
        for start in &directories {
            for files_mode in [false, true] {
                let found = outline(start, files_mode);
                assert!(found.is_empty(), "outline {start:?}: {found:?}");
            }
        }
        // No walk listed anything under a refused start: a walk that did
        // would have met entries the read policy denies and counted them.
        assert_eq!(confinement::denied_count(), 0);
        // The pinned root itself still starts every walk.
        assert!(confinement::check_walk_root("confine-walk-start", &f.pin).is_ok());
        assert!(!glob(&f.pin).0.is_empty());
        assert!(grep(&f.pin).total_matches > 0);
        for files_mode in [false, true] {
            assert!(!outline(&f.pin, files_mode).is_empty());
        }
        Ok(())
    })
    .unwrap();
    for watch in &watches {
        assert_eq!(watch.opens.load(Relaxed), 0);
    }
}

fn pattern_cases(f: &Fixture) -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "glob-parent-pattern",
            "glob",
            json!({"pattern":"../live/**"}),
        ),
        (
            "glob-absolute-pattern",
            "glob",
            json!({"pattern":format!("{}/*", f.live.display())}),
        ),
        (
            "glob-nested-parent-pattern",
            "glob",
            json!({"pattern":"**/../../live/*"}),
        ),
        (
            "grep-parent-include",
            "grep",
            json!({"pattern":"outside_secret|pinned_token", "include":"../live/*"}),
        ),
        (
            "grep-outside-excludes",
            "grep",
            json!({"pattern":"outside_secret|pinned_token", "include":"**/*.rs", "exclude":["../live/*", "**/../../live/*", format!("{}/*", f.live.display())]}),
        ),
    ]
}

fn assert_pinned_results(f: &Fixture, name: &str, response: &Response) -> Vec<String> {
    assert!(response.success, "{name}: {response:?}");
    assert_eq!(response.data["complete"], true, "{name}: {response:?}");
    assert_eq!(
        response.data["fallback"], "filesystem",
        "{name}: {response:?}"
    );
    let paths = if name == "grep" {
        response.data["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["file"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    } else {
        response.data["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| path.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    for path in &paths {
        assert!(
            Path::new(path).starts_with(&f.pin),
            "{name} escaped through {path}"
        );
        assert_eq!(
            Path::new(path).canonicalize().unwrap(),
            f.pin.join("only.rs")
        );
    }
    assert!(
        !serde_json::to_string(response)
            .unwrap()
            .contains("outside_secret"),
        "{name}: {response:?}"
    );
    paths
}

#[test]
fn confined_patterns_filter_only_pinned_walk_candidates() {
    let f = Fixture::new();
    for (case, name, args) in pattern_cases(&f) {
        let response = f.call(name, args);
        let paths = assert_pinned_results(&f, name, &response);
        if case == "grep-outside-excludes" {
            assert_eq!(paths, [f.pin.join("only.rs").display().to_string()]);
        } else {
            assert!(paths.is_empty(), "{case}: {response:?}");
        }
    }
    for name in ["grep", "glob"] {
        assert_eq!(
            assert_pinned_results(&f, name, &f.call(name, arguments(name, "."))),
            [f.pin.join("only.rs").display().to_string()]
        );
    }
}

#[test]
fn confined_grep_rayon_scans_inherit_fence() {
    let f = Fixture::new();
    let response = f.call("grep", arguments("grep", "."));
    assert_eq!(
        assert_pinned_results(&f, "grep", &response),
        [f.pin.join("only.rs").display().to_string()]
    );
    assert!(response.data["files_searched"].as_u64().unwrap() > 0);
}

fn read_on_filesystem_helper(f: &Fixture, input: &str) -> Response {
    let ctx = Arc::new(context(&f.pin, f.storage.path(), None));
    ctx.update_config(|config| config.restrict_to_project_root = false);
    let request = crate::protocol::RawRequest {
        id: "confine-helper".into(),
        command: "read".into(),
        lsp_hints: None,
        session_id: Some(f.route.session.clone()),
        params: json!({"file":input}),
    };
    let helper_ctx = ctx.clone();
    let caller_thread = std::thread::current().id();
    with_call_policy(
        &ctx,
        &f.route,
        &request.id.clone(),
        "read",
        json!({}),
        |_| {
            crate::bounded_io::run(&f.pin, None, move |_| {
                assert_ne!(std::thread::current().id(), caller_thread);
                assert!(
                    confinement::for_request(&request.id),
                    "filesystem helper lost the confinement fence"
                );
                assert!(helper_ctx.request_force_restrict(&request.id));
                let asks = confinement::ask_calls();
                let response = crate::commands::read::handle_read(&request, &helper_ctx);
                assert_eq!(confinement::ask_calls(), asks);
                Ok(response)
            })
            .unwrap()
        },
    )
    .unwrap()
}

#[test]
fn confined_read_helper_inherits_fence_and_refuses_symlink_targets() {
    let f = Fixture::new();
    for input in ["escape/sentinel.rs", "escape-file"] {
        let response = read_on_filesystem_helper(&f, input);
        assert_eq!(
            response.data["code"], "path_outside_confined_root",
            "{response:?}"
        );
        assert!(response.data["message"].as_str().unwrap().contains(input));
        let text = serde_json::to_string(&response).unwrap();
        assert!(!text.contains(f.live.to_str().unwrap()));
        assert!(!text.contains("outside_secret"));
    }
}

#[test]
fn confined_in_root_symlink_is_allowed_and_walks_skip_outside_links() {
    let f = Fixture::new();
    std::os::unix::fs::symlink("only.rs", f.pin.join("safe-file.rs")).unwrap();
    for name in ["read", "grep", "glob", "outline"] {
        let response = f.call(name, arguments(name, "safe-file.rs"));
        assert!(response.success, "{name}: {response:?}");
        if name == "read" || name == "outline" {
            assert!(
                serde_json::to_string(&response)
                    .unwrap()
                    .contains("pinned_token"),
                "{name}: {response:?}"
            );
        } else {
            assert_eq!(
                assert_pinned_results(&f, name, &response),
                [f.pin.join("only.rs").display().to_string()]
            );
        }
    }
    std::os::unix::fs::symlink("../live/sentinel.rs", f.pin.join("outside-file.rs")).unwrap();
    for name in ["grep", "glob"] {
        let response = f.call(name, arguments(name, "."));
        assert_eq!(
            assert_pinned_results(&f, name, &response),
            [f.pin.join("only.rs").display().to_string()]
        );
    }
}

#[test]
fn confined_review_golden_vectors() {
    let f = Fixture::new();
    for (case, name, args) in pattern_cases(&f) {
        let response = f.call(name, args);
        assert_pinned_results(&f, name, &response);
        vector(
            &f,
            &format!("review-{case}"),
            serde_json::to_value(response).unwrap(),
        );
    }
    for (case, input) in [
        ("directory-symlink", "escape/sentinel.rs"),
        ("file-symlink", "escape-file"),
    ] {
        let response = read_on_filesystem_helper(&f, input);
        assert_eq!(response.data["code"], "path_outside_confined_root");
        vector(
            &f,
            &format!("review-read-helper-{case}"),
            serde_json::to_value(response).unwrap(),
        );
    }
    for name in ["grep", "glob"] {
        let response = f.call(name, arguments(name, "."));
        assert_eq!(
            assert_pinned_results(&f, name, &response),
            [f.pin.join("only.rs").display().to_string()]
        );
        vector(
            &f,
            &format!("review-{name}-walk-encounters-outside-symlink"),
            serde_json::to_value(response).unwrap(),
        );
    }
}
