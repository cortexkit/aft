use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response, ERROR_PERMISSION_REQUIRED};

// Foreground bash no longer has a 30s default kill cap. When `params.timeout`
// is `None`, the spawn path passes `None` through and the registry applies
// `DEFAULT_BG_TIMEOUT` (30 min) — same default as explicit `background: true`.
// The "agent should expect a 30s wait" UX is now enforced purely in the plugin
// layer's polling wait-window, decoupled from the task budget. See council
// decision in .alfonso/athena/council-aft-bash-timeout-design-5f25c3ee503ab303/
// for the full rationale.
#[cfg(test)]
const INLINE_OUTPUT_LIMIT: usize = 30 * 1024;
const BLOCKED_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "IFS",
    "PATH",
];

#[derive(Debug, Deserialize)]
struct BashParams {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    background: bool,
    #[serde(default = "default_notify_on_completion")]
    notify_on_completion: bool,
    #[serde(default = "default_compressed")]
    compressed: bool,
    #[serde(default)]
    permissions_granted: Vec<String>,
    #[serde(default)]
    permissions_requested: bool,
    #[serde(default)]
    env: HashMap<String, String>,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash: invalid params: {e}"),
            );
        }
    };

    if let Some(description) = params.description.as_deref() {
        log::debug!("bash description: {description}");
    }

    if let Some(blocked) = blocked_env_var(&params.env) {
        return Response::error(
            &req.id,
            "blocked_env_var",
            format!("bash env contains blocked variable: {blocked}"),
        );
    }

    let workdir = params
        .workdir
        .clone()
        .unwrap_or_else(|| default_workdir(ctx));
    let permission_asks = if params.permissions_requested || ctx.config().bash_permissions {
        crate::bash_permissions::scan::scan_with_cwd(&params.command, ctx, &workdir)
    } else {
        Vec::new()
    };
    if !permission_asks.is_empty()
        && !permissions_granted_cover(&permission_asks, &params.permissions_granted)
    {
        return Response::error_with_data(
            &req.id,
            ERROR_PERMISSION_REQUIRED,
            "bash command requires permission",
            json!({ "asks": permission_asks }),
        );
    }

    if let Some(mut response) =
        crate::bash_rewrite::try_rewrite(&params.command, req.session_id.as_deref(), ctx)
    {
        // Rewriter rules build their own internal request with a placeholder id
        // (e.g. "bash_rewrite") to call into read/grep/glob handlers. Stamp the
        // original bash request id back onto the response so the bridge correlates
        // it with the in-flight `send()` instead of timing out.
        response.id = req.id.clone();
        return response;
    }

    let workdir = params.workdir.clone();
    let env = (!params.env.is_empty()).then_some(params.env.clone());
    crate::bash_background::spawn(
        &req.id,
        req.session(),
        &params.command,
        workdir,
        env,
        params.timeout,
        ctx,
        params.background,
        params.notify_on_completion,
        params.compressed,
    )
}

fn blocked_env_var(env: &HashMap<String, String>) -> Option<&str> {
    env.keys()
        .find(|key| {
            BLOCKED_ENV_VARS.iter().any(|blocked| {
                #[cfg(windows)]
                {
                    key.eq_ignore_ascii_case(blocked)
                }
                #[cfg(not(windows))]
                {
                    key.as_str() == *blocked
                }
            })
        })
        .map(String::as_str)
}

fn permissions_granted_cover(
    asks: &[crate::bash_permissions::PermissionAsk],
    granted: &[String],
) -> bool {
    if asks.is_empty() {
        return true;
    }
    if granted.is_empty() {
        return false;
    }

    asks.iter().all(|ask| {
        ask.patterns
            .iter()
            .chain(ask.always.iter())
            .any(|pattern| granted.iter().any(|grant| grant == pattern))
    })
}

fn default_compressed() -> bool {
    true
}

fn default_notify_on_completion() -> bool {
    true
}

fn default_workdir(ctx: &AppContext) -> PathBuf {
    // Prefer the configured project root so bash commands run against the
    // user's project rather than the (often unrelated) cwd of the long-lived
    // aft worker process. Falls back to process cwd only when no project root
    // is configured (e.g. direct CLI usage).
    if let Some(root) = ctx.config().project_root.clone() {
        return root;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Generic retry loop for the Windows shell-fallback path. Walks the
/// `candidates` list, calling `try_one(shell)` for each; on `NotFound`
/// continues to the next candidate, on success returns the child, on
/// other errors returns immediately. Extracted from `spawn_shell_command`
/// so tests can exercise the retry decision logic without a real
/// `Command::spawn` (mock closures simulate per-shell outcomes).
///
/// `Child` is generic so tests can substitute a unit type or mock value;
/// production callers always pass `std::process::Child`. Compiled for tests
/// only so the retry-decision unit tests can run on macOS/Linux dev machines
/// without leaving dead code in non-test builds.
#[cfg(test)]
fn try_spawn_with_fallback<C, F>(
    candidates: &[crate::windows_shell::WindowsShell],
    mut try_one: F,
) -> Result<C, String>
where
    F: FnMut(&crate::windows_shell::WindowsShell) -> std::io::Result<C>,
{
    let mut last_error: Option<String> = None;
    for (idx, shell) in candidates.iter().enumerate() {
        match try_one(shell) {
            Ok(child) => {
                if idx > 0 {
                    crate::slog_warn!(
                        "bash spawn fell back to {} after {} earlier candidate(s) failed; \
                     the cached PATH probe disagreed with runtime spawn — likely PATH \
                     inheritance, antivirus / AppLocker / Defender ASR, or sandbox policy.",
                        shell.binary(),
                        idx
                    );
                }
                return Ok(child);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::slog_warn!(
                    "bash spawn: {} returned NotFound at runtime — trying next candidate",
                    shell.binary()
                );
                last_error = Some(format!("{}: {e}", shell.binary()));
                continue;
            }
            Err(e) => {
                // Non-NotFound errors (permission denied, OOM, etc.) are not
                // remediated by trying a different shell — return immediately.
                return Err(format!(
                    "failed to spawn bash command via {}: {e}",
                    shell.binary()
                ));
            }
        }
    }
    Err(format!(
        "failed to spawn bash command: no Windows shell could be spawned. \
         Last error: {}. PATH-probed candidates: {:?}",
        last_error.unwrap_or_else(|| "no candidates were attempted".to_string()),
        candidates.iter().map(|s| s.binary()).collect::<Vec<_>>()
    ))
}

/// Compute the byte index where the last `INLINE_OUTPUT_LIMIT` bytes of
/// `output` start, snapped forward to a UTF-8 character boundary so we
/// never split a multi-byte char.
///
/// The earlier implementation walked `char_indices().rev().find_map(...)`,
/// which returned the LAST char's start index on the very first iteration
/// (because `output.len() - idx == 1 <= INLINE_OUTPUT_LIMIT`). That bug
/// made the inline preview a single character for any output above the
/// limit. This helper computes the suffix start by byte arithmetic and
/// keeps approximately `INLINE_OUTPUT_LIMIT` trailing bytes intact.
#[cfg(test)]
fn inline_output_suffix_start(output: &str) -> usize {
    let mut start = output.len().saturating_sub(INLINE_OUTPUT_LIMIT);
    while start < output.len() && !output.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::windows_shell::WindowsShell;

    /// Regression: prior reverse `char_indices` logic returned only the LAST
    /// character of `output` because the first reverse-iteration index already
    /// satisfied `output.len() - idx == 1 <= INLINE_OUTPUT_LIMIT`. The new
    /// implementation must keep approximately `INLINE_OUTPUT_LIMIT` trailing
    /// bytes intact for ASCII input.
    #[test]
    fn inline_output_suffix_keeps_full_limit_for_ascii() {
        let total = INLINE_OUTPUT_LIMIT * 2;
        let output: String = "x".repeat(total);
        let start = inline_output_suffix_start(&output);
        let suffix_len = output.len() - start;
        assert!(
            suffix_len > INLINE_OUTPUT_LIMIT / 2,
            "ascii suffix too short: got {suffix_len} bytes (limit={INLINE_OUTPUT_LIMIT})"
        );
        assert!(
            suffix_len <= INLINE_OUTPUT_LIMIT,
            "ascii suffix exceeded limit: got {suffix_len} bytes (limit={INLINE_OUTPUT_LIMIT})"
        );
        // Guard against a regression to the 1-char bug.
        assert!(suffix_len > 1, "suffix collapsed to a single character");
    }

    /// The suffix-start index must always land on a UTF-8 char boundary so
    /// `output[start..]` is a valid `&str`. Multi-byte chars (like 4-byte
    /// emoji) require boundary snapping when the raw byte split lands inside
    /// a code point.
    #[test]
    fn inline_output_suffix_respects_utf8_boundaries() {
        // Each crab is 4 bytes. 20_000 of them = 80_000 bytes, well over the
        // inline limit. The byte index `len - INLINE_OUTPUT_LIMIT` is unlikely
        // to be a 4-byte boundary.
        let output: String = "🦀".repeat(20_000);
        let start = inline_output_suffix_start(&output);
        assert!(
            output.is_char_boundary(start),
            "suffix split a multi-byte char"
        );
        // Slicing must succeed without panic.
        let suffix = &output[start..];
        let suffix_bytes = suffix.len();
        assert!(
            suffix_bytes <= INLINE_OUTPUT_LIMIT + 4,
            "utf8 suffix far above limit: got {suffix_bytes} bytes (limit={INLINE_OUTPUT_LIMIT})"
        );
        assert!(
            suffix_bytes > INLINE_OUTPUT_LIMIT / 2,
            "utf8 suffix too short: got {suffix_bytes} bytes (limit={INLINE_OUTPUT_LIMIT})"
        );
    }

    /// Output below the inline limit is returned by `maybe_truncate` directly,
    /// but the helper must still return `0` so callers slicing `output[start..]`
    /// get the full string.
    #[test]
    fn inline_output_suffix_returns_zero_for_short_input() {
        let output = "small";
        assert_eq!(inline_output_suffix_start(output), 0);
    }

    /// Issue #27: `WindowsShell::args` must produce shell-appropriate flags.
    /// PowerShell variants need `-Command <string>`; cmd.exe needs `/D /C
    /// <string>`. Mixing these up would make the spawned shell ignore the
    /// command or interpret it as a parameter to the wrong cmdlet.
    #[cfg(windows)]
    #[test]
    fn windows_shell_args_match_each_shells_invocation_contract() {
        let cmd = "echo hello";
        let pwsh_args = WindowsShell::Pwsh.args(cmd);
        assert!(
            pwsh_args.contains(&"-Command"),
            "pwsh args missing -Command: {pwsh_args:?}"
        );
        assert!(pwsh_args.contains(&cmd), "pwsh args missing command body");
        assert!(
            pwsh_args.contains(&"-NonInteractive"),
            "pwsh args missing -NonInteractive (would hang on prompts)"
        );

        let ps_args = WindowsShell::Powershell.args(cmd);
        assert_eq!(
            pwsh_args, ps_args,
            "pwsh and powershell share the same arg set"
        );

        let cmd_args = WindowsShell::Cmd.args(cmd);
        assert_eq!(
            cmd_args,
            vec!["/D", "/C", cmd],
            "cmd.exe must use /D /C contract"
        );
        assert!(
            !cmd_args.contains(&"-Command"),
            "cmd args must not leak PowerShell flags: {cmd_args:?}"
        );
    }

    /// Each shell's binary name must match what `Command::new` expects on
    /// Windows. Bare names rely on PATH lookup; `.exe` suffix is mandatory
    /// for cross-compatibility with `which::which()` probing.
    #[cfg(windows)]
    #[test]
    fn windows_shell_binary_names_have_exe_suffix() {
        assert_eq!(WindowsShell::Pwsh.binary(), "pwsh.exe");
        assert_eq!(WindowsShell::Powershell.binary(), "powershell.exe");
        assert_eq!(WindowsShell::Cmd.binary(), "cmd.exe");
    }

    /// Issue #27 P2 test gap: foreground retry path. When the first
    /// candidate returns NotFound at runtime spawn time, the loop must
    /// move to the next candidate. The first SUCCESSFUL spawn wins.
    /// Uses the generic `try_spawn_with_fallback` so the test runs on
    /// macOS/Linux dev machines without a real Windows spawn.
    #[test]
    fn try_spawn_with_fallback_retries_on_notfound_until_success() {
        use crate::windows_shell::WindowsShell;
        use std::cell::RefCell;
        use std::io::{Error, ErrorKind};

        let candidates = [
            WindowsShell::Pwsh,
            WindowsShell::Powershell,
            WindowsShell::Cmd,
        ];
        let attempts: RefCell<Vec<WindowsShell>> = RefCell::new(Vec::new());

        let result: Result<&'static str, String> = try_spawn_with_fallback(&candidates, |shell| {
            attempts.borrow_mut().push(shell.clone());
            match shell {
                WindowsShell::Pwsh | WindowsShell::Powershell => {
                    Err(Error::new(ErrorKind::NotFound, "blocked"))
                }
                WindowsShell::Cmd => Ok("ok-from-cmd"),
                WindowsShell::Posix(_) => unreachable!("test fixture has no Posix shell"),
            }
        });

        assert_eq!(result, Ok("ok-from-cmd"));
        assert_eq!(
            attempts.into_inner(),
            vec![
                WindowsShell::Pwsh,
                WindowsShell::Powershell,
                WindowsShell::Cmd,
            ],
            "retry loop must walk candidates in order until one succeeds"
        );
    }

    /// Issue #27 P2 test gap: short-circuit on first success. When pwsh
    /// spawns successfully, the loop must NOT call try_one for the
    /// remaining candidates — that would waste resources and could double-
    /// spawn shells.
    #[test]
    fn try_spawn_with_fallback_stops_at_first_success() {
        use crate::windows_shell::WindowsShell;
        use std::cell::RefCell;

        let candidates = [
            WindowsShell::Pwsh,
            WindowsShell::Powershell,
            WindowsShell::Cmd,
        ];
        let attempts: RefCell<usize> = RefCell::new(0);

        let result: Result<u32, String> = try_spawn_with_fallback(&candidates, |_shell| {
            *attempts.borrow_mut() += 1;
            Ok(42)
        });

        assert_eq!(result, Ok(42));
        assert_eq!(
            attempts.into_inner(),
            1,
            "first success must short-circuit; later candidates not attempted"
        );
    }

    /// Issue #27 P2 test gap: non-NotFound errors return immediately.
    /// PermissionDenied, OutOfMemory, etc. are not remediated by trying a
    /// different shell — those would just fail in the same way. Returning
    /// early avoids wasted work and surfaces the real error.
    #[test]
    fn try_spawn_with_fallback_returns_immediately_on_non_notfound_error() {
        use crate::windows_shell::WindowsShell;
        use std::cell::RefCell;
        use std::io::{Error, ErrorKind};

        let candidates = [
            WindowsShell::Pwsh,
            WindowsShell::Powershell,
            WindowsShell::Cmd,
        ];
        let attempts: RefCell<Vec<WindowsShell>> = RefCell::new(Vec::new());

        let result: Result<&'static str, String> = try_spawn_with_fallback(&candidates, |shell| {
            attempts.borrow_mut().push(shell.clone());
            Err(Error::new(ErrorKind::PermissionDenied, "denied by ACL"))
        });

        assert!(result.is_err(), "PermissionDenied must error out");
        let err = result.unwrap_err();
        assert!(
            err.contains("pwsh.exe"),
            "error must name the failing shell: {err}"
        );
        assert!(
            err.contains("denied by ACL"),
            "error must include underlying io error: {err}"
        );
        assert_eq!(
            attempts.into_inner(),
            vec![WindowsShell::Pwsh],
            "non-NotFound must NOT retry with later candidates"
        );
    }

    /// Issue #27 P2 test gap: all candidates fail with NotFound. This is
    /// the worst case where no shell on the system is reachable — the
    /// final error must include the candidate list so users debugging
    /// issue #27-class problems can see what was attempted.
    #[test]
    fn try_spawn_with_fallback_reports_all_candidates_when_none_succeed() {
        use crate::windows_shell::WindowsShell;
        use std::io::{Error, ErrorKind};

        let candidates = [WindowsShell::Pwsh, WindowsShell::Cmd];

        let result: Result<&'static str, String> = try_spawn_with_fallback(&candidates, |_shell| {
            Err(Error::new(ErrorKind::NotFound, "no shell"))
        });

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("pwsh.exe"),
            "error must list pwsh.exe candidate: {err}"
        );
        assert!(
            err.contains("cmd.exe"),
            "error must list cmd.exe candidate: {err}"
        );
        assert!(
            err.contains("no Windows shell could be spawned"),
            "error message must indicate exhaustion: {err}"
        );
    }

    /// Edge case: empty candidate list. Should return an error mentioning
    /// "no candidates were attempted" rather than panic on empty iteration.
    #[test]
    fn try_spawn_with_fallback_handles_empty_candidates_list() {
        use crate::windows_shell::WindowsShell;

        let candidates: [WindowsShell; 0] = [];
        let result: Result<&'static str, String> = try_spawn_with_fallback(&candidates, |_shell| {
            panic!("try_one must not be called for empty candidates")
        });

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("no candidates were attempted"),
            "empty list must report no-attempt error: {err}"
        );
    }
}
