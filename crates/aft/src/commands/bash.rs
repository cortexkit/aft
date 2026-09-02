use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
const DEFAULT_PTY_ROWS: u16 = 24;
const DEFAULT_PTY_COLS: u16 = 80;
const MAX_PTY_ROWS: u16 = 60;
const MAX_PTY_COLS: u16 = 140;

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

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BashSandbox {
    Host,
}

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
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    pty: bool,
    #[serde(default)]
    pty_rows: Option<u16>,
    #[serde(default)]
    pty_cols: Option<u16>,
    #[serde(default = "default_notify_on_completion")]
    notify_on_completion: bool,
    #[serde(default = "default_compressed")]
    compressed: bool,
    #[serde(default)]
    sandbox: Option<BashSandbox>,
    #[serde(default)]
    shell: crate::bash_background::BashShell,
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
    let mut params = match serde_json::from_value::<BashParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash: invalid params: {e}"),
            );
        }
    };
    // The dedicated external route is defensive as well as convenient: callers
    // cannot accidentally execute Bash merely by omitting the selector.
    if req.command == "powershell" {
        params.shell = crate::bash_background::BashShell::Powershell;
    }

    if let Some(description) = params.description.as_deref() {
        log::debug!("bash description: {description}");
    }

    let shell_path = match crate::bash_background::resolve_shell_path(params.pty, params.shell) {
        Ok(path) => path,
        Err(message) => return Response::error(&req.id, "powershell_not_installed", message),
    };
    let execution_command = params.shell.command_text(&params.command);

    // NOTE (v0.30.1 prep, unblock-only): the previous two rejections
    // ("PTY mode requires background: true" and "ptyRows/ptyCols require
    // pty: true") have been removed so that:
    //   1. pty:true silently implies background:true (handled below by
    //      passing `params.background || params.pty` to bash_background::spawn)
    //   2. ptyRows/ptyCols are silently ignored when pty:false instead of
    //      rejecting agent calls that defensively include the params
    // Bounds validation (1..60 rows, 1..140 cols) still applies via
    // `validate_pty_dimensions` below.

    if params.wait && params.pty {
        return Response::error(
            &req.id,
            "invalid_request",
            "bash: wait:true cannot be used with pty:true because PTY sessions run in background",
        );
    }
    if params.wait && params.background {
        return Response::error(
            &req.id,
            "invalid_request",
            "bash: wait:true cannot be used with background:true",
        );
    }

    if let Err(message) = validate_pty_dimensions(params.pty_rows, params.pty_cols) {
        return Response::error(&req.id, "invalid_request", message);
    }

    if let Some(blocked) = blocked_env_var(&params.env) {
        return Response::error(
            &req.id,
            "blocked_env_var",
            format!("bash env contains blocked variable: {blocked}"),
        );
    }

    if let Some(refusal) = crate::sandbox_spawn::unsupported_platform_sandbox_refusal(ctx) {
        return Response::error(
            &req.id,
            refusal
                .refusal_code()
                .expect("unsupported platform refusal has a code"),
            refusal
                .refusal_message()
                .expect("unsupported platform refusal has a message"),
        );
    }

    let workdir = params
        .workdir
        .clone()
        .unwrap_or_else(|| default_workdir(ctx));
    let principal = crate::sandbox_spawn::current_authenticated_principal();
    let host_requested = matches!(params.sandbox, Some(BashSandbox::Host));
    if host_requested && !crate::sandbox_spawn::principal_is_first_party(&principal) {
        return Response::error(
            &req.id,
            "sandbox_escalation_denied",
            "sandbox host escalation is unavailable to untrusted principals",
        );
    }

    #[cfg(unix)]
    let mut spawn_workdir = params.workdir.clone();
    #[cfg(not(unix))]
    let spawn_workdir = params.workdir.clone();
    #[cfg(unix)]
    let mut host_escalation: Option<crate::sandbox_spawn::HostEscalationAttempt> = None;
    #[cfg(not(unix))]
    let host_escalation: Option<crate::sandbox_spawn::HostEscalationAttempt> = None;
    #[cfg(unix)]
    if host_requested && ctx.config().sandbox.enabled {
        let configured_root = ctx
            .config()
            .project_root
            .clone()
            .unwrap_or_else(|| default_workdir(ctx));
        let root = match std::fs::canonicalize(&configured_root) {
            Ok(root) => root,
            Err(error) => {
                return Response::error(
                    &req.id,
                    "sandbox_escalation_denied",
                    format!("failed to canonicalize sandbox escalation root: {error}"),
                );
            }
        };
        let candidate_cwd = if workdir.is_absolute() {
            workdir.clone()
        } else {
            root.join(&workdir)
        };
        let cwd = match std::fs::canonicalize(&candidate_cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                return Response::error(
                    &req.id,
                    "sandbox_escalation_denied",
                    format!("failed to canonicalize sandbox escalation cwd: {error}"),
                );
            }
        };
        let shell_path = std::fs::canonicalize(&shell_path).unwrap_or_else(|_| shell_path.clone());
        let environment =
            crate::sandbox_spawn::approved_payload_environment(&params.env, &std::env::temp_dir());
        if let Some(grant_id) = params
            .permissions_granted
            .iter()
            .find(|grant| grant.starts_with("esc_"))
        {
            host_escalation = Some(crate::sandbox_spawn::HostEscalationAttempt {
                grant_id: grant_id.clone(),
                command: execution_command.as_bytes().to_vec(),
                root,
                cwd: cwd.clone(),
                shell_path,
                environment,
            });
            spawn_workdir = Some(cwd);
        } else {
            let storage_dir = crate::bash_background::task_storage_dir(ctx);
            let grant_id = match crate::sandbox_spawn::mint_host_escalation_grant(
                ctx,
                &principal,
                execution_command.as_bytes(),
                &root,
                &cwd,
                &shell_path,
                &environment,
                &storage_dir,
                req.session(),
            ) {
                Ok(grant_id) => grant_id,
                Err(error) => {
                    return Response::error(&req.id, "sandbox_escalation_denied", error);
                }
            };
            return Response::error_with_data(
                &req.id,
                ERROR_PERMISSION_REQUIRED,
                "bash command requires host escalation approval",
                json!({
                    "asks": [{
                        "kind": "escalation",
                        "command": params.command,
                        "cwd": cwd,
                        "grant_id": grant_id,
                    }]
                }),
            );
        }
    }
    let native_report_only =
        crate::sandbox_spawn::native_sandbox_enforced(ctx, &principal) && host_escalation.is_none();
    // The bash scanner parses POSIX grammar and must not treat PowerShell
    // syntax as understood. PowerShell therefore takes an explicit, per-command
    // approval path instead of a potentially unsafe rewrite or auto-allow.
    let permission_asks = if params.shell.is_powershell() {
        conservative_powershell_permission_asks(&params.command)
    } else if native_report_only || params.permissions_requested || ctx.config().bash_permissions {
        crate::bash_permissions::scan::scan_with_cwd(&params.command, ctx, &workdir)
    } else {
        Vec::new()
    };
    if !native_report_only
        && !permission_asks.is_empty()
        && !permissions_granted_cover(&permission_asks, &params.permissions_granted)
    {
        return Response::error_with_data(
            &req.id,
            ERROR_PERMISSION_REQUIRED,
            "bash command requires permission",
            json!({ "asks": permission_asks }),
        );
    }

    // Rewrite (cat→read, grep→grep tool, append→edit, …) resolves relative
    // paths via ctx.validate_path, i.e. against the PROJECT ROOT — it has no
    // notion of the bash cwd. So it's only faithful when the effective workdir
    // IS the project root. With an explicit different `workdir`, a rewritten
    // `echo hi >> notes.txt` would write project_root/notes.txt instead of
    // workdir/notes.txt — silent wrong-file mutation. Rewrite is a pure
    // optimization, so skip it and let native bash (which honors cwd) run the
    // command verbatim when the workdir differs from the project root.
    if !params.shell.is_powershell()
        && host_escalation.is_none()
        && workdir_matches_project_root(&workdir, ctx)
    {
        if let Some(response) = crate::bash_rewrite::try_rewrite_for_request(
            &params.command,
            &req.id,
            req.session_id.as_deref(),
            ctx,
            &principal,
        ) {
            // The typed rewrite request already carries the public request ID,
            // so the bridge can correlate the handler response without a
            // second response mutation or a native fallback.
            return response;
        }
    } else {
        let reason = if params.shell.is_powershell() {
            "PowerShell syntax bypasses POSIX rewrite rules"
        } else if host_escalation.is_some() {
            "host escalation owns process execution"
        } else {
            "bash workdir differs from the project root"
        };
        let branch = if params.shell.is_powershell() || host_escalation.is_some() {
            "dispatch.native.no_rule"
        } else {
            "dispatch.native.non_root_workdir"
        };
        crate::bash_rewrite::dispatch::record_native(
            &req.id,
            crate::bash_rewrite::catalog::ControlRole::Native,
            branch,
            reason,
        );
    }

    let workdir = spawn_workdir;
    let env = (!params.env.is_empty()).then_some(params.env.clone());
    // pty:true silently implies background:true so agents don't need to know
    // both flags. The PTY runtime requires a polling lifecycle regardless.
    let effective_background = params.background || params.pty;
    // Treat ptyRows/ptyCols == 0 as "use default" so empty-sentinel-style
    // agent calls don't trip bounds validation.
    let pty_rows = params
        .pty_rows
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PTY_ROWS);
    let pty_cols = params
        .pty_cols
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PTY_COLS);
    let scanner_report = native_report_only
        .then_some(permission_asks)
        .unwrap_or_default();
    crate::bash_background::spawn(
        &req.id,
        req.session(),
        &execution_command,
        params.shell,
        shell_path,
        workdir,
        env,
        params.timeout,
        ctx,
        effective_background,
        params.notify_on_completion,
        params.compressed,
        params.pty,
        pty_rows,
        pty_cols,
        scanner_report,
        host_escalation,
    )
}

fn validate_pty_dimensions(rows: Option<u16>, cols: Option<u16>) -> Result<(), &'static str> {
    // 0 is silently treated as "use default" (see handle()); only reject
    // explicit out-of-bound positive values.
    if rows.is_some_and(|value| value > MAX_PTY_ROWS) {
        return Err("ptyRows must be an integer between 1 and 60");
    }
    if cols.is_some_and(|value| value > MAX_PTY_COLS) {
        return Err("ptyCols must be an integer between 1 and 140");
    }
    Ok(())
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

fn conservative_powershell_permission_asks(
    command: &str,
) -> Vec<crate::bash_permissions::PermissionAsk> {
    vec![crate::bash_permissions::PermissionAsk {
        kind: crate::bash_permissions::PermissionKind::Bash,
        patterns: vec![command.to_string()],
        always: Vec::new(),
    }]
}

fn default_compressed() -> bool {
    true
}

fn default_notify_on_completion() -> bool {
    true
}

/// Is the effective bash workdir the same directory as the project root? Used
/// to gate command rewriting, which resolves relative paths against the project
/// root and would otherwise target the wrong file when an explicit workdir
/// differs. Canonicalizes both sides so a relative workdir or a /var↔/private/var
/// symlink alias still compares equal; falls back to a plain compare when
/// canonicalization fails (e.g. a not-yet-existing dir — native bash handles it).
fn workdir_matches_project_root(workdir: &Path, ctx: &AppContext) -> bool {
    let Some(root) = ctx.config().project_root.clone() else {
        // No project root → rewrite handlers resolve against process cwd via
        // default_workdir, which equals the workdir default here. Only match when
        // the caller didn't pass a divergent explicit workdir.
        return workdir == default_workdir(ctx);
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(workdir) == canon(&root) || workdir == root
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::windows_shell::WindowsShell;

    fn ctx_with_root(root: &std::path::Path) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                ..crate::config::Config::default()
            },
        )
    }

    #[test]
    fn powershell_commands_require_approval_without_posix_scanning() {
        // `cat` is a POSIX rewrite candidate, but PowerShell has its own syntax
        // and aliases. The request must remain an explicit permission ask.
        let asks = conservative_powershell_permission_asks("cat $env:USERPROFILE\\secret.txt");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].patterns, ["cat $env:USERPROFILE\\secret.txt"]);
    }

    // Command rewriting resolves relative paths against the project root, so it
    // must only run when the effective workdir IS the project root — otherwise a
    // rewritten `echo hi >> notes.txt` with workdir=subdir writes the wrong file.
    #[test]
    fn workdir_gate_matches_root_and_rejects_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let ctx = ctx_with_root(root);

        // Project root itself matches (incl. /var↔/private/var symlink alias via
        // canonicalize).
        assert!(workdir_matches_project_root(root, &ctx));
        assert!(workdir_matches_project_root(
            &std::fs::canonicalize(root).unwrap(),
            &ctx
        ));
        // A real subdirectory must NOT match → rewrite is skipped, native bash runs.
        assert!(!workdir_matches_project_root(&sub, &ctx));
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

    fn spawn_test_context(project_root: &Path, storage_dir: &Path) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project_root.to_path_buf()),
                storage_dir: Some(storage_dir.to_path_buf()),
                experimental_bash_background: true,
                ..crate::config::Config::default()
            },
        )
    }

    fn spawn_test_request(id: &str, command: &str, background: bool) -> RawRequest {
        serde_json::from_value(json!({
            "id": id,
            "command": "bash",
            "session_id": "sandbox-spawn-test",
            "params": {
                "command": command,
                "background": background,
                "compressed": false,
            },
        }))
        .unwrap()
    }

    fn stop_spawned_test_task(ctx: &AppContext, response: &Response) {
        if let Some(task_id) = response
            .data
            .get("task_id")
            .and_then(serde_json::Value::as_str)
        {
            let _ = ctx.bash_background().kill(task_id, "sandbox-spawn-test");
        }
    }

    #[cfg(unix)]
    #[test]
    fn host_request_returns_exact_escalation_ask_and_untrusted_request_is_denied() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| config.sandbox.enabled = true);
        crate::sandbox_spawn::install_sandbox_spawn_test_seam(project.path().to_path_buf());
        let mut request = spawn_test_request("host-ask", "printf 'exact  value'", false);
        request.params["params"]["sandbox"] = json!("host");

        let response = handle(&request, &ctx);
        assert!(!response.success);
        assert_eq!(
            response
                .data
                .get("code")
                .and_then(serde_json::Value::as_str),
            Some(ERROR_PERMISSION_REQUIRED)
        );
        let ask = &response.data["asks"][0];
        assert_eq!(ask["kind"], "escalation");
        assert_eq!(ask["command"], "printf 'exact  value'");
        let canonical_project = project.path().canonicalize().unwrap();
        assert_eq!(ask["cwd"].as_str(), canonical_project.to_str());
        let grant_id = ask["grant_id"].as_str().unwrap().to_string();
        assert!(grant_id.starts_with("esc_"));

        let mut changed = spawn_test_request("host-changed", "printf 'exact  valuE'", false);
        changed.params["params"]["sandbox"] = json!("host");
        changed.params["params"]["permissions_granted"] = json!([grant_id]);
        let changed_response = handle(&changed, &ctx);
        assert!(!changed_response.success);
        assert_eq!(changed_response.data["code"], "sandbox_escalation_denied");
        assert_eq!(changed_response.data["mismatch_class"], "digest_mismatch");
        assert_eq!(
            ctx.bash_background().try_health_counts().unwrap().running,
            0
        );

        let mut approved = spawn_test_request("host-approved", "printf 'exact  value'", false);
        approved.params["params"]["sandbox"] = json!("host");
        approved.params["params"]["permissions_granted"] = json!([grant_id]);
        let invalidated_response = handle(&approved, &ctx);
        assert!(!invalidated_response.success);
        assert_eq!(invalidated_response.data["mismatch_class"], "consumed");

        approved.params["params"]
            .as_object_mut()
            .unwrap()
            .remove("permissions_granted");
        let remint_response = handle(&approved, &ctx);
        assert!(!remint_response.success);
        assert_eq!(remint_response.data["code"], ERROR_PERMISSION_REQUIRED);
        let fresh_grant = remint_response.data["asks"][0]["grant_id"]
            .as_str()
            .unwrap()
            .to_string();
        approved.params["params"]["permissions_granted"] = json!([fresh_grant]);
        let approved_response = handle(&approved, &ctx);
        assert!(approved_response.success, "{:#?}", approved_response.data);
        assert_eq!(
            crate::sandbox_spawn::sandbox_spawn_test_observations(project.path())
                .last()
                .map(|observation| observation.requested_tier),
            Some(crate::sandbox_spawn::RequestedSandboxTier::Host)
        );
        stop_spawned_test_task(&ctx, &approved_response);

        let consumed_response = handle(&approved, &ctx);
        assert!(!consumed_response.success);
        assert_eq!(consumed_response.data["mismatch_class"], "consumed");

        let principal = crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::Untrusted,
            route_channel: 4,
            route_epoch: 2,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "mcp-session".to_string(),
            principal_id: Some("unverified".to_string()),
        };
        let denied = crate::sandbox_spawn::with_authenticated_principal(principal, || {
            handle(&request, &ctx)
        });
        assert!(!denied.success);
        assert_eq!(denied.data["code"], "sandbox_escalation_denied");
        assert_eq!(ctx.escalation_grants().lock().len_for_test(), 2);
        crate::sandbox_spawn::clear_sandbox_spawn_test_seam(project.path());
    }

    #[test]
    fn foreground_and_background_bash_both_resolve_a_spawn_plan() {
        use crate::sandbox_spawn::{
            clear_sandbox_spawn_test_seam, install_sandbox_spawn_test_seam,
            sandbox_spawn_test_observations, AuthenticatedPrincipal, RequestedSandboxTier,
            SandboxTaskKind,
        };

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        install_sandbox_spawn_test_seam(project.path().to_path_buf());

        let foreground = handle(
            &spawn_test_request("sandbox-foreground", "echo foreground", false),
            &ctx,
        );
        assert!(
            foreground.success,
            "foreground spawn failed: {foreground:?}"
        );
        let background = handle(
            &spawn_test_request("sandbox-background", "echo background", true),
            &ctx,
        );
        assert!(
            background.success,
            "background spawn failed: {background:?}"
        );

        let observations = sandbox_spawn_test_observations(project.path());
        clear_sandbox_spawn_test_seam(project.path());
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations
                .iter()
                .map(|observation| observation.task_kind)
                .collect::<Vec<_>>(),
            vec![
                SandboxTaskKind::BashForeground,
                SandboxTaskKind::BashBackground,
            ]
        );
        assert!(observations.iter().all(|observation| {
            observation.principal == AuthenticatedPrincipal::FirstParty
                && observation.requested_tier == RequestedSandboxTier::Disabled
        }));

        stop_spawned_test_task(&ctx, &foreground);
        stop_spawned_test_task(&ctx, &background);
    }

    #[test]
    fn refused_test_plan_fails_closed_before_process_creation() {
        use crate::sandbox_spawn::{with_spawn_plan_for_test, SpawnPlan};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let sentinel = project.path().join("must-not-exist");
        let ctx = spawn_test_context(project.path(), storage.path());
        let request = spawn_test_request(
            "sandbox-refused",
            &format!("touch {}", sentinel.display()),
            false,
        );

        let response = with_spawn_plan_for_test(
            SpawnPlan::refused_for_test("sandbox_principal_unknown"),
            || handle(&request, &ctx),
        );

        assert!(!response.success);
        assert_eq!(
            response
                .data
                .get("code")
                .and_then(serde_json::Value::as_str),
            Some("sandbox_principal_unknown")
        );
        assert!(!sentinel.exists());
    }

    #[cfg(windows)]
    #[test]
    fn unsupported_platform_refuses_before_rewrite_or_spawn() {
        use crate::sandbox_spawn::{
            clear_sandbox_spawn_test_seam, install_sandbox_spawn_test_seam,
            sandbox_spawn_test_observations,
        };

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let file = project.path().join("rewrite-probe.txt");
        std::fs::write(&file, "must-not-be-read").unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.enabled = true;
            config.experimental_bash_rewrite = true;
        });
        install_sandbox_spawn_test_seam(project.path().to_path_buf());
        let request = spawn_test_request(
            "unsupported-sandbox-platform",
            &format!("cat {}", file.display()),
            false,
        );

        let response = handle(&request, &ctx);
        let observations = sandbox_spawn_test_observations(project.path());
        clear_sandbox_spawn_test_seam(project.path());

        assert!(!response.success);
        assert_eq!(response.data["code"], "sandbox_unavailable");
        assert_eq!(
            response.data["message"],
            "sandbox is not supported on this platform; disable sandbox.enabled or run on macOS/Linux"
        );
        assert!(observations.is_empty(), "no child spawn may be resolved");
    }

    #[cfg(unix)]
    #[test]
    fn launcher_test_plan_wraps_the_spawned_command_line() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        use crate::sandbox_profile::SandboxProfile;
        use crate::sandbox_spawn::{with_spawn_plan_for_test, SpawnPlan};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let args_log = project.path().join("launcher-args.txt");
        let launcher_script = project.path().join("sandbox-launch");
        let launcher = PathBuf::from("/bin/sh");
        fs::write(
            &launcher_script,
            r#"printf '%s\n' "$0" "$@" > "$AFT_TEST_LAUNCH_ARGS.tmp"
mv -f "$AFT_TEST_LAUNCH_ARGS.tmp" "$AFT_TEST_LAUNCH_ARGS"
[ "$1" = "--profile-json" ] || exit 92
[ -n "$2" ] || exit 93
[ "$3" = "--" ] || exit 94
shift 3
exec "$@"
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&launcher_script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&launcher_script, permissions).unwrap();

        let profile = SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            storage.path().to_path_buf(),
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile, launcher.clone());
        let ctx = spawn_test_context(project.path(), storage.path());
        let request: RawRequest = serde_json::from_value(json!({
            "id": "sandbox-launcher",
            "command": "bash",
            "session_id": "sandbox-spawn-test",
            "params": {
                "command": "printf launcher-wrapped",
                "compressed": false,
                "env": { "AFT_TEST_LAUNCH_ARGS": args_log.to_string_lossy() },
            },
        }))
        .unwrap();

        let response = with_spawn_plan_for_test(plan, || handle(&request, &ctx));
        assert!(response.success, "launcher spawn failed: {response:?}");

        let started = Instant::now();
        while !args_log.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "fake launcher did not record its argv; response={response:?}; status={:?}",
                ctx.bash_background().status(
                    response.data["task_id"].as_str().unwrap(),
                    "sandbox-spawn-test",
                    None,
                    None,
                    4096,
                )
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let args = fs::read_to_string(&args_log).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert_eq!(lines[0], "sandbox-launch");
        assert_eq!(lines[1], "--profile-json");
        assert!(serde_json::from_str::<serde_json::Value>(lines[2]).is_ok());
        assert_eq!(lines[3], "--");
        assert!(lines.len() >= 6, "wrapped argv missing target: {lines:?}");

        stop_spawned_test_task(&ctx, &response);
    }

    #[cfg(unix)]
    #[test]
    fn launcher_init_failure_is_terminal_without_unsandboxed_retry() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        use crate::response_finalize::DispatchOutcome;
        use crate::sandbox_profile::SandboxProfile;
        use crate::sandbox_spawn::{with_spawn_plan_for_test, SpawnPlan};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let launcher_script = project.path().join("sandbox-launch");
        let launcher = PathBuf::from("/bin/sh");
        let launcher_spawns = project.path().join("launcher-spawns");
        let command_marker = project.path().join("command-must-not-run");
        fs::write(
            &launcher_script,
            format!(
                "printf 'spawn\\n' >> '{}'\necho 'sandbox_unavailable: test backend failed' >&2\nexit 78\n",
                launcher_spawns.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&launcher_script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&launcher_script, permissions).unwrap();

        let profile = SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            storage.path().to_path_buf(),
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile, launcher);
        let ctx = spawn_test_context(project.path(), storage.path());
        let mut request = spawn_test_request(
            "sandbox-unavailable",
            &format!("printf command-ran > '{}'", command_marker.display()),
            false,
        );
        request.params["params"]["foreground_orchestrate"] = json!(true);

        let spawn_response = with_spawn_plan_for_test(plan, || handle(&request, &ctx));
        assert!(
            spawn_response.success,
            "launcher spawn failed: {spawn_response:?}"
        );
        let task_id = spawn_response.data["task_id"].as_str().unwrap().to_string();
        let outcome =
            crate::commands::bash_orchestrate::build_bash_outcome(&request, &ctx, spawn_response);
        let terminal = match outcome {
            DispatchOutcome::Immediate(response) => response,
            DispatchOutcome::Deferred(mut pending) => {
                let started = Instant::now();
                loop {
                    if let Some(response) = (pending.poll)(&ctx) {
                        break response;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(20),
                        "sandbox launcher failure did not terminate; status={:?}",
                        ctx.bash_background().status(
                            &task_id,
                            "sandbox-spawn-test",
                            None,
                            None,
                            4096,
                        )
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        assert!(!terminal.success, "terminal response: {terminal:?}");
        assert_eq!(terminal.data["code"], "sandbox_unavailable");
        assert_eq!(
            terminal.data["exit_code"],
            crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE
        );

        std::thread::sleep(Duration::from_millis(650));
        let status_request: RawRequest = serde_json::from_value(json!({
            "id": "sandbox-unavailable-status",
            "command": "bash_status",
            "session_id": "sandbox-spawn-test",
            "params": { "task_id": task_id },
        }))
        .unwrap();
        let status = crate::commands::bash_status::handle(&status_request, &ctx);
        assert!(!status.success, "status response: {status:?}");
        assert_eq!(status.data["code"], "sandbox_unavailable");
        assert_eq!(
            status.data["exit_code"],
            crate::sandbox_spawn::SANDBOX_UNAVAILABLE_EXIT_CODE
        );
        assert_eq!(
            fs::read_to_string(&launcher_spawns)
                .unwrap()
                .lines()
                .count(),
            1,
            "sandbox launcher must be spawned exactly once"
        );
        assert!(
            !command_marker.exists(),
            "sandbox failure must never retry the command unsandboxed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_retry_reclassified_as_first_party_resolves_native_plan() {
        let _env_lock = crate::test_env::process_env_lock();
        use crate::sandbox_spawn::{
            clear_sandbox_spawn_test_seam, install_sandbox_spawn_test_seam,
            sandbox_spawn_test_observations, with_authenticated_principal, AuthenticatedPrincipal,
            PrincipalTrust, RequestedSandboxTier,
        };

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.enabled = true;
            config.bash_permissions = true;
        });
        install_sandbox_spawn_test_seam(project.path().to_path_buf());

        // Native first-party scans are report-only, so use an untrusted route
        // for the initial prompt and then exercise the adverse case where the
        // retry is reclassified as first-party under the same enabled policy.
        let mut request = spawn_test_request("permission-retry", "git status --short", false);
        request.params["params"]["permissions_requested"] = json!(true);
        let untrusted = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 9,
            route_epoch: 4,
            project_root: project.path().to_path_buf(),
            harness: "mcp:test".to_string(),
            session_id: "sandbox-spawn-test".to_string(),
            principal_id: Some("unverified".to_string()),
        };
        let permission_required =
            with_authenticated_principal(untrusted, || handle(&request, &ctx));
        assert!(!permission_required.success);
        assert_eq!(permission_required.data["code"], ERROR_PERMISSION_REQUIRED);

        let mut grants = Vec::new();
        for ask in permission_required.data["asks"].as_array().unwrap() {
            let candidates = ask["always"]
                .as_array()
                .filter(|values| !values.is_empty())
                .or_else(|| ask["patterns"].as_array())
                .unwrap();
            grants.extend(
                candidates
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string),
            );
        }
        assert!(!grants.is_empty());
        assert!(grants.iter().all(|grant| !grant.starts_with("esc_")));
        request.params["params"]["permissions_granted"] = json!(grants);

        let retried = handle(&request, &ctx);
        assert!(retried.success, "permission retry failed: {retried:?}");
        let task_id = retried.data["task_id"].as_str().unwrap();
        let snapshot = ctx
            .bash_background()
            .status(
                task_id,
                "sandbox-spawn-test",
                Some(project.path()),
                Some(storage.path()),
                4096,
            )
            .expect("retried sandbox task status");
        assert!(
            snapshot.sandbox_native,
            "permission grants must not select an unsandboxed spawn plan"
        );

        let observations = sandbox_spawn_test_observations(project.path());
        clear_sandbox_spawn_test_seam(project.path());
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].requested_tier, RequestedSandboxTier::Native);
        stop_spawned_test_task(&ctx, &retried);
    }
}
