//! Confirm bash tree-sitter errors with the shell's own syntax-only check.
//!
//! tree-sitter-bash rejects some scripts that bash itself accepts; an escaped
//! string comparison such as `[ "$a" \< "$b" ]` is one. A syntax check must
//! not tell the agent a valid script is broken, so when tree-sitter rejects a
//! new bash file, or an edit to a bash file that was valid before, `bash -n`
//! decides. It parses without running anything. When the shell cannot give a
//! verdict on a new file, the result is unknown rather than broken.

use std::path::{Path, PathBuf};
#[cfg(all(test, unix))]
use std::process::Command;
use std::time::{Duration, Instant};

use crate::format::{FormatError, SubprocessOutcome};
use crate::parser::{detect_language, LangId};

const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// `bash -n` exit status for a syntax error.
const SYNTAX_ERROR_STATUS: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
enum CheckOutcome {
    Exited { code: Option<i32>, stderr: String },
    Missing,
    TimedOut,
    Unavailable(String),
}

/// The shell's verdict on a source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShellVerdict {
    Valid,
    /// A syntax error, with the line bash named for it when its message
    /// carried one.
    Broken(Option<usize>),
}

/// Files parsed with the bash grammar.
pub(super) fn applies(path: &Path) -> bool {
    detect_language(path) == Some(LangId::Bash)
}

/// Confirm a tree-sitter rejection of an existing file's new content. The
/// shell's verdict on the new content is the result whatever the original
/// was, so an edit that repairs a broken script into valid bash the grammar
/// cannot read is reported valid. The original only feeds the rollback
/// decision: when tree-sitter also rejected it, the shell is asked about it,
/// so a valid script tree-sitter misreads is still protected from a broken
/// edit. When the shell cannot answer about the new content,
/// tree-sitter's verdict stands, which keeps the conservative rollback for a
/// file that was valid before.
pub(super) fn confirm_if_needed(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    source: &str,
    original: Option<&str>,
    was_valid: &mut Option<bool>,
    is_valid: Option<bool>,
) -> Option<bool> {
    if is_valid != Some(false) {
        return is_valid;
    }
    if *was_valid == Some(false)
        && original.is_some_and(|source| verdict(tool_policy, source) == Ok(ShellVerdict::Valid))
    {
        *was_valid = Some(true);
    }
    match verdict(tool_policy, source) {
        Ok(ShellVerdict::Valid) => Some(true),
        Ok(ShellVerdict::Broken(_)) => Some(false),
        Err(_) => is_valid,
    }
}

/// Confirm a tree-sitter rejection of a file the current call created. The
/// error is a reason token for an unknown verdict: `bash_unavailable` (no
/// usable bash, which is always the case on Windows), `bash_timeout`, or
/// `bash_failed` (bash ended without a syntax verdict).
pub(super) fn confirm_created(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    source: &str,
) -> Result<ShellVerdict, &'static str> {
    verdict(tool_policy, source)
}

fn verdict(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    source: &str,
) -> Result<ShellVerdict, &'static str> {
    let outcome = check(tool_policy, source);
    log::debug!("bash syntax confirmation: {outcome:?}");
    match outcome {
        CheckOutcome::Exited { code: Some(0), .. } => Ok(ShellVerdict::Valid),
        CheckOutcome::Exited {
            code: Some(SYNTAX_ERROR_STATUS),
            stderr,
        } => Ok(ShellVerdict::Broken(reported_line(&stderr))),
        CheckOutcome::Exited { .. } => Err("bash_failed"),
        CheckOutcome::TimedOut => Err("bash_timeout"),
        CheckOutcome::Missing => Err("bash_unavailable"),
        CheckOutcome::Unavailable(reason) => {
            log::debug!("bash syntax confirmation unavailable: {reason}");
            Err("bash_unavailable")
        }
    }
}

/// The first line number in a `bash: line N: ...` message.
fn reported_line(stderr: &str) -> Option<usize> {
    stderr.match_indices("line ").find_map(|(start, marker)| {
        let rest = &stderr[start + marker.len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 || !rest[digits..].starts_with(':') {
            return None;
        }
        rest[..digits].parse().ok().filter(|line| *line > 0)
    })
}

fn check(tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy, source: &str) -> CheckOutcome {
    #[cfg(test)]
    if let Some(outcome) =
        RUNNER.with(|runner| runner.borrow_mut().as_mut().map(|runner| runner(source)))
    {
        return outcome;
    }
    match find_bash() {
        Ok(Some(bash)) => run_bash(tool_policy, &bash, source, Instant::now() + CHECK_TIMEOUT),
        Ok(None) => CheckOutcome::Missing,
        Err(reason) => CheckOutcome::Unavailable(reason),
    }
}

/// An absolute path to bash on `PATH`. Never on Windows: the `bash.exe` found
/// there is usually the WSL launcher, which starts a Linux environment
/// rather than checking anything locally.
fn find_bash() -> Result<Option<PathBuf>, String> {
    if cfg!(windows) {
        return Ok(None);
    }
    match which::which("bash") {
        Ok(path) if path.is_absolute() => Ok(Some(path)),
        Ok(_) => Err("bash lookup returned a relative path".into()),
        Err(which::Error::CannotFindBinaryPath) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Run `bash -n` over `source` on stdin. The file path is never passed, and
/// inherited startup hooks are cleared before the request's policy environment
/// is installed; `--norc --noprofile` rules out user startup files too.
fn run_bash(
    tool_policy: &crate::sandbox_spawn::ToolLaunchPolicy,
    bash: &Path,
    source: &str,
    deadline: Instant,
) -> CheckOutcome {
    let args = ["--norc", "--noprofile", "-n"].map(std::ffi::OsString::from);
    let command = match tool_policy.command_with_clean_environment(bash.as_os_str(), &args) {
        Ok(command) => command,
        Err(reason) => return CheckOutcome::Unavailable(reason),
    };
    check_outcome(super::swift::run_command(
        command, "bash", source, deadline, true,
    ))
}

fn check_outcome(result: Result<SubprocessOutcome, FormatError>) -> CheckOutcome {
    match result {
        Ok(result) => CheckOutcome::Exited {
            code: result.status.code(),
            stderr: result.stderr,
        },
        Err(FormatError::NotFound { .. }) => CheckOutcome::Missing,
        Err(FormatError::Timeout { .. }) => CheckOutcome::TimedOut,
        Err(error) => CheckOutcome::Unavailable(error.to_string()),
    }
}

#[cfg(test)]
type Runner = Box<dyn FnMut(&str) -> CheckOutcome>;

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
    use crate::edit::{check_created_file_syntax, validate_syntax_str};

    /// A real script that `bash -n` accepts and tree-sitter-bash rejects.
    const FIXTURE: &str = include_str!("../../tests/fixtures/aft-window-6deceda4.sh");
    /// Truly broken: the second `case` item lacks its `)`; bash names line 5.
    const BROKEN_CASE: &str = "set -u\necho a\ncase x in\n  a) echo ;;\n  b echo ;;\nesac\n";
    /// Valid bash that tree-sitter-bash rejects (escaped comparison).
    #[cfg(unix)]
    const GRAMMAR_GAP: &str = "x=1\nif [ \"$x\" \\< 2 ]; then echo lt; fi\n";
    /// The grammar gap on line 1 and a real error, a stray `fi`, on line 3.
    #[cfg(unix)]
    const GAP_THEN_BROKEN: &str = "[ a \\< b ] && echo lt\necho ok\nfi\n";

    fn config() -> Config {
        Config {
            format_on_edit: false,
            ..Config::default()
        }
    }

    #[cfg(unix)]
    fn require_bash() {
        assert!(
            matches!(find_bash(), Ok(Some(_))),
            "these tests need bash on PATH: {:?}",
            find_bash()
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_syntax_confirmation_launches_under_session_tool_policy() {
        use crate::sandbox_spawn::{SpawnPlan, ToolLaunchPolicy};
        require_bash();
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let temp = root.path().join("private-temp");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&temp).unwrap();
        let argv_log = root.path().join("argv");
        let env_log = root.path().join("environment");
        let launcher = root.path().join("launcher");
        crate::executable_cache::write_executable(&launcher, format!(
            "#!/bin/sh\nif [ \"$1\" = --warm-up ]; then exit 0; fi\nprintf '%s\\n' \"$@\" > '{}'\nenv > '{}'\nshift 4\nexec \"$@\"\n",
            argv_log.display(), env_log.display()
        ).as_bytes()).unwrap();
        assert!(Command::new(&launcher)
            .arg("--warm-up")
            .env_clear()
            .env("PATH", crate::effective_path::effective_path())
            .status()
            .unwrap()
            .success());
        let profile = crate::sandbox_profile::SandboxProfile::build(
            vec![project.clone()],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            temp.clone(),
        )
        .unwrap();
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(project.clone()),
                storage_dir: Some(root.path().join("store")),
                ..config()
            },
        );
        let policy = crate::sandbox_spawn::with_spawn_plan_for_test(
            SpawnPlan::launcher_for_test(profile.clone(), launcher),
            || ToolLaunchPolicy::resolve(&ctx),
        )
        .shared();
        let path = project.join("script.sh");
        std::fs::write(&path, "echo old\n").unwrap();
        assert_eq!(validate_syntax_str(GRAMMAR_GAP, &path), Some(false));
        let request = serde_json::from_value(serde_json::json!({"id":"syntax-policy", "command":"write", "file":path, "content":GRAMMAR_GAP})).unwrap();
        let response = crate::log_ctx::with_session(Some("syntax-policy".into()), || {
            ToolLaunchPolicy::with_current(Some(policy), || {
                crate::commands::write::handle_write(&request, &ctx)
            })
        });
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["syntax_valid"], true, "{response:?}");
        let args =
            std::fs::read_to_string(argv_log).expect("bash -n must use the session's launcher");
        let args = args.lines().collect::<Vec<_>>();
        assert_eq!(&args[..2], &["sandbox-launch", "--profile-json"]);
        let launched: crate::sandbox_profile::SandboxProfile =
            serde_json::from_str(args[2]).unwrap();
        assert_eq!(launched, profile);
        assert_eq!(args[3], "--");
        assert_eq!(args[4], find_bash().unwrap().unwrap().to_str().unwrap());
        assert_eq!(&args[5..], &["--norc", "--noprofile", "-n"]);
        let environment = std::fs::read_to_string(env_log).unwrap();
        assert!(
            environment
                .lines()
                .any(|line| line == format!("TMPDIR={}", temp.canonicalize().unwrap().display())),
            "{environment}"
        );
        assert!(
            !environment
                .lines()
                .any(|line| line.starts_with("CARGO_MANIFEST_DIR=")),
            "{environment}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_syntax_confirmation_never_falls_back_after_session_policy_refusal() {
        use crate::sandbox_spawn::{SpawnPlan, ToolLaunchPolicy};
        require_bash();
        let root = tempfile::tempdir().unwrap();
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..config()
            },
        );
        let policy = crate::sandbox_spawn::with_spawn_plan_for_test(
            SpawnPlan::refused_for_test("sandbox_unavailable"),
            || ToolLaunchPolicy::resolve(&ctx),
        )
        .shared();
        let path = root.path().join("new.sh");
        assert_eq!(validate_syntax_str(GRAMMAR_GAP, &path), Some(false));
        let request = serde_json::from_value(serde_json::json!({"id":"syntax-refused", "command":"write", "file":path, "content":GRAMMAR_GAP})).unwrap();
        let response = ToolLaunchPolicy::with_current(Some(policy), || {
            crate::commands::write::handle_write(&request, &ctx)
        });
        assert!(response.success, "{response:?}");
        assert!(response.data.get("syntax_valid").is_none(), "{response:?}");
        assert_eq!(
            response.data["syntax_skipped_reason"], "bash_unavailable",
            "{response:?}"
        );
        assert!(
            response.data.get("syntax_error_line").is_none(),
            "{response:?}"
        );
    }

    #[test]
    fn reported_line_reads_the_first_bash_line_number() {
        assert_eq!(
            reported_line(
                "bash: line 5: syntax error near unexpected token `echo'\nbash: line 5: `  b echo ;;'\n"
            ),
            Some(5)
        );
        assert_eq!(
            reported_line(
                "bash: line 2: unexpected EOF while looking for matching `)'\nbash: line 4: syntax error: unexpected end of file\n"
            ),
            Some(2)
        );
        assert_eq!(reported_line("bash: online 7 failed: line x: 9"), None);
        assert_eq!(reported_line("syntax error"), None);
    }

    #[cfg(unix)]
    #[test]
    fn created_bash_file_is_judged_by_the_shell() {
        require_bash();
        let path = Path::new("new.sh");
        for source in [FIXTURE, GRAMMAR_GAP] {
            assert_eq!(validate_syntax_str(source, path), Some(false));
            let check = check_created_file_syntax(
                &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                path,
                source,
            );
            assert_eq!(check.valid, Some(true), "{source}");
            assert_eq!(check.first_error_line, None);
            assert_eq!(check.skipped_reason, None);
        }

        let check = check_created_file_syntax(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            path,
            BROKEN_CASE,
        );
        assert_eq!(check.valid, Some(false));
        assert_eq!(check.first_error_line, Some(5));
        assert_eq!(check.skipped_reason, None);

        // tree-sitter stops at the grammar gap on line 1; bash names the stray
        // `fi` on line 3, and its line wins.
        let check = check_created_file_syntax(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            path,
            GAP_THEN_BROKEN,
        );
        assert_eq!(check.valid, Some(false));
        assert_eq!(check.first_error_line, Some(3));
    }

    fn created_write_response(source: &str, runner: Runner) -> serde_json::Value {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("new.sh");
        assert_eq!(validate_syntax_str(source, &path), Some(false));
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config(),
        );
        let request = serde_json::from_value(serde_json::json!({
            "id": "created-bash", "command": "write", "file": path, "content": source
        }))
        .unwrap();
        let response = with_runner(runner, || {
            crate::commands::write::handle_write(&request, &ctx)
        });
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["created"], true);
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
        let text = crate::subc_format::format_response("write", &response, false);
        assert!(!text.contains("new file does not parse"), "{text}");
        if response.data.get("syntax_skipped_reason").is_some() {
            assert!(text.contains("Syntax check: unknown"), "{text}");
        }
        response.data
    }

    #[test]
    fn created_bash_file_without_a_shell_verdict_is_unknown() {
        for (outcome, reason) in [
            (CheckOutcome::Missing, "bash_unavailable"),
            (
                CheckOutcome::Unavailable("permission denied".into()),
                "bash_unavailable",
            ),
            (CheckOutcome::TimedOut, "bash_timeout"),
            (
                CheckOutcome::Exited {
                    code: Some(1),
                    stderr: String::new(),
                },
                "bash_failed",
            ),
            (
                CheckOutcome::Exited {
                    code: None,
                    stderr: String::new(),
                },
                "bash_failed",
            ),
        ] {
            let mut outcome = Some(outcome);
            let response = created_write_response(
                BROKEN_CASE,
                Box::new(move |source| {
                    assert_eq!(source, BROKEN_CASE);
                    outcome.take().expect("one shell confirmation")
                }),
            );
            assert!(response.get("syntax_valid").is_none(), "{response:?}");
            assert!(response.get("syntax_error_line").is_none(), "{response:?}");
            assert_eq!(response["syntax_skipped_reason"], reason);
        }
    }

    #[test]
    fn created_bash_file_uses_the_shell_verdict() {
        let response = created_write_response(
            FIXTURE,
            Box::new(|_| CheckOutcome::Exited {
                code: Some(0),
                stderr: String::new(),
            }),
        );
        assert_eq!(response["syntax_valid"], true, "{response:?}");
        assert!(response.get("syntax_error_line").is_none(), "{response:?}");
        assert!(response.get("syntax_skipped_reason").is_none());
    }

    #[cfg(unix)]
    fn fake_bash(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("bash");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn run_bash_reads_stdin_with_only_path_in_its_environment() {
        let temp = tempfile::tempdir().unwrap();
        let env_dump = temp.path().join("env.txt");
        let bash = fake_bash(
            temp.path(),
            &format!(
                "[ \"$*\" = \"--norc --noprofile -n\" ] || exit 7\n\
                 [ \"$(cat)\" = \"echo hi\" ] || exit 8\n\
                 env > '{}'\n\
                 echo 'bash: line 1: syntax error' >&2\n\
                 exit 2\n",
                env_dump.display()
            ),
        );
        let outcome = run_bash(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &bash,
            "echo hi\n",
            Instant::now() + CHECK_TIMEOUT,
        );
        assert_eq!(
            outcome,
            CheckOutcome::Exited {
                code: Some(2),
                stderr: "bash: line 1: syntax error\n".into()
            }
        );
        let names: Vec<String> = std::fs::read_to_string(&env_dump)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_string()))
            .collect();
        assert!(names.iter().any(|name| name == "PATH"), "{names:?}");
        // The shell running the fake bash adds a few of its own variables.
        let shell_own = ["PATH", "PWD", "OLDPWD", "SHLVL", "_"];
        let inherited: Vec<String> = std::env::vars_os()
            .filter_map(|(name, _)| name.into_string().ok())
            .filter(|name| !shell_own.contains(&name.as_str()))
            .collect();
        assert!(!inherited.is_empty());
        for name in &names {
            assert!(
                !inherited.contains(name),
                "{name} leaked into bash -n's environment"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_bash_timeout_kills_a_slow_shell() {
        let temp = tempfile::tempdir().unwrap();
        let bash = fake_bash(temp.path(), "exec sleep 30\n");
        let started = Instant::now();
        let outcome = run_bash(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &bash,
            "echo hi\n",
            Instant::now() + Duration::from_millis(150),
        );
        assert_eq!(outcome, CheckOutcome::TimedOut);
        assert!(started.elapsed() < CHECK_TIMEOUT, "{:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn run_bash_keeps_the_verdict_when_bash_stops_reading_early() {
        let temp = tempfile::tempdir().unwrap();
        // Exits with a syntax error without reading a source larger than the
        // pipe buffer, as bash does at the first error.
        let bash = fake_bash(temp.path(), "exit 2\n");
        let source = "echo hi\n".repeat(64 * 1024);
        let outcome = run_bash(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &bash,
            &source,
            Instant::now() + CHECK_TIMEOUT,
        );
        assert!(
            matches!(outcome, CheckOutcome::Exited { code: Some(2), .. }),
            "{outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_bash_file_edit_is_judged_by_the_shell() {
        use crate::edit::write_format_validate;
        require_bash();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("script.sh");
        let original = "set -u\necho start\n";

        // Valid bash that tree-sitter rejects is kept, not rolled back.
        std::fs::write(&path, original).unwrap();
        let result = write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            GRAMMAR_GAP,
            &config(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), GRAMMAR_GAP);

        // An edit to a valid script tree-sitter misreads is also judged by bash.
        std::fs::write(&path, FIXTURE).unwrap();
        let edited = FIXTURE.replace("WINDOW DONE", "WINDOW FINISHED");
        let result = write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &edited,
            &config(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));

        // A real syntax error is still rolled back.
        std::fs::write(&path, original).unwrap();
        let result = write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            BROKEN_CASE,
            &config(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(result.rolled_back);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// An edit that repairs a script bash rejects into valid bash the grammar
    /// cannot read is judged by bash, even though the original is broken.
    #[test]
    fn broken_bash_file_repaired_into_a_grammar_gap_is_valid() {
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = calls.clone();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("script.sh");
        std::fs::write(&path, BROKEN_CASE).unwrap();
        assert_eq!(validate_syntax_str(FIXTURE, &path), Some(false));
        let result = with_runner(
            Box::new(move |source| {
                recorded.borrow_mut().push(source.to_owned());
                let code = match source {
                    BROKEN_CASE => SYNTAX_ERROR_STATUS,
                    FIXTURE => 0,
                    _ => panic!("unexpected bash input: {source}"),
                };
                CheckOutcome::Exited {
                    code: Some(code),
                    stderr: String::new(),
                }
            }),
            || {
                crate::edit::write_format_validate(
                    &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
                    &path,
                    FIXTURE,
                    &config(),
                    &serde_json::json!({}),
                )
                .unwrap()
            },
        );
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FIXTURE);
        assert_eq!(*calls.borrow(), [BROKEN_CASE, FIXTURE]);
    }

    #[cfg(unix)]
    #[test]
    fn broken_bash_file_repaired_with_real_bash_is_valid() {
        use crate::edit::write_format_validate;
        require_bash();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("script.sh");
        std::fs::write(&path, BROKEN_CASE).unwrap();
        let result = write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            GRAMMAR_GAP,
            &config(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(true));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), GRAMMAR_GAP);

        // Still broken after the edit: kept (nothing valid to roll back to)
        // and reported invalid.
        std::fs::write(&path, BROKEN_CASE).unwrap();
        let still_broken = format!("{BROKEN_CASE}{BROKEN_CASE}");
        let result = write_format_validate(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            &path,
            &still_broken,
            &config(),
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.syntax_valid, Some(false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), still_broken);
    }

    #[cfg(windows)]
    #[test]
    fn windows_never_runs_bash_and_reports_unknown() {
        assert_eq!(find_bash(), Ok(None));
        let check = check_created_file_syntax(
            &crate::sandbox_spawn::ToolLaunchPolicy::unconfined_for_test(),
            Path::new("new.sh"),
            BROKEN_CASE,
        );
        assert_eq!(check.valid, None);
        assert_eq!(check.skipped_reason, Some("bash_unavailable"));
        assert_eq!(check.first_error_line, None);
    }
}
