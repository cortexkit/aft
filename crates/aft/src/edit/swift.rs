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
static SWIFTC: OnceLock<LookupOutcome> = OnceLock::new();

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

#[derive(Clone, Debug, PartialEq, Eq)]
enum LookupOutcome {
    Found(PathBuf),
    Missing,
    #[cfg(any(test, target_os = "macos"))]
    TimedOut,
    Unavailable(String),
}

pub(super) fn confirm_if_needed(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
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
    // a safe original for the rollback decision. The compiler's verdict on the
    // new content applies whatever the original was, so an edit that repairs a
    // broken file into valid Swift tree-sitter misreads is reported valid.
    if *was_valid == Some(false)
        && original.is_some_and(|source| {
            matches!(parse(tool_policy, source), ParseOutcome::Exited(Some(0)))
        })
    {
        *was_valid = Some(true);
    }

    confirm_with_compiler(tool_policy, path, source, is_valid)
}

/// Confirm a tree-sitter error in a Swift file that the current call created.
///
/// There is no original to compare against, so the compiler's verdict on the
/// new content decides directly. Without an installed compiler, tree-sitter
/// remains the authority. An unfinished confirmation is unknown, not evidence
/// that the new file contains a syntax error.
pub(super) fn confirm_created(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    path: &Path,
    source: &str,
    is_valid: Option<bool>,
) -> Result<Option<bool>, &'static str> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("swift") || is_valid != Some(false) {
        return Ok(is_valid);
    }
    compiler_verdict(tool_policy, path, source).map(|verdict| verdict.or(is_valid))
}

fn confirm_with_compiler(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    path: &Path,
    source: &str,
    is_valid: Option<bool>,
) -> Option<bool> {
    // Existing files keep the conservative rollback policy when confirmation
    // cannot finish; newly created files have no original content to protect.
    compiler_verdict(tool_policy, path, source)
        .unwrap_or(None)
        .or(is_valid)
}

fn compiler_verdict(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    path: &Path,
    source: &str,
) -> Result<Option<bool>, &'static str> {
    let outcome = parse(tool_policy, source);
    log::debug!(
        "Swift syntax confirmation for {}: {outcome:?}",
        path.display()
    );
    match outcome {
        ParseOutcome::Exited(Some(0)) => Ok(Some(true)),
        ParseOutcome::Exited(Some(1)) => Ok(Some(false)),
        ParseOutcome::TimedOut => Err("swiftc_timeout"),
        ParseOutcome::Missing => Ok(None),
        ParseOutcome::Unavailable(reason) => {
            log::debug!("Swift syntax confirmation unavailable: {reason}");
            Err("swiftc_unavailable")
        }
        ParseOutcome::Exited(None) => Err("swiftc_terminated"),
        ParseOutcome::Exited(Some(_)) => Err("swiftc_failed"),
    }
}

fn cached_lookup(
    cache: &OnceLock<LookupOutcome>,
    lookup: impl FnOnce() -> LookupOutcome,
) -> LookupOutcome {
    if let Some(outcome) = cache.get() {
        return outcome.clone();
    }
    let outcome = lookup();
    // A slow or failed lookup does not establish whether Swift is installed.
    // Leave discovery retryable instead of poisoning the daemon's cache.
    if matches!(outcome, LookupOutcome::Found(_) | LookupOutcome::Missing) {
        cache.get_or_init(|| outcome).clone()
    } else {
        outcome
    }
}

fn find_swiftc() -> LookupOutcome {
    cached_lookup(&SWIFTC, discover_swiftc)
}

fn discover_swiftc() -> LookupOutcome {
    match which::which("swiftc") {
        Ok(path) if path.is_absolute() => return LookupOutcome::Found(path),
        Ok(_) => {
            return LookupOutcome::Unavailable("swiftc lookup returned a relative path".into())
        }
        Err(which::Error::CannotFindBinaryPath) => {}
        Err(error) => return LookupOutcome::Unavailable(error.to_string()),
    }
    #[cfg(target_os = "macos")]
    {
        // Discovery is operator-only and cached across sessions, unlike each
        // source parse. Do not attach one session's launch policy to this probe.
        let mut command = Command::new("xcrun");
        command.args(["--find", "swiftc"]);
        xcrun_lookup_outcome(run_command(
            command,
            "swiftc",
            "",
            Instant::now() + parse_timeout(),
            false,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    LookupOutcome::Missing
}

#[cfg(any(test, target_os = "macos"))]
fn xcrun_lookup_outcome(result: Result<SubprocessOutcome, FormatError>) -> LookupOutcome {
    match result {
        Ok(result) => match result.status.code() {
            Some(0) if result.truncated => {
                LookupOutcome::Unavailable("xcrun output was truncated".into())
            }
            Some(0) => {
                let output = result.stdout.trim();
                if output.is_empty() {
                    LookupOutcome::Missing
                } else {
                    let path = PathBuf::from(output);
                    if path.is_absolute() {
                        LookupOutcome::Found(path)
                    } else {
                        LookupOutcome::Unavailable("xcrun returned a relative swiftc path".into())
                    }
                }
            }
            Some(_) => LookupOutcome::Missing,
            None => LookupOutcome::Unavailable("xcrun was terminated".into()),
        },
        Err(FormatError::Timeout { .. }) => LookupOutcome::TimedOut,
        Err(error) => LookupOutcome::Unavailable(format!("xcrun lookup failed: {error}")),
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn available() -> bool {
    with_parse_timeout(NATIVE_SWIFTC_TEST_TIMEOUT, || {
        let lookup = find_swiftc();
        if !matches!(lookup, LookupOutcome::Found(_)) {
            eprintln!("swiftc discovery failed: {lookup:?}");
            return false;
        }
        // Warm up the compiler under the same generous guard as the native tests.
        let outcome = parse(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            "",
        );
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

fn parse(tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy, source: &str) -> ParseOutcome {
    #[cfg(test)]
    if let Some(outcome) =
        RUNNER.with(|runner| runner.borrow_mut().as_mut().map(|runner| runner(source)))
    {
        return outcome;
    }
    parse_with_lookup(source, find_swiftc, |path, source| {
        run_swiftc(tool_policy, path, source)
    })
}

fn parse_with_lookup(
    source: &str,
    lookup: impl FnOnce() -> LookupOutcome,
    checker: impl FnOnce(&Path, &str) -> ParseOutcome,
) -> ParseOutcome {
    match lookup() {
        LookupOutcome::Found(path) => checker(&path, source),
        LookupOutcome::Missing => ParseOutcome::Missing,
        #[cfg(any(test, target_os = "macos"))]
        LookupOutcome::TimedOut => ParseOutcome::TimedOut,
        LookupOutcome::Unavailable(reason) => ParseOutcome::Unavailable(reason),
    }
}

fn run_swiftc(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    path: &Path,
    source: &str,
) -> ParseOutcome {
    // Parsing stdin avoids temporary files. -parse performs no type checking or imports.
    let args = ["-parse", "-"].map(std::ffi::OsString::from);
    let command = match tool_policy.command(path.as_os_str(), &args) {
        Ok(command) => command,
        Err(reason) => return ParseOutcome::Unavailable(reason),
    };
    parse_outcome(run_command(
        command,
        "swiftc",
        source,
        Instant::now() + parse_timeout(),
        false,
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

/// Run a prepared tool command that reads `source` on stdin, killing its
/// process tree at `deadline`. Syntax-confirmation callers supply commands
/// prepared under the request policy; operator-only compiler discovery does not.
///
/// With `may_stop_reading`, a checker that exits before reading all of its
/// input (bash stops at the first syntax error) is judged by its exit status;
/// otherwise the failed write makes the run fail.
pub(super) fn run_command(
    mut command: Command,
    tool: &str,
    source: &str,
    deadline: Instant,
    may_stop_reading: bool,
) -> Result<SubprocessOutcome, FormatError> {
    // Avoid setting an environment or using pre_exec where possible: both
    // force fork on macOS. process_group keeps posix_spawn available while
    // allowing timeout tree cleanup.
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
                FormatError::NotFound { tool: tool.into() }
            } else {
                FormatError::Failed {
                    tool: tool.into(),
                    stderr: error.to_string(),
                }
            }
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = source.as_bytes().to_vec();
    // A large file can fill stdin; start the deadline wait before waiting for its writer.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let result = crate::format::wait_with_deadline(child, tool, deadline, || false);
    if result.is_ok() {
        match writer
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("stdin writer panicked")))
        {
            Ok(()) => {}
            Err(error) if may_stop_reading && error.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(error) => {
                return Err(FormatError::Failed {
                    tool: tool.into(),
                    stderr: error.to_string(),
                })
            }
        }
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

    #[cfg(unix)]
    #[test]
    fn swiftc_parse_cannot_bypass_the_supplied_tool_policy() {
        use crate::sandbox_spawn::{SpawnPlan, ToolLaunchPolicy};
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
        let outcome = run_swiftc(&policy, &compiler, TUPLE);
        assert!(
            matches!(&outcome, ParseOutcome::Unavailable(reason) if reason.starts_with("sandbox_unavailable")),
            "{outcome:?}"
        );
        assert!(
            !marker.exists(),
            "a refused syntax checker must not run on the host"
        );
    }

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
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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

                let first = write_format_validate(
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                    &path,
                    TUPLE,
                    &config,
                    &serde_json::json!({}),
                )
                .unwrap();
                assert!(!first.rolled_back);
                assert_eq!(first.syntax_valid, Some(true));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
                assert_eq!(*calls.borrow(), [TUPLE]);

                let mut second = write_format_validate(
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                    &path,
                    BROKEN,
                    &config,
                    &serde_json::json!({}),
                )
                .unwrap();
                assert!(second.rolled_back);
                assert_eq!(second.syntax_valid, Some(false));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
                assert_eq!(second.final_content(&path), Some(TUPLE));
                assert_eq!(*calls.borrow(), [TUPLE, TUPLE, BROKEN]);
            },
        );
    }

    /// An original the compiler does not confirm is never rolled back to, so
    /// the edit is kept whatever the original's outcome. The new content is
    /// still judged by the compiler; here it confirms the error.
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
            let updated = BROKEN.repeat(2);
            let expected = updated.clone();
            let mut outcome = Some(outcome);
            with_runner(
                Box::new(move |source| {
                    count.set(count.get() + 1);
                    match count.get() {
                        1 => {
                            assert_eq!(source, BROKEN, "the original is checked first");
                            outcome.take().unwrap()
                        }
                        2 => {
                            assert_eq!(source, expected, "then the new content");
                            ParseOutcome::Exited(Some(1))
                        }
                        _ => panic!("only two compiler calls"),
                    }
                }),
                || {
                    let temp = tempfile::tempdir().unwrap();
                    let path = temp.path().join("broken.swift");
                    std::fs::write(&path, BROKEN).unwrap();
                    let result = write_format_validate(
                        &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
                    assert_eq!(calls.get(), 2);
                },
            );
        }
    }

    /// An edit that repairs a file the compiler rejects into valid Swift that
    /// tree-sitter misreads is judged by the compiler, even though the
    /// original is broken.
    #[test]
    fn swift_parser_broken_original_repaired_uses_compiler_verdict() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = calls.clone();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("broken.swift");
        std::fs::write(&path, BROKEN).unwrap();
        assert_eq!(validate_syntax_str(TUPLE, &path), Some(false));
        let result = with_runner(
            Box::new(move |source| {
                recorded.borrow_mut().push(source.to_owned());
                match source {
                    BROKEN => ParseOutcome::Exited(Some(1)),
                    TUPLE => ParseOutcome::Exited(Some(0)),
                    _ => panic!("unexpected compiler input: {source}"),
                }
            }),
            || {
                write_format_validate(
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                    &path,
                    TUPLE,
                    &Config {
                        format_on_edit: false,
                        ..Config::default()
                    },
                    &serde_json::json!({}),
                )
                .unwrap()
            },
        );
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
        assert_eq!(*calls.borrow(), [BROKEN, TUPLE]);
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
                        &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
                        &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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

    fn created_write_response(source: &str, runner: Runner) -> serde_json::Value {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("new.swift");
        assert_eq!(validate_syntax_str(source, &path), Some(false));
        let ctx = crate::context::AppContext::new(
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
    fn swift_parser_lookup_caches_definitive_answers() {
        let temp = tempfile::tempdir().unwrap();
        for outcome in [
            LookupOutcome::Found(temp.path().join("swiftc")),
            LookupOutcome::Missing,
        ] {
            let cache = OnceLock::new();
            assert_eq!(cached_lookup(&cache, || outcome.clone()), outcome);
            assert_eq!(
                cached_lookup(&cache, || panic!("definitive answers are cached")),
                outcome
            );
        }
    }

    #[test]
    fn swift_parser_lookup_unavailable_is_retryable() {
        let cache = OnceLock::new();
        let outcome = cached_lookup(&cache, || LookupOutcome::Unavailable("spawn denied".into()));
        assert!(matches!(outcome, LookupOutcome::Unavailable(_)));
        assert!(
            cache.get().is_none(),
            "a transient error must not poison discovery"
        );
        assert_eq!(
            cached_lookup(&cache, || LookupOutcome::Missing),
            LookupOutcome::Missing
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn swift_parser_xcrun_lookup_distinguishes_definitive_answers() {
        let output = |code, stdout: String, truncated| {
            #[cfg(unix)]
            let status = {
                use std::os::unix::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(code << 8)
            };
            #[cfg(windows)]
            let status = {
                use std::os::windows::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(code as u32)
            };
            Ok(SubprocessOutcome {
                stdout,
                stderr: String::new(),
                status,
                truncated,
            })
        };
        let temp = tempfile::tempdir().unwrap();
        let compiler = temp.path().join("swiftc");
        assert_eq!(
            xcrun_lookup_outcome(output(0, compiler.display().to_string(), false)),
            LookupOutcome::Found(compiler)
        );
        assert_eq!(
            xcrun_lookup_outcome(output(0, " \n".into(), false)),
            LookupOutcome::Missing
        );
        assert_eq!(
            xcrun_lookup_outcome(output(1, String::new(), false)),
            LookupOutcome::Missing
        );
        assert!(matches!(
            xcrun_lookup_outcome(output(0, "relative/swiftc".into(), false)),
            LookupOutcome::Unavailable(_)
        ));
        assert!(matches!(
            xcrun_lookup_outcome(output(0, String::new(), true)),
            LookupOutcome::Unavailable(_)
        ));
        assert_eq!(
            xcrun_lookup_outcome(Err(FormatError::Timeout {
                tool: "xcrun".into(),
                timeout_secs: 5
            })),
            LookupOutcome::TimedOut
        );
        for error in [
            FormatError::NotFound {
                tool: "xcrun".into(),
            },
            FormatError::Failed {
                tool: "xcrun".into(),
                stderr: "spawn denied".into(),
            },
        ] {
            assert!(matches!(
                xcrun_lookup_outcome(Err(error)),
                LookupOutcome::Unavailable(_)
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert!(matches!(
                xcrun_lookup_outcome(Ok(SubprocessOutcome {
                    stdout: String::new(),
                    stderr: String::new(),
                    status: std::process::ExitStatus::from_raw(libc::SIGKILL),
                    truncated: false,
                })),
                LookupOutcome::Unavailable(_)
            ));
        }
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
            let ctx = crate::context::AppContext::new(
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
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
    fn swift_parser_skips_valid_new_content() {
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
                    (Some(true), None),
                ] {
                    let mut original_valid = was_valid;
                    assert_eq!(
                        confirm_if_needed(
                            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
            "no parser call for new content tree-sitter accepts or cannot judge"
        );
    }

    /// With an unknown original, the compiler still judges rejected new
    /// content; the original stays unknown, so nothing can be rolled back.
    #[test]
    fn swift_parser_unknown_original_still_confirms_new_content() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = calls.clone();
        with_runner(
            Box::new(move |source| {
                recorded.borrow_mut().push(source.to_owned());
                ParseOutcome::Exited(Some(0))
            }),
            || {
                let mut original_valid = None;
                assert_eq!(
                    confirm_if_needed(
                        &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                        Path::new("test.swift"),
                        TUPLE,
                        Some(TUPLE),
                        &mut original_valid,
                        Some(false),
                    ),
                    Some(true)
                );
                assert_eq!(original_valid, None);
            },
        );
        assert_eq!(*calls.borrow(), [TUPLE]);
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
            let first = write_format_validate(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                &path,
                TUPLE,
                &config,
                &serde_json::json!({}),
            )
            .unwrap();
            assert!(!first.rolled_back);
            assert_eq!(first.syntax_valid, Some(true));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
            let second = write_format_validate(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                &path,
                BROKEN,
                &config,
                &serde_json::json!({}),
            )
            .unwrap();
            assert!(second.rolled_back);
            assert_eq!(second.syntax_valid, Some(false));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), TUPLE);
        }) else {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        };
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn swift_parser_native_created_file_is_valid() {
        let Some(()) = with_native_swiftc(|| {
            let check = crate::edit::check_created_file_syntax(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                Path::new("new.swift"),
                TUPLE,
            );
            assert_eq!(check.valid, Some(true), "{check:?}");
            assert_eq!(check.first_error_line, None);
            assert_eq!(check.skipped_reason, None);
        }) else {
            eprintln!("skipping native Swift parser test: swiftc is unavailable");
            return;
        };
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn swift_parser_real_missing_binary_fallback() {
        if matches!(find_swiftc(), LookupOutcome::Found(_)) {
            eprintln!("skipping missing-swiftc test: swiftc is available on PATH");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tuple.swift");
        std::fs::write(&path, ORIGINAL).unwrap();
        let result = write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
                parse(
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                    "import AFTModuleThatDoesNotExist\n_ = undefinedName\n"
                ),
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
            if !matches!(find_swiftc(), LookupOutcome::Found(_)) {
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
            let outcome = parse_outcome(run_command(
                command,
                "swiftc",
                "",
                Instant::now() + parse_timeout(),
                false,
            ));
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
            parse_outcome(run_command(
                command,
                "swiftc",
                TUPLE,
                Instant::now() + PARSE_TIMEOUT,
                false
            )),
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
                    "swiftc",
                    source,
                    Instant::now() + Duration::from_millis(150),
                    false,
                ));
                assert!(matches!(outcome, ParseOutcome::TimedOut));
                outcome
            }),
            || {
                write_format_validate(
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
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
