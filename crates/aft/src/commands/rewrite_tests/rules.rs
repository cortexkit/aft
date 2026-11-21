mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use crate::bash_rewrite::rules::{HeadRule, TailRule};
    use crate::bash_rewrite::{RewriteDecision, RewriteRule};
    use crate::config::Config;
    use crate::context::default_language_provider_factory;
    use crate::hashline::integration::RegistrationRequest;
    use crate::protocol::DEFAULT_SESSION_ID;
    use aft_engine::test_bridge::bash_remote::{
        find_request, grep_basic_regex_has_dialect_conflict, grep_request,
        should_suppress_grep_footer,
    };

    #[test]
    fn recursive_ls_declines_single_level_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        for command in ["ls -R", "ls -AR", "ls -RA ."] {
            assert!(aft_engine::test_bridge::bash_remote::bash_rewrite::rules::ls_request(command, &ctx).is_none(), "{command}");
        }
        assert!(aft_engine::test_bridge::bash_remote::bash_rewrite::rules::ls_request("ls -A", &ctx).is_some());
    }

    /// A directory read never lists `.` and `..`, which is the `ls -A`
    /// contract. `ls -a` prints them too, so only `-A` may be rewritten.
    #[test]
    fn ls_rewrites_dash_capital_a_and_keeps_dash_a_native() {
        let root = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let request = aft_engine::test_bridge::bash_remote::bash_rewrite::rules::ls_request("ls -A", &ctx).expect("ls -A is rewritten");
        assert_eq!(request["include_hidden"], json!(true));
        for command in ["ls -a", "ls -la", "ls -aA", "ls -Aa ."] {
            assert!(aft_engine::test_bridge::bash_remote::bash_rewrite::rules::ls_request(command, &ctx).is_none(), "{command}");
        }
    }

    // The oracle needs a real POSIX bash. On Windows `bash` on PATH is often the
    // WSL launcher, which fails without an installed distribution, so the byte
    // comparison runs on Unix runners only.
    #[cfg(unix)]
    #[test]
    fn literal_append_rewrites_match_bash_bytes() {
        use aft_engine::test_bridge::bash_remote::LITERAL_APPEND_CASES;
        use std::process::Command;

        if which::which("bash").is_err() {
            eprintln!("skipping bash byte oracle: bash is unavailable");
            return;
        }
        let native = tempfile::tempdir().unwrap();
        let rewritten = tempfile::tempdir().unwrap();
        let ctx = crate::test_context(
            default_language_provider_factory(),
            Config {
                project_root: Some(rewritten.path().to_path_buf()),
                experimental_bash_rewrite: true,
                restrict_to_project_root: true,
                ..Config::default()
            },
        );
        for command in LITERAL_APPEND_CASES {
            fs::write(native.path().join("notes.txt"), "existing\n").unwrap();
            fs::write(rewritten.path().join("notes.txt"), "existing\n").unwrap();
            let output = Command::new("bash")
                .args(["--noprofile", "--norc", "-c", command])
                .env_remove("BASH_ENV")
                .current_dir(native.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "bash failed: {command}: {output:?}"
            );
            let RewriteDecision::Accept(request) =
                crate::bash_rewrite::rules::CatAppendRule.decide(command, "literal-oracle", None, &ctx)
            else {
                panic!("must rewrite: {command}");
            };
            let response = crate::bash_rewrite::rules::CatAppendRule.execute(&request, &ctx);
            assert!(
                response.success || response.data["code"] == "no_change",
                "rewrite failed: {command}: {response:?}"
            );
            assert_eq!(
                fs::read(rewritten.path().join("notes.txt")).unwrap(),
                fs::read(native.path().join("notes.txt")).unwrap(),
                "byte mismatch: {command}"
            );
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.ts"), "foo\n").unwrap();
        dir
    }

    #[test]
    fn single_named_file_suppresses_grep_footer() {
        let dir = fixture();
        assert!(should_suppress_grep_footer(Some("src/app.ts"), dir.path()));
    }

    #[test]
    fn directory_path_keeps_grep_footer() {
        let dir = fixture();
        filetime::set_file_mtime(
            dir.path().join("src"),
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
        assert!(!should_suppress_grep_footer(Some("src"), dir.path()));
    }

    #[test]
    fn no_path_keeps_grep_footer() {
        let dir = fixture();
        assert!(!should_suppress_grep_footer(None, dir.path()));
    }

    #[test]
    fn external_file_suppresses_grep_footer() {
        let dir = fixture();
        let external = tempfile::NamedTempFile::new().unwrap();
        assert!(should_suppress_grep_footer(
            external.path().to_str(),
            dir.path()
        ));
    }

    #[test]
    fn freshly_modified_file_suppresses_grep_footer() {
        let dir = fixture();
        let file = dir.path().join("src/app.ts");
        fs::write(&file, "foo\nbar\n").unwrap();
        assert!(should_suppress_grep_footer(file.to_str(), dir.path()));
    }

    #[test]
    fn old_directory_inside_project_root_keeps_grep_footer() {
        let dir = fixture();
        let directory = dir.path().join("src");
        filetime::set_file_mtime(
            &directory,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
        assert!(!should_suppress_grep_footer(Some("src"), dir.path()));
    }

    #[test]
    fn old_file_inside_project_root_suppresses_grep_footer() {
        let dir = fixture();
        let file = dir.path().join("src/app.ts");
        filetime::set_file_mtime(
            &file,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
        assert!(should_suppress_grep_footer(Some("src/app.ts"), dir.path()));
    }

    #[test]
    fn grep_basic_regex_anchor_positions_and_escaped_operators_are_guarded() {
        for pattern in ["$HOME", "a$b", "x^y"] {
            assert!(grep_basic_regex_has_dialect_conflict(pattern), "{pattern}");
        }
        for pattern in ["foo$", "^fn "] {
            assert!(!grep_basic_regex_has_dialect_conflict(pattern), "{pattern}");
        }
        for pattern in [r"\(a\)", r"\{1\}", r"a\|b"] {
            assert!(grep_basic_regex_has_dialect_conflict(pattern), "{pattern}");
        }
    }

    #[test]
    fn grep_declines_basic_regex_tokens_with_extended_meanings() {
        assert_eq!(grep_request("grep -rn 'a+b' src", "grep"), None);
        assert_eq!(
            grep_request("rg -n 'a+b' src", "rg").unwrap()["pattern"],
            "a+b"
        );
        assert_eq!(
            grep_request("grep -rn 'a.b' src", "grep").unwrap()["pattern"],
            "a.b"
        );
    }

    /// Native grep without -r reads stdin when given no path and refuses a
    /// directory, so neither form may become a project search. With -r both
    /// search the tree, and rg keeps its own default of the working directory.
    #[test]
    fn grep_keeps_stdin_and_nonrecursive_directory_forms_native() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("match.txt"), "needle\n").unwrap();
        let ctx = crate::test_context(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let decide = |command: &str| crate::bash_rewrite::rules::GrepRule.decide(command, "grep-shape", None, &ctx);
        for command in [
            "grep needle",
            "grep -n needle",
            "grep needle .",
            "grep -in needle .",
        ] {
            assert!(
                !matches!(decide(command), RewriteDecision::Accept(_)),
                "{command} must stay native"
            );
        }
        for command in [
            "grep -r needle",
            "grep -r needle .",
            "grep -rn needle .",
            "grep needle match.txt",
        ] {
            assert!(
                matches!(decide(command), RewriteDecision::Accept(_)),
                "{command} must be rewritten"
            );
        }
        assert!(grep_request("rg needle", "rg").is_some());
    }

    #[test]
    fn find_absolute_path_uses_glob_path_arg() {
        assert_eq!(
            find_request(r#"find /tmp/foo -name "*.ts" -type f"#),
            Some(json!({ "path": "/tmp/foo", "pattern": "**/*.ts" }))
        );
    }

    #[test]
    fn find_dot_keeps_project_root_relative_pattern() {
        assert_eq!(
            find_request(r#"find . -name "*.ts" -type f"#),
            Some(json!({ "pattern": "**/*.ts" }))
        );
    }

    #[test]
    fn find_relative_path_uses_glob_path_arg() {
        assert_eq!(
            find_request(r#"find ./src -name "*.go" -type f"#),
            Some(json!({ "path": "./src", "pattern": "**/*.go" }))
        );
    }

    #[test]
    fn find_trims_trailing_slash_from_path_arg() {
        assert_eq!(
            find_request(r#"find /tmp/foo/ -type f -name "*.ts""#),
            Some(json!({ "path": "/tmp/foo", "pattern": "**/*.ts" }))
        );
    }

    #[test]
    fn find_requires_explicit_file_type() {
        assert_eq!(find_request(r#"find . -name sub"#), None);
    }

    #[test]
    fn find_filesystem_root_is_not_rewritten() {
        // `find /` must NOT rewrite — trimming the slash would yield "" which
        // resolves as the project root, silently searching the wrong scope.
        assert_eq!(find_request(r#"find / -name "*.rs" -type f"#), None);
        assert_eq!(find_request(r#"find // -type f -name "*.rs""#), None);
    }

    #[test]
    fn head_and_tail_rewrites_are_hashline_only_and_tail_keeps_absolute_numbering() {
        let dir = fixture();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::write(root.join("src/app.ts"), "one\ntwo\nthree\nfour\n").unwrap();
        let ctx = crate::test_context(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                experimental_bash_rewrite: true,
                ..Default::default()
            },
        );

        assert!(matches!(
            HeadRule.decide("head -2 src/app.ts", "head-off", None, &ctx),
            RewriteDecision::Decline(_)
        ));
        assert!(matches!(
            TailRule.decide("tail -2 src/app.ts", "tail-off", None, &ctx),
            RewriteDecision::Decline(_)
        ));

        ctx.hashline_bindings().register(
            &root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        let RewriteDecision::Accept(request) =
            TailRule.decide("tail -2 src/app.ts", "tail-on", None, &ctx)
        else {
            panic!("effective hashline tail should rewrite");
        };
        let response = TailRule.execute(&request, &ctx);
        assert!(response.success, "{}", response.data);
        let content = response.data["content"].as_str().unwrap();
        assert!(content.starts_with("[src/app.ts#"), "{content}");
        assert!(content.contains("3:three\n4:four\n"), "{content}");
    }

}
