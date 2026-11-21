mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::bash_rewrite::bash_hints::{
        code_search_nudge, fresh_at, operands, pipeline_stages, statements, token,
    };

    const SEARCH_HINT: &str = "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";
    const GREP_HINT: &str = "DO NOT search code by running grep/rg in shell — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

    fn decide(
        output: &str,
        command: &str,
        root: Option<&Path>,
        cwd: Option<&str>,
    ) -> Option<String> {
        code_search_nudge(
            output,
            command,
            root,
            cwd,
            Some("aft_search"),
            Some("grep"),
            "bash",
        )
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/app.ts"), "foo\n").unwrap();
        old(&root.path().join("src"));
        root
    }

    fn old(path: &Path) {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
    }

    #[test]
    fn code_search_detection_matches_plugin_statements_and_filters() {
        for command in [
            r#"grep -nE "x" src/"#,
            "grep foo file.ts | head",
            "rg -n pat",
            "cd packages/x && grep -rn foo .",
            "cd \"my dir\" && rg 'p' .",
            "\"grep\" -n pat file",
            "grep pat file || true",
            "grep \"a|b\" file | head",
            "cd x; grep foo",
            "false || grep pat",
            "echo y & rg pat",
            "cd ~/proj && echo '=== marker ===' && grep -rn foo src/ | head -20",
            "cd ~/proj\necho '=== marker ==='\ngrep -rn foo src/ | head -20",
        ] {
            assert_eq!(
                decide("matches", command, None, None).as_deref(),
                Some(SEARCH_HINT),
                "{command}"
            );
        }
        for command in [
            "bun test | grep fail",
            "cargo build 2>&1 | rg error",
            "echo hi | grep h",
            "make test | grep -i pass",
            "ls -la",
            "FOO=1 grep pat file",
            "2>&1 grep pat",
            "cd \"unclosed && grep foo",
            "grep 'unclosed",
            "grep foo `unclosed",
            "cd x && bun test | grep fail",
            "echo 'grep is mentioned here' && ls",
            "echo $(echo hi; grep foo)",
            "echo 'a; grep foo'",
            "echo \"a && rg foo\"",
        ] {
            assert_eq!(decide("matches", command, None, None), None, "{command}");
        }
    }

    #[test]
    fn code_search_nudge_output_and_tool_names_are_pinned() {
        assert_eq!(decide("", "grep foo", None, None), None);
        assert_eq!(
            decide(&format!("matches\n\n{SEARCH_HINT}"), "grep foo", None, None),
            None
        );
        assert_eq!(decide(GREP_HINT, "grep foo", None, None), None);
        assert_eq!(
            code_search_nudge("matches", "rg foo", None, None, None, Some("grep"), "shell")
                .as_deref(),
            Some(GREP_HINT)
        );
        assert_eq!(
            code_search_nudge(
                "matches",
                "rg foo",
                None,
                None,
                Some("search"),
                None,
                "bash"
            )
            .unwrap(),
            SEARCH_HINT.replace("aft_search", "search")
        );
        assert_eq!(
            code_search_nudge("matches", "rg foo", None, None, None, None, "bash"),
            None
        );
        for root in [None, Some(Path::new("")), Some(Path::new("   "))] {
            assert_eq!(
                decide("matches", "rg foo missing.ts", root, None).as_deref(),
                Some(SEARCH_HINT)
            );
        }
    }

    #[test]
    fn code_search_nudge_standalone_appends_once_only_to_native_terminal_results() {
        let root = fixture();
        let context = crate::subc_format::BashCodeSearchHint {
            command: "cd src && echo marker && rg foo . | head".into(),
            workdir: None,
            search_tool: Some("aft_search".into()),
            grep_tool: Some("grep".into()),
            shell_tool: "bash".into(),
            runner_notice: None,
        };
        let mut response = crate::protocol::Response::success(
            "call",
            serde_json::json!({"task_id": "native", "status": "completed", "exit_code": 0, "output": "matches"}),
        );
        context.deliver(&mut response, root.path());
        assert_eq!(response.data["output"], format!("matches\n\n{SEARCH_HINT}"));
        let before = serde_json::to_vec(&response).unwrap();
        context.deliver(&mut response, root.path());
        assert_eq!(serde_json::to_vec(&response).unwrap(), before);
        for data in [
            serde_json::json!({"task_id": "native", "status": "running", "output": "background launch"}),
            serde_json::json!({"task_id": "native", "status": "completed", "output": "background launch"}),
            serde_json::json!({"output": "rewritten grep result"}),
        ] {
            let mut response = crate::protocol::Response::success("call", data.clone());
            context.deliver(&mut response, root.path());
            assert_eq!(response.data, data);
        }
    }

    #[test]
    fn code_search_suppression_matches_plugin_paths_options_and_redirections() {
        let root = fixture();
        let root = Some(root.path());
        for command in [
            "grep -rn foo",
            "grep foo ./src",
            "grep $PAT ./src",
            "cd src && grep -n foo .",
            "grep -f ~/pat.txt foo src/",
            "grep -e foo src",
            "rg --regexp=foo src",
            "rg -g '*.ts' -t typescript foo src",
            "grep -rn foo src 2>/dev/null",
            "grep -rn foo 2>/dev/null",
            "grep -rn foo missing.ts src | head -5",
        ] {
            assert_eq!(
                decide("hit", command, root, None).as_deref(),
                Some(SEARCH_HINT),
                "{command}"
            );
        }
        for command in [
            "grep foo /etc/hosts",
            "grep x ~/.config/opencode/aft.jsonc",
            "grep -rn foo $DIR",
            "L=~/.local/share/x/logs/a.log; grep -n abc123 $L | tail -8 | cut -c1-230",
            "grep x \"$HOME/foo\"",
            "grep x '$PROJECT/foo'",
            "cd x && echo y && grep z ~/outside/f",
            "cd /other/repo/src && grep -n foo tools/bash.ts",
            "cd /other/repo\necho scanning\ngrep -n foo tools/bash.ts",
            "cd $DIR && grep -n foo file.ts",
            "cd - && grep -n foo",
            "rg -n foo missing.ts",
            "grep -n foo src/missing.ts | head -5",
            "grep -n foo src/app.ts | head -5",
            "grep -n foo src/app.ts src/ | head -5",
            "grep '' src/app.ts",
            "grep -rn foo /etc/hosts 2>/dev/null",
        ] {
            assert_eq!(decide("hit", command, root, None), None, "{command}");
        }
        assert_eq!(decide("hit", "rg foo $DIR", None, None), None);
        assert_eq!(
            decide("hit", "cd '' && rg foo src", root, None).as_deref(),
            Some(SEARCH_HINT)
        );
    }

    #[test]
    fn code_search_suppression_resolves_workdir_separately_from_project() {
        let root = fixture();
        let outside = fixture();
        for cwd in [
            "src".to_string(),
            root.path().join("src").to_string_lossy().into_owned(),
        ] {
            assert!(decide("hit", "rg foo .", Some(root.path()), Some(&cwd)).is_some());
            assert_eq!(
                decide(
                    "hit",
                    "cd .. && rg foo src/app.ts",
                    Some(root.path()),
                    Some(&cwd)
                ),
                None
            );
        }
        assert_eq!(
            decide(
                "hit",
                "rg foo src",
                Some(root.path()),
                outside.path().to_str()
            ),
            None
        );
        std::fs::create_dir(root.path().join("fresh")).unwrap();
        assert_eq!(decide("hit", "rg foo fresh", Some(root.path()), None), None);
        old(&root.path().join("src/app.ts"));
        assert_eq!(
            decide("hit", "rg foo src/app.ts", Some(root.path()), None),
            None
        );
    }

    #[test]
    fn code_search_lexer_preserves_windows_backslashes() {
        for windows in [false, true] {
            for (input, expected) in [
                (r#""a\b""#, r"a\b"),
                (r#""a\"b""#, "a\"b"),
                (r#""a\$b""#, "a$b"),
                (r#""a\\b""#, r"a\b"),
                ("\"a\\\nb\"", "ab"),
            ] {
                assert_eq!(
                    token(input, 0, windows),
                    Some((expected.into(), input.len()))
                );
            }
        }
        assert_eq!(token(r"C:\a\b\", 0, true), Some((r"C:\a\b\".into(), 7)));
        assert_eq!(token(r"r\g foo .", 0, false).unwrap().0, "rg");
        assert_eq!(token(r"r\g foo .", 0, true).unwrap().0, r"r\g");
        for command in [
            r"rg foo C:\proj\src\| head",
            r"rg foo C:\proj\src\; echo done",
            r"rg foo C:\proj\src\&& echo done",
        ] {
            let parts = statements(command, true).unwrap();
            let pipeline = pipeline_stages(parts[0], true).unwrap();
            assert_eq!(operands(pipeline[0], 2, true), vec![r"C:\proj\src\"]);
        }
    }

    #[test]
    fn code_search_freshness_matches_plugin_clock_slack() {
        let now = UNIX_EPOCH + Duration::from_millis(1_800_000_000_000);
        assert!(fresh_at(now + Duration::from_micros(600), now));
        assert!(!fresh_at(now + Duration::from_secs(5), now));
        assert!(fresh_at(now - Duration::from_millis(59_999), now));
        assert!(!fresh_at(now - Duration::from_secs(60), now));
    }
}
