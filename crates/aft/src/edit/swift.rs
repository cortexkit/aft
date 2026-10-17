//! Confirm Swift tree-sitter errors with the compiler's syntax-only parser.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::format::{FormatError, SubprocessOutcome};

const PARSE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(all(test, target_os = "macos"))]
const NATIVE_SWIFTC_TEST_TIMEOUT: Duration = Duration::from_secs(120);
static SWIFTC: OnceLock<Result<PathBuf, String>> = OnceLock::new();

#[cfg(all(test, target_os = "macos"))]
thread_local! {
    static PARSE_TIMEOUT_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

fn parse_timeout() -> Duration {
    #[cfg(all(test, target_os = "macos"))]
    if let Some(timeout) = PARSE_TIMEOUT_OVERRIDE.with(std::cell::Cell::get) {
        return timeout;
    }
    PARSE_TIMEOUT
}

#[cfg(all(test, target_os = "macos"))]
fn with_parse_timeout<T>(timeout: Duration, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Duration>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PARSE_TIMEOUT_OVERRIDE.with(|value| value.set(self.0));
        }
    }

    let previous = PARSE_TIMEOUT_OVERRIDE.with(|value| value.replace(Some(timeout)));
    let _restore = Restore(previous);
    f()
}

#[derive(Debug)]
enum ParseOutcome {
    Exited(Option<i32>),
    Missing,
    TimedOut,
    Unavailable(String),
}

pub(super) fn confirm_if_needed(
    path: &Path,
    source: &str,
    original: Option<&str>,
    was_valid: &mut Option<bool>,
    is_valid: Option<bool>,
) -> Option<bool> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("swift") || is_valid != Some(false) {
        return is_valid;
    }

    // Tree-sitter can also misread a previously accepted Swift file. Establish
    // a safe original before deciding whether the new content needs rollback.
    if *was_valid == Some(false)
        && original.is_some_and(|source| matches!(parse(source), ParseOutcome::Exited(Some(0))))
    {
        *was_valid = Some(true);
    }
    if *was_valid != Some(true) {
        return is_valid;
    }

    confirm_with_compiler(path, source, is_valid)
}

/// Confirm a tree-sitter error in a Swift file that the current call created.
///
/// There is no original to compare against, so the compiler's verdict on the
/// new content decides directly. When `swiftc` is unavailable or fails, the
/// tree-sitter verdict stands.
pub(super) fn confirm_created(path: &Path, source: &str, is_valid: Option<bool>) -> Option<bool> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("swift") || is_valid != Some(false) {
        return is_valid;
    }
    confirm_with_compiler(path, source, is_valid)
}

fn confirm_with_compiler(path: &Path, source: &str, is_valid: Option<bool>) -> Option<bool> {
    let outcome = parse(source);
    match outcome {
        ParseOutcome::Exited(Some(0)) => {
            log::debug!(
                "swiftc confirmed syntax after tree-sitter errors: {}",
                path.display()
            );
            Some(true)
        }
        ParseOutcome::Unavailable(reason) => {
            log::debug!(
                "Swift syntax confirmation unavailable for {}: {reason}",
                path.display()
            );
            is_valid
        }
        other => {
            log::debug!(
                "Swift syntax confirmation failed for {}: {other:?}",
                path.display()
            );
            is_valid
        }
    }
}

fn find_swiftc() -> &'static Result<PathBuf, String> {
    SWIFTC.get_or_init(|| {
        if let Ok(path) = which::which("swiftc") {
            return Ok(path);
        }
        #[cfg(target_os = "macos")]
        {
            let mut command = Command::new("xcrun");
            command.args(["--find", "swiftc"]);
            match run_command(command, "", Instant::now() + parse_timeout()) {
                Ok(result) if result.status.success() => {
                    let path = PathBuf::from(result.stdout.trim());
                    if path.is_absolute() && path.is_file() {
                        return Ok(path);
                    }
                    return Err("xcrun returned no swiftc path".into());
                }
                Ok(result) => return Err(format!("xcrun exited with {}", result.status)),
                Err(error) => return Err(format!("xcrun lookup failed: {error}")),
            }
        }
        #[cfg(not(target_os = "macos"))]
        Err("swiftc is not on PATH".into())
    })
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn available() -> bool {
    with_parse_timeout(NATIVE_SWIFTC_TEST_TIMEOUT, || {
        if let Err(reason) = find_swiftc() {
            eprintln!("swiftc discovery failed: {reason}");
            return false;
        }
        // Warm up the compiler under the same generous guard as the native tests.
        let outcome = parse("");
        assert!(
            matches!(outcome, ParseOutcome::Exited(Some(0))),
            "discovered swiftc failed to parse empty input: {outcome:?}"
        );
        true
    })
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn with_native_swiftc<T>(f: impl FnOnce() -> T) -> Option<T> {
    with_parse_timeout(NATIVE_SWIFTC_TEST_TIMEOUT, || {
        if available() {
            Some(f())
        } else {
            None
        }
    })
}

fn parse(source: &str) -> ParseOutcome {
    #[cfg(test)]
    if let Some(outcome) =
        RUNNER.with(|runner| runner.borrow_mut().as_mut().map(|runner| runner(source)))
    {
        return outcome;
    }
    let path = match find_swiftc() {
        Ok(path) => path,
        Err(reason) => {
            log::debug!("Swift syntax confirmation lookup failed: {reason}");
            return ParseOutcome::Missing;
        }
    };
    let mut command = Command::new(path);
    // Parsing stdin avoids temporary files. -parse performs no type checking or imports.
    command.args(["-parse", "-"]);
    parse_outcome(run_command(
        command,
        source,
        Instant::now() + parse_timeout(),
    ))
}

fn parse_outcome(result: Result<SubprocessOutcome, FormatError>) -> ParseOutcome {
    match result {
        Ok(result) => ParseOutcome::Exited(result.status.code()),
        Err(FormatError::NotFound { .. }) => ParseOutcome::Missing,
        Err(FormatError::Timeout { .. }) => ParseOutcome::TimedOut,
        Err(error) => ParseOutcome::Unavailable(error.to_string()),
    }
}

fn run_command(
    mut command: Command,
    source: &str,
    deadline: Instant,
) -> Result<SubprocessOutcome, FormatError> {
    // Do not set an environment or use pre_exec: both force fork on macOS.
    // process_group keeps posix_spawn available while allowing timeout tree cleanup.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FormatError::NotFound {
                    tool: "swiftc".into(),
                }
            } else {
                FormatError::Failed {
                    tool: "swiftc".into(),
                    stderr: error.to_string(),
                }
            }
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = source.as_bytes().to_vec();
    // A large file can fill stdin; start the deadline wait before waiting for its writer.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let result = crate::format::wait_with_deadline(child, "swiftc", deadline, || false);
    if result.is_ok() {
        writer
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("stdin writer panicked")))
            .map_err(|error| FormatError::Failed {
                tool: "swiftc".into(),
                stderr: error.to_string(),
            })?;
    }
    result
}

#[cfg(test)]
type Runner = Box<dyn FnMut(&str) -> ParseOutcome>;

#[cfg(test)]
thread_local! {
    static RUNNER: std::cell::RefCell<Option<Runner>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_runner<T>(runner: Runner, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Runner>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUNNER.with(|runner| *runner.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(RUNNER.with(|slot| slot.replace(Some(runner))));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::edit::{validate_syntax_str, write_format_validate, WriteResult};

    const ORIGINAL: &str = "let value = 1\n";
    const TUPLE: &str = "_ = (Int, Result<String, Error>).self\n";
    const BROKEN: &str = "_ = (Int, Result<String, Error>.self\n";

    fn write_case(source: &str, outcome: ParseOutcome) -> (WriteResult, String) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        std::fs::write(&path, ORIGINAL).unwrap();
        assert_eq!(validate_syntax_str(source, &path), Some(false));
        let expected = source.to_owned();
        let mut outcome = Some(outcome);
        let result = with_runner(
            Box::new(move |actual| {
                assert_eq!(actual, expected);
                outcome.take().expect("only one parser call per edit")
            }),
            || {
                write_format_validate(
                    &path,
                    source,
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap()
            },
        );
        (result, std::fs::read_to_string(&path).unwrap())
    }

    #[test]
    fn swift_parser_success_overrides_tree_sitter() {
        let (result, text) = write_case(TUPLE, ParseOutcome::Exited(Some(0)));
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));
        assert_eq!(text, TUPLE);
        assert!(result.reformatted_excerpt.is_none());
        assert!(result.validation_errors.is_empty());
    }

    #[test]
    fn swift_parser_two_edits_preserve_rollback() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = calls.clone();
        with_runner(
            Box::new(move |source| {
                recorded.borrow_mut().push(source.to_owned());
                match source {
                    TUPLE => ParseOutcome::Exited(Some(0)),
                    BROKEN => ParseOutcome::Exited(Some(1)),
                    _ => panic!("unexpected compiler input: {source}"),
                }
            }),
            || {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("tuple.swift");
                std::fs::write(&path, ORIGINAL).unwrap();
                let config = Config {
                    format_on_edit: false,
                    ..Config::default()
                };
                assert_eq!(validate_syntax_str(TUPLE, &path), Some(false));
                assert_eq!(validate_syntax_str(BROKEN, &path), Some(false));

                let first =
                    write_format_validate(&path, TUPLE, &config, &serde_json::json!({})).unwrap();
                assert!(!first.rolled_back);
                assert_eq!(first.syntax_valid, Some(true));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
                assert_eq!(*calls.borrow(), [TUPLE]);

                let mut second =
                    write_format_validate(&path, BROKEN, &config, &serde_json::json!({})).unwrap();
                assert!(second.rolled_back);
                assert_eq!(second.syntax_valid, Some(false));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
                assert_eq!(second.final_content(&path), Some(TUPLE));
                assert_eq!(*calls.borrow(), [TUPLE, TUPLE, BROKEN]);
            },
        );
    }

    #[test]
    fn swift_parser_invalid_original_keeps_edit_without_rollback() {
        for outcome in [
            ParseOutcome::Exited(Some(1)),
            ParseOutcome::Missing,
            ParseOutcome::TimedOut,
            ParseOutcome::Unavailable("permission denied".into()),
            ParseOutcome::Exited(None),
        ] {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let count = calls.clone();
            let mut outcome = Some(outcome);
            with_runner(
                Box::new(move |source| {
                    count.set(count.get() + 1);
                    assert_eq!(source, BROKEN, "only the original should be parsed");
                    outcome.take().expect("only one compiler call")
                }),
                || {
                    let temp = tempfile::tempdir().unwrap();
                    let path = temp.path().join("broken.swift");
                    std::fs::write(&path, BROKEN).unwrap();
                    let updated = BROKEN.repeat(2);
                    let result = write_format_validate(
                        &path,
                        &updated,
                        &Config {
                            format_on_edit: false,
                            ..Config::default()
                        },
                        &serde_json::json!({}),
                    )
                    .unwrap();
                    assert!(!result.rolled_back);
                    assert_eq!(result.syntax_valid, Some(false));
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
                    assert_eq!(calls.get(), 1);
                },
            );
        }
    }

    #[test]
    fn swift_parser_confirmed_original_uses_new_parse_result() {
        for (outcome, rolled_back) in [
            (ParseOutcome::Exited(Some(0)), false),
            (ParseOutcome::Exited(Some(1)), true),
            (ParseOutcome::Missing, true),
            (ParseOutcome::TimedOut, true),
            (ParseOutcome::Unavailable("permission denied".into()), true),
        ] {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let count = calls.clone();
            let updated = TUPLE.repeat(2);
            let expected = updated.clone();
            let mut outcome = Some(outcome);
            with_runner(
                Box::new(move |source| {
                    count.set(count.get() + 1);
                    match count.get() {
                        1 => {
                            assert_eq!(source, TUPLE);
                            ParseOutcome::Exited(Some(0))
                        }
                        2 => {
                            assert_eq!(source, expected);
                            outcome.take().unwrap()
                        }
                        _ => panic!("only two compiler calls"),
                    }
                }),
                || {
                    let temp = tempfile::tempdir().unwrap();
                    let path = temp.path().join("tuple.swift");
                    std::fs::write(&path, TUPLE).unwrap();
                    let result = write_format_validate(
                        &path,
                        &updated,
                        &Config {
                            format_on_edit: false,
                            ..Config::default()
                        },
                        &serde_json::json!({}),
                    )
                    .unwrap();
                    assert_eq!(result.rolled_back, rolled_back);
                    assert_eq!(result.syntax_valid, Some(!rolled_back));
                    assert_eq!(
                        std::fs::read_to_string(&path).unwrap(),
                        if rolled_back { TUPLE } else { &updated }
                    );
                    assert_eq!(calls.get(), 2);
                },
            );
        }
    }

    #[test]
    fn swift_parser_created_file_uses_compiler_verdict() {
        for (outcome, valid, line) in [
            (ParseOutcome::Exited(Some(0)), Some(true), None),
            (ParseOutcome::Exited(Some(1)), Some(false), Some(1)),
            (ParseOutcome::Missing, Some(false), Some(1)),
        ] {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0));
            let count = calls.clone();
            let mut outcome = Some(outcome);
            with_runner(
                Box::new(move |source| {
                    count.set(count.get() + 1);
                    assert_eq!(source, TUPLE);
                    outcome.take().expect("one compiler call per created file")
                }),
                || {
                    let temp = tempfile::tempdir().unwrap();
                    let path = temp.path().join("new.swift");
                    assert_eq!(validate_syntax_str(TUPLE, &path), Some(false));
                    let result = write_format_validate(
                        &path,
                        TUPLE,
                        &Config {
                            format_on_edit: false,
                            ..Config::default()
                        },
                        &serde_json::json!({}),
                    )
                    .unwrap();
                    assert!(!result.rolled_back);
                    assert_eq!(result.syntax_valid, valid);
                    assert_eq!(result.syntax_error_line, line);
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
                },
            );
            assert_eq!(calls.get(), 1);
        }
    }

    #[test]
    fn swift_parser_valid_to_valid_skips_compiler() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = calls.clone();
        with_runner(
            Box::new(move |_| {
                count.set(count.get() + 1);
                ParseOutcome::Exited(Some(0))
            }),
            || {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("valid.swift");
                std::fs::write(&path, ORIGINAL).unwrap();
                let updated = "let value = 2\n";
                assert_eq!(validate_syntax_str(updated, &path), Some(true));
                let result = write_format_validate(
                    &path,
                    updated,
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap();
                assert!(!result.rolled_back);
                assert_eq!(result.syntax_valid, Some(true));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
                assert_eq!(calls.get(), 0);
            },
        );
    }

    #[test]
    fn swift_parser_nonzero_rolls_back() {
        let (result, text) = write_case(BROKEN, ParseOutcome::Exited(Some(1)));
        assert!(result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(text, ORIGINAL);
    }

    #[test]
    fn swift_parser_missing_binary_rolls_back() {
        let (result, text) = write_case(TUPLE, ParseOutcome::Missing);
        assert!(result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(text, ORIGINAL);
    }

    #[test]
    fn swift_parser_unavailable_rolls_back() {
        for outcome in [
            ParseOutcome::TimedOut,
            ParseOutcome::Unavailable("permission denied".into()),
            ParseOutcome::Exited(None),
        ] {
            let (result, text) = write_case(TUPLE, outcome);
            assert!(result.rolled_back);
            assert_eq!(result.syntax_valid, Some(false));
            assert_eq!(text, ORIGINAL);
        }
    }

    #[test]
    fn swift_parser_never_runs_for_non_swift() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = calls.clone();
        with_runner(
            Box::new(move |_| {
                count.set(count.get() + 1);
                ParseOutcome::Exited(Some(0))
            }),
            || {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("bad.ts");
                std::fs::write(&path, "const value = 1;\n").unwrap();
                let result = write_format_validate(
                    &path,
                    "function {",
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap();
                assert_eq!(calls.get(), 0, "non-Swift edit must not spawn swiftc");
                assert!(result.rolled_back);
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "const value = 1;\n"
                );
            },
        );
    }

    #[test]
    fn swift_parser_skips_valid_new_content_and_unknown_original() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let count = calls.clone();
        with_runner(
            Box::new(move |_| {
                count.set(count.get() + 1);
                ParseOutcome::Exited(Some(0))
            }),
            || {
                for (was_valid, is_valid) in [
                    (Some(true), Some(true)),
                    (Some(false), Some(true)),
                    (None, Some(false)),
                    (Some(true), None),
                ] {
                    let mut original_valid = was_valid;
                    assert_eq!(
                        confirm_if_needed(
                            Path::new("test.swift"),
                            TUPLE,
                            Some(TUPLE),
                            &mut original_valid,
                            is_valid,
                        ),
                        is_valid
                    );
                    assert_eq!(original_valid, was_valid);
                }
            },
        );
        assert_eq!(
            calls.get(),
            0,
            "no parser call for valid new content or an unknown original"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_native_two_edits_preserve_rollback() {
        let Some(()) = with_native_swiftc(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("tuple.swift");
            std::fs::write(&path, ORIGINAL).unwrap();
            let config = Config {
                format_on_edit: false,
                ..Config::default()
            };
            let first =
                write_format_validate(&path, TUPLE, &config, &serde_json::json!({})).unwrap();
            assert!(!first.rolled_back);
            assert_eq!(first.syntax_valid, Some(true));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
            let second =
                write_format_validate(&path, BROKEN, &config, &serde_json::json!({})).unwrap();
            assert!(second.rolled_back);
            assert_eq!(second.syntax_valid, Some(false));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
        }) else {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        };
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn swift_parser_real_missing_binary_fallback() {
        if find_swiftc().is_ok() {
            eprintln!("skipping missing-swiftc test: swiftc is available on PATH");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        std::fs::write(&path, ORIGINAL).unwrap();
        let result = write_format_validate(
            &path,
            TUPLE,
            &Config {
                format_on_edit: false,
                ..Config::default()
            },
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_rejects_broken_swift() {
        let Some(()) = with_native_swiftc(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("broken.swift");
            std::fs::write(&path, ORIGINAL).unwrap();
            let result = write_format_validate(
                &path,
                BROKEN,
                &Config {
                    format_on_edit: false,
                    ..Config::default()
                },
                &serde_json::json!({}),
            )
            .unwrap();
            assert!(result.rolled_back);
            assert_eq!(result.syntax_valid, Some(false));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
        }) else {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        };
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_does_not_typecheck_or_load_imports() {
        let Some(()) = with_native_swiftc(|| {
            assert!(matches!(
                parse("import AFTModuleThatDoesNotExist\n_ = undefinedName\n"),
                ParseOutcome::Exited(Some(0))
            ));
        }) else {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        };
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_available_fails_for_discovered_broken_compiler() {
        with_parse_timeout(NATIVE_SWIFTC_TEST_TIMEOUT, || {
            if find_swiftc().is_err() {
                eprintln!("skipping native Swift parser test: swiftc is unavailable");
                return;
            }
            for outcome in [ParseOutcome::Exited(Some(1)), ParseOutcome::Missing] {
                let mut outcome = Some(outcome);
                let failure = std::panic::catch_unwind(|| {
                    with_runner(
                        Box::new(move |source| {
                            assert!(source.is_empty());
                            outcome.take().expect("only one availability parse")
                        }),
                        available,
                    )
                });
                let failure =
                    failure.expect_err("a discovered broken compiler must fail, not skip");
                let message = failure
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| failure.downcast_ref::<&str>().copied())
                    .expect("panic should explain the broken compiler");
                assert!(message.contains("discovered swiftc failed to parse empty input"));
            }
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_test_deadline_override_covers_slow_first_call() {
        with_parse_timeout(NATIVE_SWIFTC_TEST_TIMEOUT, || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "sleep 6"]);
            let started = Instant::now();
            let outcome = parse_outcome(run_command(command, "", Instant::now() + parse_timeout()));
            assert!(
                matches!(outcome, ParseOutcome::Exited(Some(0))),
                "test-only deadline should let a slow compiler startup finish: {outcome:?}"
            );
            assert!(started.elapsed() >= Duration::from_secs(5));
        });
    }

    #[test]
    fn swift_parser_spawn_missing_binary_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let command = Command::new(temp.path().join("absent-swiftc"));
        assert!(matches!(
            parse_outcome(run_command(command, TUPLE, Instant::now() + PARSE_TIMEOUT)),
            ParseOutcome::Missing
        ));
    }

    #[cfg(unix)]
    #[test]
    fn swift_parser_timeout_rolls_back_and_reaps_child() {
        struct Cleanup(i32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // Also clean up if an assertion exposes a timeout-cleanup regression.
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                    libc::waitpid(self.0, std::ptr::null_mut(), 0);
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        let pid_path = temp.path().join("child.pid");
        std::fs::write(&path, ORIGINAL).unwrap();
        let child_pid_path = pid_path.clone();
        let started = Instant::now();
        let result = with_runner(
            Box::new(move |source| {
                let mut command = Command::new("/bin/sh");
                command
                    .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "swiftc-test"])
                    .arg(&child_pid_path);
                let outcome = parse_outcome(run_command(
                    command,
                    source,
                    Instant::now() + Duration::from_millis(150),
                ));
                assert!(matches!(outcome, ParseOutcome::TimedOut));
                outcome
            }),
            || {
                write_format_validate(
                    &path,
                    &TUPLE.repeat(4_096),
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap()
            },
        );
        // Read the PID published by the real child so the liveness check targets that process.
        let pid_deadline = Instant::now() + Duration::from_secs(2);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(
                Instant::now() < pid_deadline,
                "child did not publish its PID"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let _cleanup = Cleanup(pid);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "deadline must bound blocked stdin too"
        );
        assert!(result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "timed-out child is still alive or unreaped"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
