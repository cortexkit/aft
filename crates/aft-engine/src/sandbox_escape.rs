//! Shared validation and wire data for a person-approved one-command escape.

use crate::config::SandboxMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const REQUIRED_CODE: &str = "sandbox_escape_required";
pub const REFUSED_CODE: &str = "sandbox_escape_refused";
pub const UNAVAILABLE_CODE: &str = "sandbox_escape_unavailable";
pub const DECLINED_CODE: &str = "sandbox_escape_declined";
pub const EXPIRED_CODE: &str = "sandbox_escape_expired";
pub const WITHDRAWN_CODE: &str = "sandbox_escape_withdrawn";

pub const HEAD_ONLY: &str = "Sandbox escape is available only to a head agent.";
pub const LOCAL_ONLY: &str = "Sandbox escape is local only; remove `runon`.";
pub const WINDOWS_REFUSED: &str = "Sandbox escape is not available on Windows.";
pub const COMMAND_INVALID: &str =
    "Sandbox escape needs a nonblank command of at most 2,000 characters.";
pub const COMMAND_UNSHOWABLE: &str =
    "The command contains characters the sandbox escape card cannot show.";
pub const REASON_INVALID: &str =
    "Give one nonblank line of at most 160 characters in `escape_sandbox`.";
pub const DECLINED: &str = "Sandbox escape declined; the command did not run.";
pub const EXPIRED: &str = "Sandbox escape approval expired; the command did not run.";
pub const WITHDRAWN: &str = "Sandbox escape approval was withdrawn; the command did not run.";
pub const UNAVAILABLE: &str = "Sandbox escape is unavailable; the command did not run.";
pub const APPROVAL_REQUIRED: &str = "Sandbox escape needs approval on an approving device.";

pub const EXPIRES_IN_MS: u64 = 600_000;

/// The exact invocation and one-shot host grant waiting for a person's answer.
/// `command` is the agent's original command, not a shell-escaped reconstruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscapePlan {
    pub grant_id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub shell: PathBuf,
    pub escapes_level: SandboxMode,
    pub background: bool,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: &'static str,
}

pub const fn refusal(code: &'static str, message: &'static str) -> Refusal {
    Refusal { code, message }
}

fn has_forbidden_command_character(command: &str) -> bool {
    command.chars().any(|character| {
        (character.is_control() && !matches!(character, '\n' | '\t'))
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

/// Validate the fields AFT must reject locally. Cingulate remains the authority
/// for the same bounds when the closed card body arrives.
pub fn validate_fields(command: &str, reason: &str) -> Option<Refusal> {
    if command.trim().is_empty() || command.chars().count() > 2_000 {
        return Some(refusal(REFUSED_CODE, COMMAND_INVALID));
    }
    if has_forbidden_command_character(command) {
        return Some(refusal(REFUSED_CODE, COMMAND_UNSHOWABLE));
    }
    if reason.trim().is_empty() || reason.chars().count() > 160 || reason.contains(['\r', '\n']) {
        return Some(refusal(REFUSED_CODE, REASON_INVALID));
    }
    None
}

pub fn escaped_level_name(mode: SandboxMode) -> Option<&'static str> {
    match mode {
        SandboxMode::Off => None,
        other => Some(other.as_str()),
    }
}
