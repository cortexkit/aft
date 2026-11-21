//! Confirm bash tree-sitter errors with the shell's own syntax-only check.
//!
//! tree-sitter-bash rejects some scripts that bash itself accepts; an escaped
//! string comparison such as `[ "$a" \< "$b" ]` is one. A syntax check must
//! not tell the agent a valid script is broken, so when tree-sitter rejects a
//! new bash file, or an edit to a bash file that was valid before, `bash -n`
//! decides. It parses without running anything. When the shell cannot give a
//! verdict on a new file, the result is unknown rather than broken.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::format::{FormatError, SubprocessOutcome};
use crate::parser::{detect_language, LangId};

pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// `bash -n` exit status for a syntax error.
pub(crate) const SYNTAX_ERROR_STATUS: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckOutcome {
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
    tool_policy: &dyn crate::edit::tool_launcher::ToolLauncher,
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
    tool_policy: &dyn crate::edit::tool_launcher::ToolLauncher,
    source: &str,
) -> Result<ShellVerdict, &'static str> {
    verdict(tool_policy, source)
}

fn verdict(
    tool_policy: &dyn crate::edit::tool_launcher::ToolLauncher,
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
pub(crate) fn reported_line(stderr: &str) -> Option<usize> {
    stderr.match_indices("line ").find_map(|(start, marker)| {
        let rest = &stderr[start + marker.len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 || !rest[digits..].starts_with(':') {
            return None;
        }
        rest[..digits].parse().ok().filter(|line| *line > 0)
    })
}

fn check(tool_policy: &dyn crate::edit::tool_launcher::ToolLauncher, source: &str) -> CheckOutcome {
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
pub(crate) fn find_bash() -> Result<Option<PathBuf>, String> {
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
pub(crate) fn run_bash(
    tool_policy: &dyn crate::edit::tool_launcher::ToolLauncher,
    bash: &Path,
    source: &str,
    deadline: Instant,
) -> CheckOutcome {
    use super::tool_launcher::{ToolCommand, ToolEnvironment, ToolOutcome, ToolStdin, ToolTimeout};
    let outcome = tool_policy.run(ToolCommand {
        tool: "bash".into(),
        program: bash.as_os_str().into(),
        args: ["--norc", "--noprofile", "-n"]
            .map(std::ffi::OsString::from)
            .into(),
        cwd: None,
        stdin: ToolStdin::Source {
            source: source.into(),
            may_stop_reading: true,
        },
        environment: ToolEnvironment::Clean,
        timeout: ToolTimeout::Deadline(deadline),
    });
    match outcome {
        ToolOutcome::Refused(reason) => CheckOutcome::Unavailable(reason.0),
        outcome => check_outcome(outcome.into_result("bash")),
    }
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
pub(crate) type Runner = Box<dyn FnMut(&str) -> CheckOutcome>;

#[cfg(test)]
thread_local! {
    static RUNNER: std::cell::RefCell<Option<Runner>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_runner<T>(runner: Runner, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Runner>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUNNER.with(|runner| *runner.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(RUNNER.with(|slot| slot.replace(Some(runner))));
    f()
}
