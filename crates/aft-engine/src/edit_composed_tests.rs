mod cases {
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::edit::{auto_backup_observed, read_edit_source};
    #[cfg(unix)]
    use crate::format::{
        auto_format, clear_tool_cache, first_batch_skip_reason_for_test,
        ruff_probe_refused_for_test, validate_full,
    };
    #[cfg(unix)]
    use aft_shell::test_bridge::{SpawnPlanTestExt as _, ToolLaunchPolicyTestExt as _};
    #[cfg(unix)]
    use std::{fs, process::Command};

    #[cfg(unix)]
    fn tool_cache_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(unix)]
    #[test]
    fn ruff_policy_refusal_does_not_poison_another_sessions_version_cache() {
        use crate::sandbox_spawn::{SpawnPlan, ToolLaunchPolicy};
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        let runs = root.path().join("ruff-runs");
        let ruff = bin.join("ruff");
        crate::executable_cache::write_executable(&ruff, format!(
            "#!/bin/sh\nif [ \"$1\" = --warm-up ]; then exit 0; fi\nif [ \"$1\" = --version ]; then printf 'probe\\n' >> '{0}'; printf 'ruff 0.1.2\\n'; else printf 'format\\n' >> '{0}'; fi\n",
            runs.display()
        ).as_bytes()).unwrap();
        // Warm the fixture before timing the probe: macOS's first-run security
        // check can otherwise consume the short version deadline.
        assert!(Command::new(&ruff)
            .arg("--warm-up")
            .status()
            .unwrap()
            .success());
        fs::write(root.path().join("ruff.toml"), "\n").unwrap();
        let path = root.path().join("main.py");
        fs::write(&path, "print('hello')\n").unwrap();
        let config = Config {
            format_on_edit: true,
            project_root: Some(root.path().to_path_buf()),
            ..Config::default()
        };
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        );
        let refused = crate::sandbox_spawn::with_spawn_plan_for_test(
            SpawnPlan::refused_for_test("sandbox_unavailable"),
            || ToolLaunchPolicy::resolve(&ctx),
        );
        let allowed = ToolLaunchPolicy::resolve(&ctx);
        let refused_result = crate::log_ctx::with_session(Some("ruff-worker".into()), || {
            auto_format(&refused, &path, &config)
        });
        assert!(!runs.exists(), "a refused session must not run Ruff");
        let allowed_result = crate::log_ctx::with_session(Some("ruff-head".into()), || {
            auto_format(&allowed, &path, &config)
        });
        assert_eq!(
            allowed_result,
            (true, None),
            "another session's refusal must not cache tool absence"
        );
        assert_eq!(refused_result, (false, Some("sandbox_unavailable".into())));
        let warm_refusal = crate::log_ctx::with_session(Some("ruff-worker".into()), || {
            auto_format(&refused, &path, &config)
        });
        assert_eq!(warm_refusal, (false, Some("sandbox_unavailable".into())));
        let warm_allowed = crate::log_ctx::with_session(Some("ruff-head".into()), || {
            auto_format(&allowed, &path, &config)
        });
        assert_eq!(warm_allowed, (true, None));
        assert_eq!(fs::read_to_string(runs).unwrap(), "probe\nformat\nformat\n");
    }

    #[cfg(unix)]
    #[test]
    fn formatter_checker_and_ruff_probe_refuse_without_running_project_code() {
        use crate::sandbox_spawn::{SpawnPlan, ToolLaunchPolicy};
        use std::os::unix::fs::PermissionsExt;
        let _guard = tool_cache_test_lock();
        clear_tool_cache();
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("node_modules/.bin");
        fs::create_dir_all(&bin).unwrap();
        let marker = root.path().join("ran");
        for tool in ["biome", "ruff"] {
            let path = bin.join(tool);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\n: > '{}'\nprintf 'ruff 0.1.2\\n'\n",
                    marker.display()
                ),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::write(root.path().join("biome.json"), "{}").unwrap();
        let path = root.path().join("test.js");
        fs::write(&path, "let answer = 1;\n").unwrap();
        let config = Config {
            format_on_edit: true,
            project_root: Some(root.path().to_path_buf()),
            ..Config::default()
        };
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        );
        let policy = crate::sandbox_spawn::with_spawn_plan_for_test(
            SpawnPlan::refused_for_test("sandbox_unavailable"),
            || ToolLaunchPolicy::resolve(&ctx),
        );
        assert_eq!(
            auto_format(&policy, &path, &config),
            (false, Some("error".into()))
        );
        assert_eq!(
            validate_full(&policy, &path, &config).1.as_deref(),
            Some("error")
        );
        let skipped_reason = first_batch_skip_reason_for_test(&policy, &[path], &config);
        assert_eq!(skipped_reason.as_deref(), Some("error"));
        assert!(ruff_probe_refused_for_test(
            &policy,
            config.project_root.as_deref()
        ));
        assert!(
            !marker.exists(),
            "a refused launch must not run any project tool"
        );
    }

    #[cfg(unix)]
    #[test]
    fn swiftc_parse_cannot_bypass_the_supplied_tool_policy() {
        use crate::sandbox_spawn::{SpawnPlan, ToolLaunchPolicy};
        use aft_edit::edit::test_support::swift::{run_swiftc_for_test, ParseOutcome, TUPLE};
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("parsed");
        let compiler = root.path().join("swiftc");
        crate::executable_cache::write_executable(
            &compiler,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --warm-up ]; then exit 0; fi\n: > '{}'\ncat >/dev/null\n",
                marker.display()
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(Command::new(&compiler)
            .arg("--warm-up")
            .status()
            .unwrap()
            .success());
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let policy = crate::sandbox_spawn::with_spawn_plan_for_test(
            SpawnPlan::refused_for_test("sandbox_unavailable"),
            || ToolLaunchPolicy::resolve(&ctx),
        );
        let outcome = run_swiftc_for_test(&policy, &compiler, TUPLE);
        assert!(
            matches!(&outcome, ParseOutcome::Unavailable(reason) if reason.starts_with("sandbox_unavailable")),
            "{outcome:?}"
        );
        assert!(
            !marker.exists(),
            "a refused syntax checker must not run on the host"
        );
    }

    #[test]
    fn a03_stale_capture_backup_preserves_live_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("capture.txt");
        let ctx = AppContext::new(Box::new(crate::language::StubProvider), Config::default());
        std::fs::write(&path, "original\n").unwrap();
        let (_, observation) = read_edit_source(&path).unwrap();
        let replacement = temp.path().join("replacement.txt");
        std::fs::write(&replacement, "external\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let error = auto_backup_observed(
            &ctx,
            "freshness",
            &path,
            "before edit",
            Some("freshness-op"),
            &observation,
        )
        .unwrap_err();
        assert_eq!(error.code(), "file_changed_since_read");
        assert!(ctx.backup().lock().history("freshness", &path).is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"external\n");
    }

    #[test]
    fn observed_snapshot_rechecks_after_waiting_for_backup_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("observed.txt");
        std::fs::write(&path, "original\n").unwrap();
        let (_, observation) = read_edit_source(&path).unwrap();
        let ctx = AppContext::new(Box::new(crate::language::StubProvider), Config::default());

        std::thread::scope(|scope| {
            let store = ctx.backup().lock();
            let (ready, waiting) = std::sync::mpsc::sync_channel(0);
            let ctx = &ctx;
            let path = &path;
            let backup = scope.spawn(move || {
                crate::edit::BEFORE_OBSERVED_STORE_LOCK.with(|slot| {
                    slot.set(Some(Box::new(move || ready.send(()).unwrap())));
                });
                auto_backup_observed(ctx, "session", path, "before edit", None, &observation)
            });

            // The observation passed the first check while this thread still
            // holds the backup store. Replace the live file before releasing it.
            waiting.recv().unwrap();
            let replacement = temp.path().join("replacement.txt");
            std::fs::write(&replacement, "external\n").unwrap();
            std::fs::rename(replacement, path).unwrap();
            drop(store);

            assert_eq!(
                backup.join().unwrap().unwrap_err().code(),
                "file_changed_since_read"
            );
            assert!(ctx.backup().lock().history("session", path).is_empty());
            assert_eq!(std::fs::read(path).unwrap(), b"external\n");
        });
    }
}
