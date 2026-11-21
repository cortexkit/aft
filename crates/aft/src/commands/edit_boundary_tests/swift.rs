mod tests {
    use crate::config::Config;
    use crate::edit::swift::{
        cached_lookup, parse_with_lookup, with_runner, LookupOutcome, ParseOutcome, Runner, BROKEN,
        TUPLE,
    };
    use crate::edit::validate_syntax_str;
    use std::sync::OnceLock;
    #[cfg(unix)]
    use {
        crate::edit::swift::{parse_outcome, run_command},
        std::process::Command,
        std::time::{Duration, Instant},
    };

    fn created_write_response(source: &str, runner: Runner) -> serde_json::Value {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("new.swift");
        assert_eq!(validate_syntax_str(source, &path), Some(false));
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                format_on_edit: false,
                ..Config::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "created-swift", "command": "write", "file": path, "content": source
        }))
        .unwrap();
        let response = with_runner(runner, || {
            crate::commands::write::handle_write(&request, &ctx)
        });
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["created"], true);
        assert_eq!(response.data["rolled_back"], false);
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
        if response.data.get("syntax_skipped_reason").is_some() {
            let text = crate::subc_format::format_response("write", &response, false);
            assert!(text.contains("Syntax check: unknown"), "{text}");
            assert!(!text.contains("new file does not parse"), "{text}");
        }
        response.data
    }

    #[test]
    fn swift_parser_created_timeout_is_unknown() {
        let response = created_write_response(TUPLE, Box::new(|_| ParseOutcome::TimedOut));
        assert!(response.get("syntax_valid").is_none(), "{response:?}");
        assert!(response.get("syntax_error_line").is_none(), "{response:?}");
        assert_eq!(response["syntax_skipped_reason"], "swiftc_timeout");
        assert!(
            response["warnings"].to_string().contains("unknown"),
            "{response:?}"
        );
    }

    #[test]
    fn swift_parser_created_unavailable_is_unknown() {
        for (outcome, reason) in [
            (
                ParseOutcome::Unavailable("permission denied".into()),
                "swiftc_unavailable",
            ),
            (ParseOutcome::Exited(None), "swiftc_terminated"),
            (ParseOutcome::Exited(Some(2)), "swiftc_failed"),
        ] {
            let mut outcome = Some(outcome);
            let response = created_write_response(
                TUPLE,
                Box::new(move |_| outcome.take().expect("one compiler confirmation")),
            );
            assert!(response.get("syntax_valid").is_none(), "{response:?}");
            assert!(response.get("syntax_error_line").is_none(), "{response:?}");
            assert_eq!(response["syntax_skipped_reason"], reason);
        }
    }

    #[test]
    fn swift_parser_created_missing_keeps_tree_sitter_verdict() {
        let response = created_write_response(TUPLE, Box::new(|_| ParseOutcome::Missing));
        assert_eq!(response["syntax_valid"], false, "{response:?}");
        assert_eq!(response["syntax_error_line"], 1);
        assert!(
            response.get("syntax_skipped_reason").is_none(),
            "{response:?}"
        );
    }

    #[test]
    fn swift_parser_lookup_timeout_retries_then_confirms_created_file() {
        let temp = tempfile::tempdir().unwrap();
        let compiler = temp.path().join("swiftc");
        let cache = std::rc::Rc::new(OnceLock::new());
        let lookups = std::rc::Rc::new(std::cell::Cell::new(0));
        let checks = std::rc::Rc::new(std::cell::Cell::new(0));
        for attempt in 0..3 {
            let cache = cache.clone();
            let lookups = lookups.clone();
            let checks = checks.clone();
            let compiler = compiler.clone();
            let response = created_write_response(
                TUPLE,
                Box::new(move |source| {
                    parse_with_lookup(
                        source,
                        || {
                            cached_lookup(&cache, || {
                                lookups.set(lookups.get() + 1);
                                if lookups.get() == 1 {
                                    LookupOutcome::TimedOut
                                } else {
                                    LookupOutcome::Found(compiler.clone())
                                }
                            })
                        },
                        |path, source| {
                            assert_eq!(path, compiler);
                            assert_eq!(source, TUPLE);
                            checks.set(checks.get() + 1);
                            ParseOutcome::Exited(Some(0))
                        },
                    )
                }),
            );
            if attempt == 0 {
                assert!(response.get("syntax_valid").is_none(), "{response:?}");
                assert!(response.get("syntax_error_line").is_none(), "{response:?}");
                assert_eq!(response["syntax_skipped_reason"], "swiftc_timeout");
            } else {
                assert_eq!(response["syntax_valid"], true, "{response:?}");
                assert!(response.get("syntax_error_line").is_none(), "{response:?}");
                assert!(
                    response.get("syntax_skipped_reason").is_none(),
                    "{response:?}"
                );
            }
        }
        assert_eq!(lookups.get(), 2, "retry once, then use the discovered path");
        assert_eq!(
            checks.get(),
            2,
            "a timed-out lookup must not run the compiler"
        );
    }

    #[test]
    fn swift_parser_created_answer_is_definite() {
        for (source, status, valid) in [(TUPLE, 0, true), (BROKEN, 1, false)] {
            let response = created_write_response(
                source,
                Box::new(move |actual| {
                    assert_eq!(actual, source);
                    ParseOutcome::Exited(Some(status))
                }),
            );
            assert_eq!(response["syntax_valid"], valid, "{response:?}");
            assert!(
                response.get("syntax_skipped_reason").is_none(),
                "{response:?}"
            );
            if valid {
                assert!(response.get("syntax_error_line").is_none(), "{response:?}");
            } else {
                assert_eq!(response["syntax_error_line"], 1);
            }
        }
    }

    #[test]
    fn swift_parser_created_append_and_patch_report_unknown() {
        for command in ["edit_match", "apply_patch"] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("new.swift");
            let ctx = crate::test_context(
                Box::new(crate::parser::TreeSitterProvider::new()),
                Config {
                    format_on_edit: false,
                    ..Config::default()
                },
            );
            let params = if command == "edit_match" {
                serde_json::json!({ "id": "append", "command": command, "file": path, "op": "append", "appendContent": TUPLE })
            } else {
                serde_json::json!({ "id": "patch", "command": command, "patch_text": format!("*** Begin Patch\n*** Add File: {}\n+{}*** End Patch\n", path.display(), TUPLE) })
            };
            let request = serde_json::from_value(params).unwrap();
            let response = with_runner(Box::new(|_| ParseOutcome::TimedOut), || {
                if command == "edit_match" {
                    crate::commands::edit_match::handle_edit_match(&request, &ctx)
                } else {
                    crate::commands::apply_patch::handle_apply_patch(&request, &ctx)
                }
            });
            assert!(response.success, "{response:?}");
            assert_eq!(std::fs::read_to_string(path).unwrap(), TUPLE);
            let data = if command == "edit_match" {
                &response.data
            } else {
                assert!(response.data["output"]
                    .as_str()
                    .unwrap()
                    .contains("Syntax check: unknown"));
                &response.data["metadata"]["files"][0]
            };
            assert!(
                data.get("syntax_valid")
                    .is_none_or(serde_json::Value::is_null),
                "{data:?}"
            );
            assert!(data.get("syntax_error_line").is_none(), "{data:?}");
            assert_eq!(data["syntax_skipped_reason"], "swiftc_timeout");
        }
    }

    #[cfg(unix)]
    #[test]
    fn swift_parser_created_slow_checker_is_unknown() {
        let response = created_write_response(
            TUPLE,
            Box::new(|source| {
                assert_eq!(source, TUPLE);
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "exec sleep 30"]);
                let outcome = parse_outcome(run_command(
                    command,
                    "swiftc",
                    source,
                    Instant::now() + Duration::from_millis(50),
                    false,
                ));
                assert!(matches!(outcome, ParseOutcome::TimedOut), "{outcome:?}");
                outcome
            }),
        );
        assert!(response.get("syntax_valid").is_none(), "{response:?}");
        assert!(response.get("syntax_error_line").is_none(), "{response:?}");
        assert_eq!(response["syntax_skipped_reason"], "swiftc_timeout");
    }
}
