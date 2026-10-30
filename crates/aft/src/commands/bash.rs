use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// Run the whole line on the remote runner this demand names.
    #[serde(default)]
    runon: Option<String>,
    /// With `runon`: files the runner copies back, relative to `workdir`.
    #[serde(default)]
    return_paths: Vec<String>,
}

/// The hard kill for a bash request: the caller's explicit `timeout`, or the
/// registry's default. Every session's blocking call gets the default
/// like every other call; its wait is capped by `bash.worker_wait_max_ms`
/// instead (see `bash_orchestrate::blocking_wait_cap_ms`), and its waits extend
/// the default kill while it keeps waiting.
fn hard_kill_for(params: &BashParams) -> crate::bash_background::HardKill {
    crate::bash_background::HardKill::from_timeout_ms(params.timeout)
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let launch_started = std::time::Instant::now();
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
    let secret_labels = crate::bash_background::secrets::labels(&params.command);
    let has_secrets = !secret_labels.is_empty();
    // Remote runs are resolved later, but whether `bash.env` travels with the
    // job depends only on the request, so decide it up front: a `runon` job
    // receives no configured value unless the user listed the name in
    // `bash.runon_env`, and it never receives a project value.
    let remote_requested = params.runon.is_some()
        || crate::bash_background::remote_for_legacy_command(
            &ctx.config(),
            &params.command,
            params.pty,
            params.shell.is_powershell(),
        )
        .is_some();
    let _runon_timings =
        remote_requested.then(|| crate::bash_background::RunonTimingScope::start(launch_started));
    if has_secrets && remote_requested {
        return crate::bash_background::secrets::refusal(
            &req.id,
            &secret_labels,
            "secrets_remote_refused",
            crate::bash_background::secrets::REMOTE_REFUSED,
        );
    }
    // Host escalation can never protect the daemon's memory, even if approved.
    if has_secrets && (cfg!(windows) || params.sandbox.is_some()) {
        return crate::bash_background::secrets::refusal(
            &req.id,
            &secret_labels,
            "secrets_sandbox_required",
            crate::bash_background::secrets::SANDBOX_REQUIRED,
        );
    }
    #[cfg(unix)]
    let secret_preflight = if has_secrets {
        match crate::bash_background::secrets::preflight(
            ctx,
            req.session(),
            params.pty,
            params.background,
        ) {
            Ok(preflight) => Some(preflight),
            Err(()) => {
                return crate::bash_background::secrets::refusal(
                    &req.id,
                    &secret_labels,
                    "secrets_sandbox_required",
                    crate::bash_background::secrets::SANDBOX_REQUIRED,
                )
            }
        }
    } else {
        None
    };
    // A project's `bash.env` applies only while the project root is trusted at
    // the time of the call, so trusting or untrusting takes effect on the next
    // command without a reload.
    crate::bash_background::enter_runon_phase("request_environment");
    let project_trusted = project_env_trusted(ctx);
    let remote_env_plan = if remote_requested {
        crate::commands::bash_remote_env::plan(ctx.config().as_ref())
    } else {
        crate::commands::bash_remote_env::RemoteEnvPlan::default()
    };
    let mut applied_env = Vec::new();
    // Every configured name, including one a remote job does receive: the
    // remote request drops them all from its shell environment, so a value is
    // sent only when `bash.runon_env` puts it in the request map explicitly.
    let configured_env_names = ctx
        .config()
        .bash
        .configured_env(true)
        .iter()
        .flat_map(|configured| configured.names.keys().cloned())
        .collect::<Vec<_>>();
    for configured in ctx.config().bash.configured_env(project_trusted) {
        let names = if remote_requested {
            crate::commands::bash_remote_env::forwarded_names(&remote_env_plan, &configured)
        } else {
            configured.names
        };
        // AFT's worker test-thread budget is written by `inject` below, which
        // keeps a value the request already carries; configured thread counts
        // must not reach it, and a value rejected at load could not apply
        // anyway. The disclosure still names them, so nothing is silent.
        let names = names
            .into_iter()
            .filter(|(key, _)| !crate::agent_child_env::is_worker_test_thread_env_name(key))
            .collect::<std::collections::BTreeMap<_, _>>();
        // The request map is the process-creation channel, with call-specific
        // values taking precedence over the configured stable defaults.
        for (key, value) in &names {
            params
                .env
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        applied_env.push(crate::config_resolve::ConfiguredBashEnv {
            names,
            tier: configured.tier,
        });
    }
    let disclosure = crate::commands::bash_remote_env::Disclosure::build(
        &applied_env,
        &ctx.config().bash.shadowed_project_env(project_trusted),
        !project_trusted && !ctx.config().bash.project_env.is_empty(),
        &remote_env_plan,
    );
    if has_secrets && crate::bash_background::secrets::env_conflicts(&params.env) {
        return crate::bash_background::secrets::refusal(
            &req.id,
            &secret_labels,
            "secrets_env_conflict",
            crate::bash_background::secrets::ENV_CONFLICT,
        );
    }
    // The dedicated external route is defensive as well as convenient: callers
    // cannot accidentally execute Bash merely by omitting the selector.
    if req.command == "powershell" {
        params.shell = crate::bash_background::BashShell::Powershell;
    }

    // Reject before permissions, rewrites, or process creation. The worker
    // flag is resolved by the caller's session context, not command text.
    if req.worker_session()
        && !params.shell.is_powershell()
        && crate::bash_permissions::synthetic_load::is_synthetic_load(&params.command)
    {
        return Response::error(
            &req.id,
            "synthetic_load_refused",
            "synthetic CPU load is not allowed on shared machines, local or remote; prove the mechanism deterministically instead",
        );
    }

    if let Some(description) = params.description.as_deref() {
        log::debug!("bash description: {description}");
    }

    // `runon` sends the whole line, as written, to the remote runner. Every
    // reason that cannot happen is refused here, before any local work (even
    // resolving a local shell).
    crate::bash_background::enter_runon_phase("routing_validation");
    let mut remote = match params.runon.as_deref() {
        // Placeholder routing was decided before sandbox checks. A live config
        // change must not turn a locally admitted launch into a remote job.
        None if has_secrets => None,
        None => crate::bash_background::remote_for_legacy_command(
            &ctx.config(),
            &params.command,
            params.pty,
            params.shell.is_powershell(),
        ),
        Some(runon) => match crate::bash_background::remote_for_runon(
            &ctx.config(),
            runon,
            params.pty,
            params.shell.is_powershell(),
            matches!(params.sandbox, Some(BashSandbox::Host)),
        ) {
            Ok(launch) => Some(launch),
            Err(message) => {
                return Response::error(
                    &req.id,
                    "remote_run_refused",
                    format!("runon refused: {message}"),
                );
            }
        },
    };

    if let Some(launch) = remote.as_mut() {
        launch.configured_env_names = configured_env_names;
    }
    let shell_path = match crate::bash_background::resolve_shell_path(params.pty, params.shell) {
        Ok(path) => path,
        // The only refusal here is a missing PowerShell. Rebuild its message so
        // it names the command tool by the name this request's host gave it.
        Err(_) if params.shell.is_powershell() => {
            return Response::error(
                &req.id,
                "powershell_not_installed",
                crate::bash_background::powershell_missing_message(req.host_tool_names().shell()),
            )
        }
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

    crate::bash_background::enter_runon_phase("workdir_return_paths");
    let workdir = resolve_workdir(params.workdir.as_deref(), ctx);
    let mut remote = remote;
    if let Err(response) = attach_return_paths(&req.id, ctx, &params, &workdir, &mut remote) {
        return response;
    }
    let duplicate_workdir = workdir.clone();
    let duplicate_project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| default_workdir(ctx));
    crate::bash_background::enter_runon_phase("permission_preflight");
    let principal = crate::sandbox_spawn::current_authenticated_principal();
    let host_requested = matches!(params.sandbox, Some(BashSandbox::Host));
    if host_requested && !crate::sandbox_spawn::principal_is_first_party(&principal) {
        return Response::error(
            &req.id,
            "sandbox_escalation_denied",
            "sandbox host escalation is unavailable to untrusted principals",
        );
    }
    // A jailed worker has no host prompt to approve an escalation with, and its
    // shells never leave the sandbox, so refuse before minting a grant.
    if host_requested && crate::sandbox_spawn::worker_jail_active() {
        return Response::error(
            &req.id,
            "sandbox_escalation_denied",
            "sandbox host escalation is unavailable to worker shells, which always run in the native sandbox",
        );
    }
    if host_requested && crate::bash_background::worker_preset_active() {
        let bound_root = ctx
            .config()
            .project_root
            .clone()
            .unwrap_or_else(|| workdir.clone());
        crate::agent_child_env::inject_worker_test_threads(&bound_root, &mut params.env);
    }

    #[cfg(unix)]
    let mut spawn_workdir = Some(workdir.clone());
    #[cfg(not(unix))]
    let spawn_workdir = Some(workdir.clone());
    #[cfg(unix)]
    let mut host_escalation: Option<crate::sandbox_spawn::HostEscalationAttempt> = None;
    #[cfg(not(unix))]
    let host_escalation: Option<crate::sandbox_spawn::HostEscalationAttempt> = None;
    #[cfg(unix)]
    if host_requested && crate::sandbox_spawn::sandbox_required(ctx) {
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
        let cwd = match std::fs::canonicalize(&workdir) {
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
    // Only first-party strict requests may defer approval until the resolved
    // spawn plan proves containment. Relaxed denies protect only selected paths
    // (and are unenforced on Linux), so relaxed must ask just like off.
    let defer_permission_asks = crate::sandbox_spawn::native_sandbox_enforced(ctx, &principal)
        && crate::sandbox_spawn::effective_sandbox_mode(ctx, &principal)
            == crate::config::SandboxMode::Strict
        && !crate::sandbox_spawn::principal_requires_strict(&principal)
        && host_escalation.is_none();
    // The bash scanner parses POSIX grammar and must not treat PowerShell
    // syntax as understood. PowerShell therefore takes an explicit, per-command
    // approval path instead of a potentially unsafe rewrite or auto-allow.
    let permission_asks = if params.shell.is_powershell() {
        conservative_powershell_permission_asks(&params.command)
    } else if defer_permission_asks || params.permissions_requested || ctx.config().bash_permissions
    {
        crate::bash_permissions::scan::scan_with_cwd_for_session(
            &params.command,
            ctx,
            &workdir,
            req.session(),
        )
    } else {
        Vec::new()
    };
    let scanner_requires_approval = !permission_asks.is_empty()
        && !permissions_granted_cover(&permission_asks, &params.permissions_granted);
    if !defer_permission_asks && scanner_requires_approval {
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
    if !has_secrets
        && !params.shell.is_powershell()
        && ctx.config().bash.changed_files == crate::config::BashChangedFiles::Off
        && host_escalation.is_none()
        && remote.is_none()
        && params.env.is_empty()
        && workdir_matches_project_root(&workdir, ctx)
    {
        if let Some(response) = crate::bash_rewrite::try_rewrite_for_request(
            &params.command,
            &req.id,
            req.session_id.as_deref(),
            &req.host_tool_names(),
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
        } else if remote.is_some() {
            "runon sends the whole line to the remote runner"
        } else if host_escalation.is_some() {
            "host escalation owns process execution"
        } else if !params.env.is_empty() {
            "environment overrides require shell process execution"
        } else {
            "bash workdir differs from the project root"
        };
        let branch = if params.shell.is_powershell()
            || host_escalation.is_some()
            || remote.is_some()
            || !params.env.is_empty()
        {
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
    let scanner_report = defer_permission_asks
        .then_some(permission_asks)
        .unwrap_or_default();
    let duplicate = ctx.bash_background().find_running_duplicate(
        req.session(),
        &execution_command,
        &duplicate_project_root,
        &duplicate_workdir,
    );
    #[cfg(unix)]
    let secret_launch = match secret_preflight {
        Some(preflight) => match crate::bash_background::secrets::authorize(
            &req.id,
            &secret_labels,
            preflight,
            &params.env,
        ) {
            Ok(launch) => Some(launch),
            Err(response) => return response,
        },
        None => None,
    };
    let mut response = crate::bash_background::spawn_with_secrets(
        &req.id,
        req.session(),
        &execution_command,
        params.shell,
        shell_path,
        workdir,
        env,
        hard_kill_for(&params),
        ctx,
        effective_background,
        params.notify_on_completion,
        params.compressed,
        params.pty,
        pty_rows,
        pty_cols,
        scanner_report,
        scanner_requires_approval,
        host_escalation,
        remote,
        #[cfg(unix)]
        secret_launch,
    );
    if response.success {
        let object = response
            .data
            .as_object_mut()
            .expect("successful bash spawn response is an object");
        if let Some(duplicate) = duplicate {
            object.insert(
                "_bash_duplicate_warning".to_string(),
                json!(format_duplicate_warning(&duplicate)),
            );
        }
        if !disclosure.is_empty()
            && ctx
                .bash_background()
                .claim_bash_env_disclosure(req.session())
        {
            object.insert("_bash_env_keys".to_string(), json!(disclosure));
        }
    }
    response
}

fn format_duplicate_warning(
    task: &crate::bash_background::registry::DuplicateRunningTask,
) -> String {
    let age = format_duplicate_task_age(task.age);
    let session = if task.same_session {
        "same session"
    } else {
        "another session"
    };
    format!(
        "Warning: identical command already running as task {} (age {age}; {session}).",
        task.task_id
    )
}

fn format_duplicate_task_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m{:02}s ago", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m ago", seconds / 3600, (seconds % 3600) / 60),
    }
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

/// Whether the session's project root is on the filter trust list right now. A
/// linked worktree inherits its main checkout's trust.
fn project_env_trusted(ctx: &AppContext) -> bool {
    let Some(root) = ctx.config().project_root.clone() else {
        return false;
    };
    // `aft doctor filters trust` writes the trust file under the configured
    // storage directory; a routed session's task storage sits one segment
    // deeper, so both are checked and the user's decision is found either way.
    let staged = crate::bash_background::task_storage_dir(ctx);
    if crate::compress::trust::is_project_env_trusted(Some(&staged), &root) {
        return true;
    }
    ctx.config()
        .storage_dir
        .as_ref()
        .filter(|configured| **configured != staged)
        .is_some_and(|configured| {
            crate::compress::trust::is_project_env_trusted(Some(configured), &root)
        })
}

fn blocked_env_var(env: &HashMap<String, String>) -> Option<&str> {
    crate::bash_env::first_blocked_env_key(env)
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

    asks.iter().all(|ask| match ask.kind {
        crate::bash_permissions::PermissionKind::ExternalDirectory => {
            !ask.targets.is_empty()
                && ask.targets.iter().all(|target| {
                    granted
                        .iter()
                        .any(|grant| external_approval_covers(grant, target))
                })
        }
        crate::bash_permissions::PermissionKind::Bash => ask
            .patterns
            .iter()
            .chain(ask.always.iter())
            .any(|pattern| granted.iter().any(|grant| grant == pattern)),
    })
}

fn external_approval_covers(granted: &str, target: &crate::access_policy::AccessGrant) -> bool {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    // Compare Windows identities without rewriting host patterns or stored targets.
    let path = Path::new(granted);
    #[cfg(windows)]
    let normalized_grant = crate::windows_path::normalize_windows_path(path);
    #[cfg(windows)]
    let path = normalized_grant.as_path();
    let target_path = target.path.as_path();
    #[cfg(windows)]
    let normalized_target = crate::windows_path::normalize_windows_path(target_path);
    #[cfg(windows)]
    let target_path = normalized_target.as_path();
    let (path, kind) = if path.file_name().is_some_and(|name| name == "*") {
        let Some(parent) = path.parent() else {
            return false;
        };
        (parent, GrantKind::Subtree)
    } else {
        (path, GrantKind::File)
    };
    path.is_absolute()
        && AccessGrant {
            path: path.to_path_buf(),
            kind,
            operation: AccessOperation::Read,
        }
        .covers_kind(AccessOperation::Read, target_path, target.kind)
}

fn conservative_powershell_permission_asks(
    command: &str,
) -> Vec<crate::bash_permissions::PermissionAsk> {
    vec![crate::bash_permissions::PermissionAsk {
        kind: crate::bash_permissions::PermissionKind::Bash,
        patterns: vec![command.to_string()],
        always: Vec::new(),
        reason: None,
        targets: Vec::new(),
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

/// Check `return_paths` and hand them to the remote launch, before anything
/// runs. They need `runon`: only a remote run has files to copy back. Each
/// path must name a file inside the project, checked exactly as `write`
/// checks its path, including symlinks that lead out of it.
fn attach_return_paths(
    req_id: &str,
    ctx: &AppContext,
    params: &BashParams,
    workdir: &Path,
    remote: &mut Option<crate::bash_background::RemoteLaunch>,
) -> Result<(), Response> {
    use crate::bash_background::registry::return_files::{
        check_requested_paths, resolve_destination, NotReturned,
    };
    if params.return_paths.is_empty() {
        return Ok(());
    }
    let refused = |message: String| Response::error(req_id, "return_paths_refused", message);
    let Some(launch) = remote.as_mut().filter(|_| params.runon.is_some()) else {
        return Err(refused(
            "return_paths refused: return_paths needs runon; a local command already writes its files here"
                .into(),
        ));
    };
    // ck-motor's Windows runners do not copy files back, and the demand
    // already names the platform, so refuse before anything is submitted.
    if launch.requested_platform == Some(crate::exec_remote::types::Platform::Windows) {
        return Err(refused(
            "return_paths refused: return_paths is not supported on Windows runners; the command did not run"
                .into(),
        ));
    }
    check_requested_paths(&params.return_paths).map_err(|message| {
        refused(format!(
            "return_paths refused: {message}; the command did not run"
        ))
    })?;
    // The files are written into the project whether or not this session
    // restricts other tools to it, so containment is checked here too.
    let root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| workdir.to_path_buf());
    let root = std::fs::canonicalize(&root).map_err(|error| {
        refused(format!(
            "return_paths refused: cannot resolve the project root {}: {error}",
            root.display()
        ))
    })?;
    for path in &params.return_paths {
        let shown = path.escape_default();
        if let Err(response) = ctx.validate_path(req_id, &workdir.join(path)) {
            let reason = response.data["message"]
                .as_str()
                .unwrap_or("the path is not allowed")
                .to_string();
            return Err(refused(format!(
                "return_paths refused: {shown}: {reason}; the command did not run"
            )));
        }
        match resolve_destination(&root, &workdir.join(path)) {
            Ok(_) => {}
            Err(NotReturned::LocalWrite { error }) => {
                return Err(refused(format!(
                    "return_paths refused: {shown}: {error}; the command did not run"
                )))
            }
            Err(_) => {
                return Err(refused(format!(
                    "return_paths refused: {shown} resolves outside the project; the command did not run"
                )))
            }
        }
    }
    launch.return_paths = params.return_paths.clone();
    Ok(())
}

fn resolve_workdir(workdir: Option<&Path>, ctx: &AppContext) -> PathBuf {
    let root = default_workdir(ctx);
    match workdir {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        // Resolve once so permissions, rewriting, escalation, and the child
        // all use the project-relative directory, not the daemon's cwd.
        Some(path) => root.join(path),
        None => root,
    }
}

/// Inject spawn outcomes into the same piped retry policy used on Windows.
/// This adapter preserves the existing test diagnostics; it has no retry loop.
#[cfg(test)]
fn try_spawn_with_fallback<C, F>(
    candidates: &[crate::windows_shell::WindowsShell],
    mut try_one: F,
) -> Result<C, String>
where
    F: FnMut(&crate::windows_shell::WindowsShell) -> std::io::Result<C>,
{
    use crate::windows_shell::{spawn_piped_candidates, CandidateFailure, PipedAttemptError};
    match spawn_piped_candidates(candidates, |shell, _| {
        try_one(shell).map_err(PipedAttemptError::Spawn)
    }) {
        Ok((child, _, _)) => Ok(child),
        Err(CandidateFailure::Stopped {
            index,
            error: PipedAttemptError::Spawn(error),
        }) => Err(format!(
            "failed to spawn bash command via {}: {error}",
            candidates[index].binary()
        )),
        Err(CandidateFailure::Exhausted { last }) => {
            let last_error = match last {
                Some((index, PipedAttemptError::Spawn(error))) => {
                    format!("{}: {error}", candidates[index].binary())
                }
                _ => "no candidates were attempted".into(),
            };
            Err(format!("failed to spawn bash command: no Windows shell could be spawned. Last error: {last_error}. PATH-probed candidates: {:?}",
                candidates.iter().map(|shell| shell.binary()).collect::<Vec<_>>()))
        }
        Err(CandidateFailure::Stopped {
            error: PipedAttemptError::Setup(message),
            ..
        }) => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::windows_shell::WindowsShell;

    #[test]
    fn standalone_runon_is_refused_by_name_even_with_remote_execution_enabled() {
        for enabled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let mut config = crate::config::Config::default();
            config.project_root = Some(root.path().into());
            config.storage_dir = Some(storage.path().into());
            config.remote_exec.enabled = enabled;
            config.bash.runon_enabled = true;
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
            let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
            let marker = root.path().join("must-not-run-locally");
            let req = RawRequest {
                id: "standalone-runon".into(),
                command: "bash".into(),
                lsp_hints: None,
                session_id: Some("standalone".into()),
                params: serde_json::json!({
                    "command": format!("echo SHOULD_NOT_RUN > \"{}\"", marker.display()),
                    "runon": "linux"
                }),
            };
            let response = handle(&req, &ctx);
            assert!(!response.success, "{response:?}");
            assert_eq!(response.data["code"], "remote_run_refused");
            assert!(
                response.data["message"].as_str().unwrap().contains("runon"),
                "{response:?}"
            );
            assert!(!marker.exists(), "unsupported runon must never run locally");
        }
    }

    fn ctx_with_root(root: &std::path::Path) -> AppContext {
        AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                ..crate::config::Config::default()
            },
        )
    }

    #[cfg(unix)]
    #[test]
    fn external_file_approval_survives_scope_probe_timeout() {
        let project = tempfile::tempdir().unwrap();
        // System temporary paths are exempt from external-directory asks, so
        // use a distinct non-temporary parent to exercise the real scanner.
        let scratch = std::env::current_dir()
            .unwrap()
            .join("target/approval-scope-tests");
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = tempfile::tempdir_in(&scratch).unwrap();
        let parent = crate::access_policy::policy_path(outside.path());
        let file = parent.join("file.txt");
        let command = format!("cat \"{}\"", file.display());
        let stall = crate::bounded_io::stall_helper(&parent);
        let first = crate::bash_permissions::scan::scan_with_project_root(
            &command,
            project.path(),
            project.path(),
        );
        stall
            .reached
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let external = first
            .iter()
            .find(|ask| {
                matches!(
                    ask.kind,
                    crate::bash_permissions::PermissionKind::ExternalDirectory
                )
            })
            .unwrap();
        assert_eq!(external.patterns, [file.to_string_lossy()]);
        let granted: Vec<String> = first
            .iter()
            .flat_map(|ask| {
                if ask.always.is_empty() {
                    ask.patterns.clone()
                } else {
                    ask.always.clone()
                }
            })
            .collect();
        drop(stall);
        let retry = crate::bash_permissions::scan::scan_with_project_root(
            &command,
            project.path(),
            project.path(),
        );
        let external = retry
            .iter()
            .find(|ask| {
                matches!(
                    ask.kind,
                    crate::bash_permissions::PermissionKind::ExternalDirectory
                )
            })
            .unwrap();
        assert_eq!(external.patterns, [format!("{}/*", parent.display())]);
        assert!(
            permissions_granted_cover(&retry, &granted),
            "the approved file must not ask again after its folder becomes eligible"
        );

        let sibling = parent.join("sibling.txt");
        let batch = crate::bash_permissions::scan::scan_with_project_root(
            &format!("cat \"{}\" \"{}\"", file.display(), sibling.display()),
            project.path(),
            project.path(),
        );
        let external: Vec<_> = batch
            .iter()
            .filter(|ask| {
                matches!(
                    ask.kind,
                    crate::bash_permissions::PermissionKind::ExternalDirectory
                )
            })
            .collect();
        assert_eq!(
            external.len(),
            1,
            "the displayed folder asks are deduplicated"
        );
        assert_eq!(external[0].targets.len(), 2);
        assert!(
            !permissions_granted_cover(&batch, &granted),
            "one approved file cannot cover a deduplicated sibling"
        );
        let mut both = granted;
        both.push(sibling.to_string_lossy().into_owned());
        assert!(permissions_granted_cover(&batch, &both));
    }

    #[test]
    fn bash_permissions_roundtrip_unknown_cwd_requires_exact_command() {
        use crate::bash_permissions::{scan::scan_with_project_root, PermissionKind};
        let project = tempfile::tempdir().unwrap();
        for command in [
            "cd \"$DEST\" && rm -rf build",
            "mkdir -p \"$DEST\" && rsync -a source/ \"$DEST/\" && cd \"$DEST\" && shasum -c checksums",
            "mkdir -p \"$DEST\" && rsync -a source/ \"$DEST/\" && cd \"$DEST\" && cat checksums",
            "cd \"$DEST\" && cat < checksums",
            "pushd \"$DEST\" && cp -R source/ copy/",
        ] {
            let first = scan_with_project_root(command, project.path(), project.path());
            let grants: Vec<_> = first.iter().flat_map(|ask| {
                if ask.always.is_empty() { &ask.patterns } else { &ask.always }
            }).cloned().collect();
            let retry = scan_with_project_root(command, project.path(), project.path());
            assert!(permissions_granted_cover(&retry, &grants),
                "command={command}, grants={grants:?}, asks={retry:?}");
            let exact = retry.iter().find(|ask| ask.reason.as_deref() == Some(crate::bash_permissions::scan::UNKNOWN_CWD_PERMISSION_REASON))
                .expect("the unknown cwd requires exact-command approval");
            assert_eq!(exact.kind, PermissionKind::Bash);
            assert_eq!(exact.patterns, [command]);
            assert!(exact.always.is_empty(), "unknown cwd must not offer a folder or reusable command grant");
            assert!(!permissions_granted_cover(&retry, &[command.into()]),
                "exact-command approval cannot replace individual command approvals");
            assert!(!permissions_granted_cover(&retry, &["build".into(), "rm *".into(), "*".into()]),
                "relative paths and reusable commands cannot approve an unknown cwd");
        }
    }

    #[test]
    fn bash_permissions_roundtrip_external_copy_shapes() {
        use crate::bash_permissions::scan::scan_with_project_root;

        let scratch = std::env::current_dir()
            .unwrap()
            .join("target/bash-roundtrip");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let project = fixture.path().join("project");
        let source = fixture.path().join("source");
        let worker = fixture.path().join("worker/.cortexkit/internal-docs/copy");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        #[cfg(unix)]
        let destinations = {
            let alias = fixture.path().join("home-alias");
            std::os::unix::fs::symlink(fixture.path(), &alias).unwrap();
            vec![worker, alias.join("worker/.cortexkit/internal-docs/copy")]
        };
        #[cfg(not(unix))]
        let destinations = vec![worker];
        for exists in [false, true] {
            if exists {
                std::fs::create_dir_all(&destinations[0]).unwrap();
            }
            for destination in &destinations {
                let src = source.display();
                let dst = destination.display();
                let commands = [
                    format!("mkdir -p '{dst}'"),
                    format!("rsync -a '{src}' '{dst}'"),
                    format!("rsync -a '{src}/' '{dst}'"),
                    format!("rsync -a '{src}' '{dst}/'"),
                    format!("rsync -a '{src}/' '{dst}/'"),
                    format!("cp -R '{src}/' '{dst}/'"),
                    format!("cd '{dst}' && cat checksums"),
                    format!("mkdir -p '{dst}' && rsync -a '{src}/' '{dst}/' && cd '{dst}' && shasum -c checksums"),
                    format!("mkdir -p '{dst}' && cp -R '{src}/' '{dst}/' && cd '{dst}' && cat checksums"),
                ];
                for command in commands {
                    let first = scan_with_project_root(&command, &project, &project);
                    assert!(!first.is_empty(), "{command}");
                    for always in [false, true] {
                        let grants: Vec<_> = first
                            .iter()
                            .flat_map(|ask| {
                                if always && !ask.always.is_empty() {
                                    &ask.always
                                } else {
                                    &ask.patterns
                                }
                            })
                            .cloned()
                            .collect();
                        let retry = scan_with_project_root(&command, &project, &project);
                        assert!(permissions_granted_cover(&retry, &grants),
                            "command={command}, exists={exists}, always={always}, grants={grants:?}, asks={retry:?}");
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn bash_permissions_roundtrip_readonly_corpus_in_worktree() {
        use crate::access_policy::{AccessOperation, GrantKind};
        use crate::bash_permissions::{scan::scan_with_project_root, PermissionKind};

        let scratch = std::env::current_dir()
            .unwrap()
            .join("target/bash-roundtrip");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let project = fixture.path().join("project");
        let worktree = project.join("worker");
        let source = fixture.path().join("specimens/two-pass-private-20261025");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SHA256SUMS"), "").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args([
                    "-c",
                    "user.name=AFT test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init"]);
        git(&["commit", "--allow-empty", "-m", "fixture"]);
        git(&["worktree", "add", "--detach", worktree.to_str().unwrap()]);
        assert!(worktree.join(".git").is_file());
        assert!(std::fs::canonicalize(&worktree)
            .unwrap()
            .starts_with(std::fs::canonicalize(&project).unwrap()));

        // Keep the variable directory changes and nested status pipeline while
        // using a literal fixture source instead of the user's private corpus.
        let copy = format!("cp -R '{}/.' $W/corpus-readonly/", source.display());
        let command = format!(
            "W='{}'; mkdir -p $W/corpus-readonly && {copy} && chmod -R a-w $W/corpus-readonly && cd $W/corpus-readonly && shasum -a 256 -c SHA256SUMS --quiet && echo ok; cd $W && echo \"status lines: $(git status --short | wc -l)\"",
            worktree.display(),
        );
        let first = scan_with_project_root(&command, &project, &project);
        let canonical_source = std::fs::canonicalize(&source).unwrap();
        assert!(!canonical_source.starts_with(std::fs::canonicalize(&project).unwrap()));
        let source_pattern = format!("{}/*", canonical_source.display());
        assert!(
            first
                .iter()
                .any(|ask| ask.kind == PermissionKind::ExternalDirectory
                    && ask.patterns == [source_pattern.as_str()]
                    && ask.always == [source_pattern.as_str()]
                    && ask
                        .targets
                        .iter()
                        .any(|target| target.path == canonical_source
                            && target.kind == GrantKind::Subtree
                            && target.operation == AccessOperation::Read
                            && target.covers(
                                AccessOperation::Read,
                                &canonical_source.join("SHA256SUMS")
                            ))),
            "the copy source must have an external-directory read ask: {first:?}"
        );
        for resource in [
            "mkdir -p $W/corpus-readonly",
            copy.as_str(),
            "chmod -R a-w $W/corpus-readonly",
            "shasum -a 256 -c SHA256SUMS --quiet",
            "echo ok",
            "echo \"status lines: $(git status --short | wc -l)\"",
            "git status --short",
            "wc -l",
        ] {
            assert!(
                first.iter().any(|ask| ask.kind == PermissionKind::Bash
                    && ask.patterns == [resource]
                    && ask.reason.is_none()),
                "individual command resource missing: {resource}; asks={first:?}"
            );
        }
        assert!(first.iter().any(|ask| ask.kind == PermissionKind::Bash
            && ask.patterns == [command.as_str()]
            && ask.always.is_empty()
            && ask.reason.as_deref()
                == Some(crate::bash_permissions::scan::UNKNOWN_CWD_PERMISSION_REASON)));

        for prefer_always in [false, true] {
            let mut grants = Vec::new();
            let mut retry = first.clone();
            let mut rounds = 0;
            while !permissions_granted_cover(&retry, &grants) {
                assert!(
                    rounds < 2,
                    "permission retry did not settle: {retry:?}; grants={grants:?}"
                );
                for ask in &retry {
                    let approved = if prefer_always && !ask.always.is_empty() {
                        &ask.always
                    } else {
                        &ask.patterns
                    };
                    for grant in approved {
                        if !grants.contains(grant) {
                            grants.push(grant.clone());
                        }
                    }
                }
                rounds += 1;
                retry = scan_with_project_root(&command, &project, &project);
            }
            assert_eq!(rounds, 1, "the approved set must cover the first retry");
            assert!(permissions_granted_cover(&retry, &grants));
        }
    }

    fn assert_bash_permission_roundtrip(
        command: &str,
        project: &Path,
    ) -> Vec<crate::bash_permissions::PermissionAsk> {
        let first =
            crate::bash_permissions::scan::scan_with_project_root(command, project, project);
        assert!(!first.is_empty(), "the command must actually be scanned");
        for prefer_always in [false, true] {
            let mut grants = Vec::new();
            let mut retry = first.clone();
            let mut rounds = 0;
            while !permissions_granted_cover(&retry, &grants) {
                assert!(
                    rounds < 2,
                    "permission retry did not settle: {retry:?}; grants={grants:?}"
                );
                for ask in &retry {
                    let approved = if prefer_always && !ask.always.is_empty() {
                        &ask.always
                    } else {
                        &ask.patterns
                    };
                    for grant in approved {
                        if !grants.contains(grant) {
                            grants.push(grant.clone());
                        }
                    }
                }
                rounds += 1;
                retry = crate::bash_permissions::scan::scan_with_project_root(
                    command, project, project,
                );
            }
            assert_eq!(rounds, 1, "the granted set must cover the first retry");
        }
        first
    }

    #[cfg(unix)]
    #[test]
    fn bash_permissions_roundtrip_alf_quoted_gate() {
        use crate::bash_permissions::PermissionKind;
        const CHILD_HOME: &str = "AFT_ALF_PERMISSION_TEST_HOME";
        if let Some(home) = std::env::var_os(CHILD_HOME) {
            assert_eq!(std::env::var_os("HOME"), Some(home.clone()));
            let project = PathBuf::from(home).join("fixture-project");
            let command = include_str!("../../tests/fixtures/bash_permissions/alf_quoted_gate.sh");
            let asks = assert_bash_permission_roundtrip(command, &project);
            assert!(
                asks.iter().all(|ask| ask.kind == PermissionKind::Bash),
                "{asks:?}"
            );
            assert_eq!(
                asks.iter().filter(|ask| ask.reason.is_none()).count(),
                8,
                "heredoc commands must remain data: {asks:?}"
            );
            for resource in [
                "set -e",
                "git worktree add -q --detach $S/fixture-s14 HEAD",
                "git log --oneline -1",
                "chmod +x .gate-s14.sh",
                "chmod +x /tmp/run-s14.sh",
                "grep -n '^run \\|^wait\\|nats' /tmp/run-s14.sh",
            ] {
                assert!(
                    asks.iter()
                        .any(|ask| ask.patterns == [resource] && ask.reason.is_none()),
                    "outer command missing: {resource}; asks={asks:?}"
                );
            }
            assert!(
                asks.iter().any(|ask| ask.reason.is_none()
                    && (ask.patterns[0] == "cat" || ask.patterns[0].starts_with("cat "))
                    && ask.always == ["cat *"]),
                "outer cat resource missing: {asks:?}"
            );
            assert!(
                asks.iter().any(|ask| ask.patterns[0]
                    .starts_with("sed -e '/^run fixture-lanes-s2/d'")
                    && ask.always == ["sed *"]),
                "outer sed resource missing: {asks:?}"
            );
            assert!(asks.iter().any(|ask| ask.patterns == [command]
                && ask.always.is_empty()
                && ask.reason.as_deref()
                    == Some(crate::bash_permissions::scan::UNKNOWN_CWD_PERMISSION_REASON)));
            return;
        }

        // HOME belongs to the child only, so parallel tests cannot observe the
        // fixture's tilde expansion instead of their own environment.
        let scratch = std::env::current_dir()
            .unwrap()
            .join("target/bash-roundtrip");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let home = fixture.path().join("home");
        std::fs::create_dir_all(home.join("fixture-project")).unwrap();
        std::fs::create_dir_all(home.join("fixture-scratch")).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "commands::bash::tests::bash_permissions_roundtrip_alf_quoted_gate",
                "--nocapture",
            ])
            .env(CHILD_HOME, &home)
            .env("HOME", &home)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "the isolated child must run its test: {stdout}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bash_permissions_roundtrip_unix_temp_aliases() {
        use crate::bash_permissions::{is_system_temp_path, PermissionKind};
        let scratch = std::env::current_dir()
            .unwrap()
            .join("target/bash-roundtrip");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let project = fixture.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::write(temp.path().join("existing.txt"), "fixture").unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        let alias = fixture.path().join("tmp-alias");
        std::os::unix::fs::symlink(&canonical, &alias).unwrap();
        assert!(!alias.starts_with(&project));
        assert_eq!(std::fs::canonicalize(&alias).unwrap(), canonical);
        #[cfg(target_os = "macos")]
        assert_eq!(
            std::fs::canonicalize("/tmp").unwrap(),
            PathBuf::from("/private/tmp")
        );
        let private_alias = Path::new("/private/tmp").join(temp.path().file_name().unwrap());
        for directory in [
            temp.path(),
            canonical.as_path(),
            alias.as_path(),
            private_alias.as_path(),
        ] {
            for tail in ["existing.txt", "missing/deep/file.txt"] {
                let path = directory.join(tail);
                assert!(
                    is_system_temp_path(&path),
                    "temporary alias must be recognized: {path:?}"
                );
                let command = format!("chmod +x '{}' && cat '{}'", path.display(), path.display());
                let asks = assert_bash_permission_roundtrip(&command, &project);
                assert!(
                    asks.iter().all(|ask| ask.kind == PermissionKind::Bash),
                    "temp paths must not create external asks: {asks:?}"
                );
            }
        }
        let ordinary = fixture.path().join("not-system-temp/file.txt");
        assert!(!is_system_temp_path(&ordinary));
        let asks = crate::bash_permissions::scan::scan_with_project_root(
            &format!("cat '{}'", ordinary.display()),
            &project,
            &project,
        );
        assert!(asks
            .iter()
            .any(|ask| ask.kind == PermissionKind::ExternalDirectory));
    }

    #[cfg(windows)]
    #[test]
    fn bash_permissions_roundtrip_windows_temp_short_and_long() {
        use crate::bash_permissions::{is_system_temp_path, PermissionKind};
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let temp = tempfile::Builder::new()
            .prefix("aft permission temp long name ")
            .tempdir()
            .unwrap();
        let long = crate::windows_path::normalize_windows_path(
            &std::fs::canonicalize(temp.path()).unwrap(),
        );
        let input: Vec<u16> = long.as_os_str().encode_wide().chain(Some(0)).collect();
        let size = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
        assert_ne!(
            size,
            0,
            "GetShortPathNameW: {}",
            std::io::Error::last_os_error()
        );
        let mut output = vec![0; size as usize];
        let written = unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), size) };
        assert!(written > 0 && written < size);
        let short = PathBuf::from(std::ffi::OsString::from_wide(&output[..written as usize]));
        if short
            .to_string_lossy()
            .eq_ignore_ascii_case(&long.to_string_lossy())
        {
            assert!(
                std::env::var_os("AFT_TEST_REQUIRE_WINDOWS_SHORT_PATH").is_none(),
                "Windows temp volume did not provide an 8.3 alias"
            );
            eprintln!("skipped: the temp volume does not create 8.3 short names");
            return;
        }
        assert_eq!(
            crate::windows_path::normalize_windows_path(&std::fs::canonicalize(&short).unwrap()),
            long
        );
        std::fs::write(long.join("existing.txt"), "fixture").unwrap();
        let scratch = std::env::current_dir()
            .unwrap()
            .join("target/bash-roundtrip");
        std::fs::create_dir_all(&scratch).unwrap();
        let project = tempfile::tempdir_in(scratch).unwrap();
        for directory in [&short, &long] {
            for tail in ["existing.txt", "missing/deep/file.txt"] {
                let path = directory.join(tail);
                assert!(
                    is_system_temp_path(&path),
                    "temporary alias must be recognized: {path:?}"
                );
                let command = format!("chmod +x '{}' && cat '{}'", path.display(), path.display());
                let asks = assert_bash_permission_roundtrip(&command, project.path());
                assert!(
                    asks.iter().all(|ask| ask.kind == PermissionKind::Bash),
                    "temp paths must not create external asks: {asks:?}"
                );
            }
        }
        println!("Windows temp aliases exercised: {short:?} == {long:?}");
    }

    #[test]
    fn external_approval_coverage_respects_path_and_kind() {
        use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/coverage-fixture");
        let file = directory.join("file.txt");
        let target = AccessGrant {
            path: file.clone(),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let exact = file.to_string_lossy().into_owned();
        let folder = format!("{}/*", directory.display());
        assert!(external_approval_covers(&exact, &target));
        assert!(external_approval_covers(&folder, &target));
        assert!(external_approval_covers(
            &folder,
            &AccessGrant {
                path: directory.join("nested/other.txt"),
                ..target.clone()
            }
        ));
        assert!(!external_approval_covers(
            &exact,
            &AccessGrant {
                path: directory.join("sibling.txt"),
                ..target.clone()
            }
        ));
        assert!(!external_approval_covers(
            &exact,
            &AccessGrant {
                kind: GrantKind::Subtree,
                ..target.clone()
            }
        ));
        assert!(external_approval_covers(
            &folder,
            &AccessGrant {
                path: directory.join("nested"),
                kind: GrantKind::Subtree,
                ..target.clone()
            }
        ));
        assert!(!external_approval_covers(
            &folder,
            &AccessGrant {
                path: directory
                    .with_file_name("coverage-fixture-other")
                    .join("file.txt"),
                ..target.clone()
            }
        ));
        assert!(!external_approval_covers("*", &target));
        assert!(!external_approval_covers("cat *", &target));

        let ask = crate::bash_permissions::PermissionAsk {
            kind: crate::bash_permissions::PermissionKind::ExternalDirectory,
            patterns: vec![folder.clone()],
            always: vec![folder.clone()],
            reason: None,
            targets: vec![target],
        };
        let serialized = serde_json::to_value(&ask).unwrap();
        assert!(
            serialized.get("targets").is_none(),
            "internal targets must not change host prompts"
        );
        let missing: crate::bash_permissions::PermissionAsk =
            serde_json::from_value(serialized).unwrap();
        assert!(
            !permissions_granted_cover(&[missing], &[folder]),
            "unknown targets must fail closed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn external_approval_coverage_normalizes_windows_path_spellings() {
        use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
        for (plain, verbatim) in [
            (r"D:\a\aft\file.txt", r"\\?\D:\a\aft\file.txt"),
            (r"D:/a/aft/file.txt", r"\\?\d:\a\aft\file.txt"),
            (
                r"\\server\share\folder\file.txt",
                r"\\?\UNC\server\share\folder\file.txt",
            ),
        ] {
            for (approved, requested) in [(plain, verbatim), (verbatim, plain)] {
                let target = AccessGrant {
                    path: PathBuf::from(requested),
                    kind: GrantKind::File,
                    operation: AccessOperation::Read,
                };
                let original = target.clone();
                assert!(
                    external_approval_covers(approved, &target),
                    "file: {approved} -> {requested}"
                );
                let folder = format!("{}/*", Path::new(approved).parent().unwrap().display());
                assert!(
                    external_approval_covers(&folder, &target),
                    "folder: {folder} -> {requested}"
                );
                assert!(!external_approval_covers(
                    approved,
                    &AccessGrant {
                        kind: GrantKind::Subtree,
                        ..target.clone()
                    }
                ));
                assert!(!external_approval_covers(
                    approved,
                    &AccessGrant {
                        path: target.path.with_file_name("other.txt"),
                        ..target.clone()
                    }
                ));
                assert!(!external_approval_covers(
                    &folder,
                    &AccessGrant {
                        path: target
                            .path
                            .parent()
                            .unwrap()
                            .with_file_name("other-folder")
                            .join("file.txt"),
                        ..target.clone()
                    }
                ));
                assert_eq!(
                    target, original,
                    "comparison must preserve the requested spelling"
                );
            }
        }
    }

    #[test]
    fn powershell_commands_require_approval_without_posix_scanning() {
        // `cat` is a POSIX rewrite candidate, but PowerShell has its own syntax
        // and aliases. The request must remain an explicit permission ask.
        let asks = conservative_powershell_permission_asks("cat $env:USERPROFILE\\secret.txt");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].patterns, ["cat $env:USERPROFILE\\secret.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_running_bash_from_other_root_is_not_warned_about() {
        let root = tempfile::tempdir().unwrap();
        let other_root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(root.path(), storage.path());
        let first_request = spawn_test_request("duplicate-root-first", "sleep 30", true);
        let first = handle(&first_request, &ctx);
        assert!(first.success, "{:?}", first.data);

        // Keep the registry and its active task, but move this request to a
        // different project root while deliberately retaining the same cwd.
        ctx.update_config(|config| config.project_root = Some(other_root.path().to_path_buf()));
        let mut other_root_request = spawn_test_request("duplicate-other-root", "sleep 30", true);
        other_root_request.params["params"]["workdir"] = serde_json::json!(root.path());
        let response = handle(&other_root_request, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert!(response.data.get("_bash_duplicate_warning").is_none());

        stop_spawned_test_task(&ctx, &first);
        stop_spawned_test_task(&ctx, &response);
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_running_bash_uses_resolved_relative_workdir() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let subdir = root.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        let ctx = spawn_test_context(root.path(), storage.path());
        ctx.update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
        let mut first_request = spawn_test_request("duplicate-absolute", "sleep 30", true);
        first_request.params["params"]["workdir"] = json!(subdir);
        let first = handle(&first_request, &ctx);
        assert!(first.success, "{:?}", first.data);

        let mut second_request = spawn_test_request("duplicate-relative", "sleep 30", true);
        second_request.params["params"]["workdir"] = json!("subdir");
        let second = handle(&second_request, &ctx);
        stop_spawned_test_task(&ctx, &first);
        stop_spawned_test_task(&ctx, &second);

        assert!(second.success, "{:?}", second.data);
        let warning = second.data["_bash_duplicate_warning"].as_str().unwrap();
        assert!(
            warning.contains(first.data["task_id"].as_str().unwrap()),
            "{warning}"
        );
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

    #[test]
    fn resolved_workdir_preserves_absolute_paths_and_gates_rewrites() {
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("sub")).unwrap();
        let ctx = ctx_with_root(project.path());
        assert_eq!(resolve_workdir(None, &ctx), project.path());
        assert_eq!(resolve_workdir(Some(other.path()), &ctx), other.path());
        assert!(workdir_matches_project_root(
            &resolve_workdir(Some(Path::new(".")), &ctx),
            &ctx
        ));
        assert!(!workdir_matches_project_root(
            &resolve_workdir(Some(Path::new("sub")), &ctx),
            &ctx
        ));
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

    #[cfg(unix)]
    #[test]
    fn relative_workdir_executes_in_project_subdirectory() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let sub = project.path().join("bash-relative-workdir");
        std::fs::create_dir(&sub).unwrap();
        let mut config = crate::config::Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            ..crate::config::Config::default()
        };
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let mut request = spawn_test_request(
            "relative-workdir",
            "printf relative-cwd-ok > marker.txt",
            false,
        );
        request.params["params"]["workdir"] = json!("bash-relative-workdir");
        let launched = handle(&request, &ctx);
        assert!(launched.success, "spawn failed: {:?}", launched.data);
        let task_id = launched.data["task_id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snapshot = ctx
                .bash_background()
                .status(
                    task_id,
                    "sandbox-spawn-test",
                    Some(project.path()),
                    Some(storage.path()),
                    0,
                )
                .unwrap();
            if snapshot.info.status.is_terminal() {
                assert_eq!(snapshot.exit_code, Some(0));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "task did not finish");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read(sub.join("marker.txt")).unwrap(),
            b"relative-cwd-ok"
        );
        assert!(!project.path().join("marker.txt").exists());
    }

    #[test]
    fn split_cache_root_bash_launch_persists_the_real_task_under_durable_storage() {
        // Tests elsewhere set AFT_STORAGE_DIR process-wide, which outranks the
        // configured storage_dir; hold the shared env lock without it.
        let _storage_env = crate::test_env::without_storage_override();
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            project_root: Some(project.path().to_path_buf()),
            storage_dir: Some(storage.path().to_path_buf()),
            cache_dir: Some(cache.path().to_path_buf()),
            experimental_bash_background: true,
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let artifacts = crate::root_cache::cache_root(&config);
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        crate::sandbox_spawn::install_sandbox_spawn_test_seam(project.path().to_path_buf());
        let response = handle(
            &spawn_test_request("split-root-task", "echo durable-task", true),
            &ctx,
        );
        crate::sandbox_spawn::clear_sandbox_spawn_test_seam(project.path());
        assert!(response.success, "{response:?}");
        let task_id = response.data["task_id"].as_str().unwrap();
        let root = crate::bash_background::task_storage_dir(&ctx);
        assert_eq!(root, storage.path());
        let task =
            crate::bash_background::persistence::resolve_task(&root, "sandbox-spawn-test", task_id)
                .unwrap();
        assert!(task.paths.json.is_file());
        assert!(task.paths.json.starts_with(storage.path()));
        assert!(!artifacts.join("bash-tasks").exists());
        stop_spawned_test_task(&ctx, &response);
    }

    #[test]
    fn synthetic_load_role_gate_refuses_worker_not_head() {
        use crate::sandbox_spawn::{with_spawn_plan_for_test, SpawnPlan};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        // A refusing spawn seam keeps this test safe even when the load guard is
        // removed: no real load generator is ever launched by either role.
        for background in [false, true] {
            let mut request = spawn_test_request("synthetic-load", "yes > /dev/null &", background);
            request.params[crate::protocol::WORKER_SESSION_FIELD] = json!(true);
            let worker =
                with_spawn_plan_for_test(SpawnPlan::refused_for_test("test_no_execution"), || {
                    handle(&request, &ctx)
                });
            assert!(!worker.success);
            assert_eq!(worker.data["code"], "synthetic_load_refused");
            assert_eq!(
                worker.data["message"].as_str(),
                Some("synthetic CPU load is not allowed on shared machines, local or remote; prove the mechanism deterministically instead")
            );

            request.params[crate::protocol::WORKER_SESSION_FIELD] = json!(false);
            let head =
                with_spawn_plan_for_test(SpawnPlan::refused_for_test("test_no_execution"), || {
                    handle(&request, &ctx)
                });
            assert_eq!(head.data["code"], "test_no_execution");

            // Both PowerShell selectors bypass the POSIX-only guard, whether
            // PowerShell is installed locally or not. The spawn seam still
            // prevents execution if it is installed and permission is granted.
            request.params[crate::protocol::WORKER_SESSION_FIELD] = json!(true);
            for dedicated_route in [false, true] {
                request.command = if dedicated_route {
                    "powershell"
                } else {
                    "bash"
                }
                .to_string();
                request.params["params"]["shell"] = json!("powershell");
                let powershell = with_spawn_plan_for_test(
                    SpawnPlan::refused_for_test("test_no_execution"),
                    || handle(&request, &ctx),
                );
                assert_ne!(powershell.data["code"], "synthetic_load_refused");
            }
        }
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
    fn bash_db_hints_real_missing_column_trailer() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let created = std::process::Command::new("sqlite3")
            .arg(project.path().join("store.db"))
            .arg("CREATE TABLE tasks(id TEXT, kind TEXT);")
            .status()
            .expect("sqlite3 must be installed for schema hint tests");
        assert!(created.success());
        let ctx = spawn_test_context(project.path(), storage.path());
        let request = spawn_test_request(
            "db-hint",
            "sqlite3 store.db 'SELECT substrate FROM tasks'",
            false,
        );
        let launched = handle(&request, &ctx);
        assert!(launched.success, "{:?}", launched.data);
        let task_id = launched.data["task_id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let snapshot = ctx
                .bash_background()
                .status(
                    task_id,
                    "sandbox-spawn-test",
                    Some(project.path()),
                    Some(storage.path()),
                    8192,
                )
                .unwrap();
            if snapshot.info.status.is_terminal() {
                assert!(
                    snapshot.output_preview.contains("[aft: no column"),
                    "{}",
                    snapshot.output_preview
                );
                assert!(
                    snapshot
                        .output_preview
                        .contains("tasks: id TEXT, kind TEXT"),
                    "{}",
                    snapshot.output_preview
                );
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A foreground bash that hits its timeout is reported to a status
    /// poller as `timed_out` with exit code 124 and the kill's exit marker,
    /// even when the poll lands while the timeout kill is still signaling the
    /// process group (held there by the kill's test gate): `bash_status`
    /// waits for the kill's outcome rather than answering `killing`.
    #[cfg(unix)]
    #[test]
    fn foreground_timeout_status_is_timed_out_with_the_exit_marker() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let mut request = spawn_test_request("fg-timeout", "sleep 5", false);
        request.params["params"]["timeout"] = json!(100);
        let launched = handle(&request, &ctx);
        assert!(launched.success, "spawn failed: {:?}", launched.data);
        assert_eq!(launched.data["status"], "running");
        let task_id = launched.data["task_id"].as_str().unwrap().to_string();
        // Install the kill's test gate before the 100 ms timeout can expire:
        // the watchdog's timeout kill then pauses after publishing `killing`
        // and before it signals.
        let (reached, release) =
            crate::bash_background::registry::install_kill_signal_gate_for_test(&task_id);
        reached
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the timeout kill reached its signal step");

        let status_request: RawRequest = serde_json::from_value(json!({
            "id": "fg-timeout-status",
            "command": "bash_status",
            "session_id": "sandbox-spawn-test",
            "params": { "task_id": task_id },
        }))
        .unwrap();
        let status = std::thread::scope(|scope| {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let ctx = &ctx;
            let status_request = &status_request;
            scope.spawn(move || {
                let _ = done_tx.send(crate::commands::bash_status::handle(status_request, ctx));
            });
            let answered_early = done_rx
                .recv_timeout(std::time::Duration::from_millis(300))
                .ok();
            release.send(()).expect("release the timeout kill");
            answered_early.unwrap_or_else(|| {
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("bash_status answered once the kill settled")
            })
        });

        assert!(status.success, "bash_status failed: {:?}", status.data);
        assert_eq!(
            status.data["status"], "timed_out",
            "status: {:?}",
            status.data
        );
        assert_eq!(status.data["exit_code"], 124, "status: {:?}", status.data);
        let exit_path = ctx
            .bash_background()
            .task_exit_path(&task_id, "sandbox-spawn-test")
            .expect("task exit path");
        assert_eq!(std::fs::read_to_string(exit_path).unwrap(), "killed");
    }

    /// The hard kill recorded for the task a `bash` request spawned.
    #[cfg(unix)]
    fn spawned_hard_kill_ms(extra: serde_json::Value) -> Option<u64> {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let mut request = spawn_test_request("hard-kill", "sleep 30", false);
        for (key, value) in extra.as_object().unwrap() {
            // `worker_session` sits beside `session_id`, outside the bash arguments,
            // as the plugins send it.
            if key == crate::protocol::WORKER_SESSION_FIELD {
                request.params[key] = value.clone();
            } else {
                request.params["params"][key] = value.clone();
            }
        }
        let response = handle(&request, &ctx);
        assert!(response.success, "spawn failed: {:?}", response.data);
        let task_id = response.data["task_id"].as_str().unwrap().to_string();
        let json_path = ctx
            .bash_background()
            .task_json_path(&task_id, "sandbox-spawn-test")
            .expect("task json path");
        let task = crate::bash_background::persistence::read_task(&json_path).unwrap();
        stop_spawned_test_task(&ctx, &response);
        task.timeout_ms
    }

    // Nothing a delegated worker starts runs without a hard kill: a worker's
    // `wait: true` call without a timeout gets the same default as every other
    // call (it used to get none, and a stuck test run then held a worker for
    // fifteen hours). An explicit timeout always wins.
    #[cfg(unix)]
    #[test]
    fn worker_wait_without_timeout_gets_the_default_hard_kill() {
        let default_ms = crate::bash_background::registry::DEFAULT_BG_TIMEOUT.as_millis() as u64;
        assert_eq!(
            spawned_hard_kill_ms(json!({ "wait": true, "worker_session": true })),
            Some(default_ms)
        );
        assert_eq!(
            spawned_hard_kill_ms(json!({ "wait": true })),
            Some(default_ms)
        );
        assert_eq!(
            spawned_hard_kill_ms(json!({ "worker_session": true })),
            Some(default_ms)
        );
        assert_eq!(
            spawned_hard_kill_ms(json!({ "background": true, "worker_session": true })),
            Some(default_ms)
        );
        assert_eq!(
            spawned_hard_kill_ms(
                json!({ "wait": true, "worker_session": true, "timeout": 45_000 })
            ),
            Some(45_000)
        );
    }

    #[cfg(unix)]
    fn spawn_background_task(ctx: &AppContext, extra: serde_json::Value) -> String {
        let mut request = spawn_test_request("renew", "sleep 30", true);
        for (key, value) in extra.as_object().unwrap() {
            request.params["params"][key] = value.clone();
        }
        let response = handle(&request, ctx);
        assert!(response.success, "spawn failed: {:?}", response.data);
        response.data["task_id"].as_str().unwrap().to_string()
    }

    #[cfg(unix)]
    fn bash_output_for_test(
        ctx: &AppContext,
        project_root: &Path,
        request: &RawRequest,
        worker_preset: bool,
    ) -> String {
        let response =
            crate::bash_background::with_worker_preset(worker_preset, || handle(request, ctx));
        assert!(response.success, "bash spawn failed: {:?}", response.data);
        let task_id = response.data["task_id"].as_str().unwrap();
        let storage = crate::bash_background::task_storage_dir(ctx);
        let started = std::time::Instant::now();
        loop {
            let snapshot = ctx
                .bash_background()
                .status(
                    task_id,
                    "sandbox-spawn-test",
                    Some(project_root),
                    Some(&storage),
                    4096,
                )
                .expect("spawned task should be visible");
            if snapshot.info.status.is_terminal() {
                return snapshot.output_preview;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "bash test task did not finish: {snapshot:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[cfg(unix)]
    fn worker_thread_output_request(id: &str) -> RawRequest {
        spawn_test_request(
            id,
            r#"printf '%s|%s' "${NEXTEST_TEST_THREADS-unset}" "${RUST_TEST_THREADS-unset}""#,
            true,
        )
    }

    #[cfg(unix)]
    fn without_test_thread_environment() -> impl Drop {
        struct Restore {
            values: [(&'static str, Option<std::ffi::OsString>); 2],
            _lock: crate::test_env::ProcessEnvLockGuard,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                for (name, value) in &self.values {
                    if let Some(value) = value {
                        std::env::set_var(name, value);
                    } else {
                        std::env::remove_var(name);
                    }
                }
            }
        }
        let lock = crate::test_env::process_env_lock();
        let values = ["NEXTEST_TEST_THREADS", "RUST_TEST_THREADS"].map(|name| {
            let previous = std::env::var_os(name);
            std::env::remove_var(name);
            (name, previous)
        });
        Restore {
            values,
            _lock: lock,
        }
    }

    #[cfg(unix)]
    #[test]
    fn worker_preset_bash_reads_budget_file_and_defaults_missing_or_garbage_to_four() {
        // Defaults apply only when the child inherits no caller-selected budget.
        // Nextest itself sets NEXTEST_TEST_THREADS in the fixture's environment.
        let _environment = without_test_thread_environment();
        let container = tempfile::tempdir().unwrap();
        let project = container.path().join("worktree");
        std::fs::create_dir(&project).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(&project, storage.path());
        let budget_path = container.path().join(".cargo/alfonso-test-threads");

        std::fs::create_dir_all(budget_path.parent().unwrap()).unwrap();
        std::fs::write(&budget_path, "7\n").unwrap();
        let mut budget_request = worker_thread_output_request("budget-file");
        budget_request.params["params"]["workdir"] = json!(project.join("nested"));
        std::fs::create_dir(project.join("nested")).unwrap();
        assert_eq!(
            bash_output_for_test(&ctx, &project, &budget_request, true),
            "7|7"
        );

        std::fs::remove_file(&budget_path).unwrap();
        assert_eq!(
            bash_output_for_test(
                &ctx,
                &project,
                &worker_thread_output_request("budget-missing"),
                true
            ),
            "4|4"
        );

        std::fs::write(&budget_path, "garbage\n").unwrap();
        assert_eq!(
            bash_output_for_test(
                &ctx,
                &project,
                &worker_thread_output_request("budget-invalid"),
                true
            ),
            "4|4"
        );
    }

    #[cfg(unix)]
    #[test]
    fn worker_bash_respects_call_environment_and_head_or_plugin_worker_gets_no_default() {
        let _environment = without_test_thread_environment();
        let container = tempfile::tempdir().unwrap();
        let project = container.path().join("worktree");
        std::fs::create_dir(&project).unwrap();
        let budget_path = container.path().join(".cargo/alfonso-test-threads");
        std::fs::create_dir_all(budget_path.parent().unwrap()).unwrap();
        std::fs::write(&budget_path, "8\n").unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(&project, storage.path());

        // A per-call test-thread value cannot defeat the budget: the names are
        // refused before the launch, so the worker still gets its budget.
        let mut explicit = worker_thread_output_request("budget-explicit");
        explicit.params["params"]["env"] = json!({
            "NEXTEST_TEST_THREADS": "13",
            "RUST_TEST_THREADS": "17"
        });
        let refused = crate::bash_background::with_worker_preset(true, || handle(&explicit, &ctx));
        assert!(!refused.success, "{:?}", refused.data);
        assert_eq!(refused.data["code"], "blocked_env_var");
        assert_eq!(
            bash_output_for_test(
                &ctx,
                &project,
                &worker_thread_output_request("budget"),
                true
            ),
            "8|8"
        );

        let mut head = worker_thread_output_request("budget-head");
        head.params[crate::protocol::WORKER_SESSION_FIELD] = json!(true);
        assert_eq!(
            bash_output_for_test(&ctx, &project, &head, false),
            "unset|unset"
        );

        std::env::set_var("NEXTEST_TEST_THREADS", "19");
        std::env::set_var("RUST_TEST_THREADS", "23");
        assert_eq!(
            bash_output_for_test(
                &ctx,
                &project,
                &worker_thread_output_request("budget-inherited"),
                true
            ),
            "19|23"
        );
        assert_eq!(bash_output_for_test(&ctx, &project, &head, false), "19|23");
    }

    #[cfg(unix)]
    fn status_request(task_id: &str, worker: bool) -> RawRequest {
        let mut request = json!({
            "id": "renew-status",
            "command": "bash_status",
            "session_id": "sandbox-spawn-test",
            "params": { "task_id": task_id },
        });
        if worker {
            request[crate::protocol::WORKER_SESSION_FIELD] = json!(true);
        }
        serde_json::from_value(request).unwrap()
    }

    /// A delegated worker's status read (what its `bash_watch` polls) pushes
    /// the task's default hard kill to at least one worker wait limit away. A
    /// primary's read does not, and an explicit timeout is never extended.
    #[cfg(unix)]
    #[test]
    fn a_worker_status_read_extends_only_the_default_hard_kill() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| config.bash.worker_wait_max_ms = 50 * 60 * 1000);
        let registry = ctx.bash_background();
        let session = "sandbox-spawn-test";
        let default_ms = crate::bash_background::registry::DEFAULT_BG_TIMEOUT.as_millis() as u64;

        let implicit = spawn_background_task(&ctx, json!({}));
        let explicit = spawn_background_task(&ctx, json!({ "timeout": 45_000 }));

        crate::commands::bash_status::handle(&status_request(&implicit, false), &ctx);
        assert_eq!(
            registry.hard_kill_limit_ms_for_test(&implicit, session),
            Some(default_ms),
            "a primary's status read must not extend the kill"
        );

        crate::commands::bash_status::handle(&status_request(&implicit, true), &ctx);
        let renewed = registry
            .hard_kill_limit_ms_for_test(&implicit, session)
            .unwrap();
        assert!(
            renewed >= 50 * 60 * 1000,
            "a worker's status read extends the kill to the worker wait limit: {renewed}"
        );

        crate::commands::bash_status::handle(&status_request(&explicit, true), &ctx);
        assert_eq!(
            registry.hard_kill_limit_ms_for_test(&explicit, session),
            Some(45_000),
            "an explicit timeout is never extended"
        );

        for task_id in [&implicit, &explicit] {
            let _ = registry.kill(task_id, session);
        }
    }

    /// The watchdog kills by the extended limit: a task whose kill is due in
    /// 300 ms survives once a worker's wait extends it, while an identical
    /// task nobody waits on is timed out.
    #[cfg(unix)]
    #[test]
    fn an_extended_hard_kill_keeps_a_watched_task_alive() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let registry = ctx.bash_background();
        let session = "sandbox-spawn-test";

        let watched = spawn_background_task(&ctx, json!({}));
        let unwatched = spawn_background_task(&ctx, json!({}));
        registry.set_hard_kill_limit_ms_for_test(&watched, session, 300);
        registry.set_hard_kill_limit_ms_for_test(&unwatched, session, 300);
        assert!(registry
            .renew_hard_kill(&watched, session, std::time::Duration::from_secs(60))
            .is_some());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = |task_id: &str| {
            crate::commands::bash_status::handle(&status_request(task_id, false), &ctx).data
                ["status"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        while status(&unwatched) != "timed_out" {
            assert!(
                std::time::Instant::now() < deadline,
                "the unwatched task was never timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(status(&watched), "running");
        // The kill is named by the limit that fired, AFT's default, so it is
        // not mistaken for the command failing; a still-running task's status
        // names its own deadline.
        let killed = crate::commands::bash_status::handle(&status_request(&unwatched, true), &ctx);
        let reason = killed.data["status_reason"].as_str().unwrap_or_default();
        assert!(
            reason.starts_with("killed by AFT's default background limit of 0.3s (exit 124)"),
            "{:?}",
            killed.data
        );
        assert_eq!(killed.data["exit_code"], 124, "{:?}", killed.data);
        let alive = crate::commands::bash_status::handle(&status_request(&watched, false), &ctx);
        assert_eq!(
            alive.data["hard_kill"]["source"], "default",
            "{:?}",
            alive.data
        );
        assert!(alive.data["hard_kill"]["limit_ms"].as_u64().unwrap() >= 60_000);
        assert!(
            alive.data["elapsed_ms"].as_u64().is_some(),
            "{:?}",
            alive.data
        );
        let _ = registry.kill(&watched, session);
    }

    #[cfg(unix)]
    fn read_record(
        ctx: &AppContext,
        task_id: &str,
    ) -> crate::bash_background::persistence::PersistedTask {
        let path = ctx
            .bash_background()
            .task_json_path(task_id, "sandbox-spawn-test")
            .expect("task json path");
        crate::bash_background::persistence::read_task(&path).unwrap()
    }

    /// Rewrites the hard-kill limit in a task's record to `limit_ms`, as if
    /// the task had already run past its recorded limit while AFT was down
    /// (the start time cannot be faked: replay checks it against the live
    /// process).
    #[cfg(unix)]
    fn set_recorded_limit(ctx: &AppContext, task_id: &str, limit_ms: u64) {
        let path = ctx
            .bash_background()
            .task_json_path(task_id, "sandbox-spawn-test")
            .expect("task json path");
        let mut record = crate::bash_background::persistence::read_task(&path).unwrap();
        record.timeout_ms = Some(limit_ms);
        crate::bash_background::persistence::write_task(&path, &record).unwrap();
    }

    /// Simulates an AFT restart: the old engine lets go of its tasks without
    /// killing them (as an exiting AFT process does), and a fresh engine
    /// context over the same storage reloads the session's tasks from their
    /// records. `window_ms` is the worker wait limit the restarted engine
    /// grants. The old context must be detached first: a real restart never
    /// leaves the old registry running, and one that still held the child
    /// would see its own child die when the new engine kills it and record
    /// `failed` over the same task record.
    #[cfg(unix)]
    fn restart(old: &AppContext, project: &Path, storage: &Path, window_ms: u64) -> AppContext {
        old.bash_background().detach();
        let restarted = spawn_test_context(project, storage);
        restarted
            .bash_background()
            .set_worker_wait_window(std::time::Duration::from_millis(window_ms));
        restarted
            .bash_background()
            .replay_session_for_project(
                &crate::bash_background::task_storage_dir(&restarted),
                "sandbox-spawn-test",
                project,
            )
            .expect("replay after restart");
        restarted
    }

    /// A default kill a worker's waits extended is written to the task's
    /// record (with its source), so a restart that reloads the task past its
    /// original default limit keeps it alive instead of killing it at once.
    /// The record starts out already past due (200 ms) and the restarted
    /// engine grants only a 300 ms window, so only the written renewal can
    /// keep the task running.
    #[cfg(unix)]
    #[test]
    fn a_renewed_task_survives_a_restart_from_its_record() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let session = "sandbox-spawn-test";
        let task = spawn_background_task(&ctx, json!({}));
        set_recorded_limit(&ctx, &task, 200);
        let renewed = ctx
            .bash_background()
            .renew_hard_kill(&task, session, std::time::Duration::from_secs(2 * 3600))
            .expect("a default kill is renewable");

        let record = read_record(&ctx, &task);
        assert!(record.default_hard_kill, "the record carries the source");
        assert_eq!(
            record.timeout_ms,
            Some(renewed),
            "the renewed limit reached the record"
        );

        let restarted = restart(&ctx, project.path(), storage.path(), 300);
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        let status =
            crate::commands::bash_status::handle(&status_request(&task, false), &restarted).data;
        assert_eq!(status["status"], "running", "{status:?}");
        let _ = restarted.bash_background().kill(&task, session);
    }

    /// A reloaded task whose recorded default limit passed during the
    /// downtime is granted one worker wait window from the restart (not
    /// killed the instant AFT comes back), stays renewable, and is still
    /// killed by the default limit once the waits stop.
    #[cfg(unix)]
    #[test]
    fn a_reloaded_default_kill_task_gets_one_window_then_is_killed() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let session = "sandbox-spawn-test";
        let task = spawn_background_task(&ctx, json!({}));
        assert!(read_record(&ctx, &task).default_hard_kill);
        // The recorded default limit passed while AFT was down.
        set_recorded_limit(&ctx, &task, 1);

        let reloaded_at = std::time::Instant::now();
        let restarted = restart(&ctx, project.path(), storage.path(), 3_000);
        let status = |restarted: &AppContext| {
            crate::commands::bash_status::handle(&status_request(&task, false), restarted).data
        };
        // Past at least two watchdog passes (one every 500 ms), which would
        // have killed a task whose limit was not extended.
        std::thread::sleep(std::time::Duration::from_millis(1_200));
        let early = status(&restarted);
        assert_eq!(
            early["status"], "running",
            "not killed when AFT comes back: {early:?}"
        );
        assert!(
            restarted
                .bash_background()
                .renew_hard_kill(&task, session, std::time::Duration::from_millis(3_000))
                .is_some(),
            "a reloaded default kill stays renewable"
        );
        let deadline = reloaded_at + std::time::Duration::from_secs(20);
        let killed = loop {
            let data = status(&restarted);
            if data["status"] == "timed_out" {
                break data;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never killed once the waits stopped: {data:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        assert!(
            reloaded_at.elapsed() >= std::time::Duration::from_millis(4_000),
            "killed before the renewed window ran out"
        );
        assert!(
            killed["status_reason"]
                .as_str()
                .unwrap_or_default()
                .starts_with("killed by AFT's default background limit"),
            "{killed:?}"
        );
        let _ = restarted.bash_background().kill(&task, session);
    }

    /// A caller's explicit `timeout` is never extended, after a restart
    /// either: a task reloaded past its timeout is killed, by that timeout.
    #[cfg(unix)]
    #[test]
    fn an_explicit_timeout_task_reloaded_past_its_timeout_is_killed() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let session = "sandbox-spawn-test";
        let task = spawn_background_task(&ctx, json!({ "timeout": 60_000 }));
        assert!(!read_record(&ctx, &task).default_hard_kill);
        // The caller's timeout passed while AFT was down.
        set_recorded_limit(&ctx, &task, 200);

        let restarted = restart(&ctx, project.path(), storage.path(), 2 * 3600 * 1000);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let data =
                crate::commands::bash_status::handle(&status_request(&task, false), &restarted)
                    .data;
            if data["status"] == "timed_out" {
                assert_eq!(
                    data["status_reason"], "killed by the `timeout` of 0.2s you passed (exit 124)",
                    "{data:?}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "an explicit timeout was extended across the restart: {data:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = restarted.bash_background().kill(&task, session);
    }

    /// A kill by the caller's own `timeout` is named as that, not as AFT's
    /// default limit.
    #[cfg(unix)]
    #[test]
    fn an_explicit_timeout_kill_is_named_as_the_callers_timeout() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        let task = spawn_background_task(&ctx, json!({ "timeout": 200 }));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let data =
                crate::commands::bash_status::handle(&status_request(&task, false), &ctx).data;
            if data["status"] == "timed_out" {
                assert_eq!(
                    data["status_reason"], "killed by the `timeout` of 0.2s you passed (exit 124)",
                    "{data:?}"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never timed out: {data:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    fn assert_worker_outside_rm_asks(
        mode: crate::config::SandboxMode,
        principal: crate::sandbox_spawn::AuthenticatedPrincipal,
    ) {
        use crate::sandbox_spawn::{with_spawn_plan_for_test, with_worker_jail, SpawnPlan};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        // Keep the target outside both the project and the temporary-directory
        // exemption used by the permission scanner.
        let outside = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let target = outside.path().join("unapproved-delete");
        std::fs::write(&target, "keep me").unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| config.sandbox.profile.worker = mode);
        for requested_by_plugin in [false, true] {
            ctx.update_config(|config| config.bash_permissions = !requested_by_plugin);
            let mut request = spawn_test_request(
                "outside-rm",
                &format!("rm -- '{}'", target.display()),
                false,
            );
            request.params[crate::protocol::WORKER_SESSION_FIELD] = json!(true);
            request.params["params"]["permissions_requested"] = json!(requested_by_plugin);
            // Refuse at the spawn seam if approval is bypassed, so a failing
            // regression cannot actually delete the fixture.
            let run = |request: &RawRequest| {
                crate::sandbox_spawn::with_authenticated_principal(principal.clone(), || {
                    with_worker_jail(true, || {
                        with_spawn_plan_for_test(
                            SpawnPlan::refused_for_test("unapproved_spawn"),
                            || handle(request, &ctx),
                        )
                    })
                })
            };
            let response = run(&request);
            assert_eq!(
                response.data["code"], ERROR_PERMISSION_REQUIRED,
                "{response:?}"
            );
            assert!(!response.success);
            assert!(
                response.data["asks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|ask| { ask["kind"] == "external_directory" }),
                "outside-project rm must ask: {response:?}"
            );
            assert_eq!(
                ctx.bash_background().try_health_counts().unwrap().running,
                0
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep me");

            let grants: Vec<_> = response.data["asks"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|ask| ask["patterns"].as_array().unwrap())
                .cloned()
                .collect();
            request.params["params"]["permissions_granted"] = json!(grants);
            assert_eq!(
                run(&request).data["code"],
                "unapproved_spawn",
                "approval must allow the request to reach spawn policy"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn relaxed_worker_outside_rm_requires_approval() {
        assert_worker_outside_rm_asks(
            crate::config::SandboxMode::Relaxed,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        );
    }

    #[cfg(unix)]
    #[test]
    fn off_worker_outside_rm_requires_approval() {
        assert_worker_outside_rm_asks(
            crate::config::SandboxMode::Off,
            crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
        );
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_worker_outside_rm_requires_approval() {
        assert_worker_outside_rm_asks(
            crate::config::SandboxMode::Relaxed,
            crate::sandbox_spawn::AuthenticatedPrincipal::RouteBind {
                trust: crate::sandbox_spawn::PrincipalTrust::Untrusted,
                route_channel: 9,
                route_epoch: 4,
                project_root: std::env::current_dir().unwrap(),
                harness: "mcp:test".to_string(),
                session_id: "sandbox-spawn-test".to_string(),
                principal_id: Some("unverified".to_string()),
            },
        );
    }

    #[cfg(unix)]
    fn worker_permission_test_plan(
        mode: crate::config::SandboxMode,
        project: &Path,
        storage: &Path,
    ) -> crate::sandbox_spawn::SpawnPlan {
        let mut profile = crate::sandbox_profile::SandboxProfile::build(
            vec![project.to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            storage.to_path_buf(),
        )
        .unwrap();
        profile.mode = mode;
        // Never execute the requested rm even if the approval guard regresses.
        crate::sandbox_spawn::SpawnPlan::launcher_for_test(profile, PathBuf::from("/bin/false"))
    }

    #[cfg(unix)]
    #[test]
    fn strict_worker_uncontained_plan_requires_approval() {
        use crate::config::SandboxMode;
        use crate::sandbox_spawn::{with_spawn_plan_for_test, with_worker_jail};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.profile.worker = SandboxMode::Strict;
            config.bash_permissions = true;
        });
        let request = spawn_test_request("strict-uncontained", "rm /outside-project/file", false);
        for mode in [SandboxMode::Off, SandboxMode::Relaxed] {
            let plan = worker_permission_test_plan(mode, project.path(), storage.path());
            let response = with_worker_jail(true, || {
                with_spawn_plan_for_test(plan, || handle(&request, &ctx))
            });
            assert_eq!(
                response.data["code"], ERROR_PERMISSION_REQUIRED,
                "{response:?}"
            );
            assert!(!response.data["asks"].as_array().unwrap().is_empty());
            assert_eq!(
                ctx.bash_background().try_health_counts().unwrap().running,
                0
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn strict_worker_contained_plan_keeps_scanner_report_only() {
        use crate::config::SandboxMode;
        use crate::sandbox_spawn::{with_spawn_plan_for_test, with_worker_jail};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.profile.worker = SandboxMode::Strict;
            config.bash_permissions = true;
        });
        let request = spawn_test_request("strict-contained", "rm /outside-project/file", false);
        let plan = worker_permission_test_plan(SandboxMode::Strict, project.path(), storage.path());
        let response = with_worker_jail(true, || {
            with_spawn_plan_for_test(plan, || handle(&request, &ctx))
        });
        assert!(response.success, "{response:?}");
        let task = response.data["task_id"].as_str().unwrap();
        assert!(!read_record(&ctx, task).scanner_report.is_empty());
        stop_spawned_test_task(&ctx, &response);
    }

    #[cfg(unix)]
    mod optional_broker_tests {
        include!("bash_optional_broker_tests.rs");
    }

    #[cfg(unix)]
    #[test]
    fn host_request_returns_exact_escalation_ask_and_untrusted_request_is_denied() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.profile.head = crate::config::SandboxMode::Strict
        });
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
            config.sandbox.profile.head = crate::config::SandboxMode::Strict;
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
            "sandbox profile strict is not enforced on Windows; select sandbox.profile off or run on macOS/Linux"
        );
        assert!(observations.is_empty(), "no child spawn may be resolved");
    }

    #[cfg(unix)]
    #[test]
    fn launcher_test_plan_wraps_the_spawned_command_line() {
        use std::fs;
        use std::time::{Duration, Instant};

        use crate::sandbox_profile::SandboxProfile;
        use crate::sandbox_spawn::{with_spawn_plan_for_test, SpawnPlan};

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let args_log = project.path().join("launcher-args.txt");
        let launcher_script = project.path().join("sandbox-launch");
        let launcher = PathBuf::from("/bin/sh");
        crate::executable_cache::write_executable_with_mode(
            &launcher_script,
            br#"printf '%s\n' "$0" "$@" > "$BASHENV_TEST_LAUNCH_ARGS.tmp"
mv -f "$BASHENV_TEST_LAUNCH_ARGS.tmp" "$BASHENV_TEST_LAUNCH_ARGS"
[ "$1" = "--profile-json" ] || exit 92
[ -n "$2" ] || exit 93
[ "$3" = "--failure-fd" ] || exit 95
[ "$5" = "--" ] || exit 94
shift 5
exec "$@"
"#,
            0o700,
        )
        .unwrap();

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
                "env": { "BASHENV_TEST_LAUNCH_ARGS": args_log.to_string_lossy() },
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
        assert_eq!(lines[3], "--failure-fd");
        assert_eq!(lines[4], crate::sandbox_spawn::CHILD_FAILURE_FD.to_string());
        assert_eq!(lines[5], "--");
        assert!(lines.len() >= 8, "wrapped argv missing target: {lines:?}");

        stop_spawned_test_task(&ctx, &response);
    }

    #[cfg(unix)]
    #[test]
    fn launcher_init_failure_is_terminal_without_unsandboxed_retry() {
        use std::fs;
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
        crate::executable_cache::write_executable_with_mode(
            &launcher_script,
            format!(
                "printf 'spawn\\n' >> '{}'\necho 'sandbox_unavailable: test backend failed' >&2\nprintf %s sandbox_unavailable >&\"$4\"\nexit 78\n",
                launcher_spawns.display()
            )
            .as_bytes(),
            0o700,
        )
        .unwrap();

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
        use crate::sandbox_spawn::{
            clear_sandbox_spawn_test_seam, install_sandbox_spawn_test_seam,
            sandbox_spawn_test_observations, with_authenticated_principal, AuthenticatedPrincipal,
            PrincipalTrust, RequestedSandboxTier,
        };

        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = spawn_test_context(project.path(), storage.path());
        ctx.update_config(|config| {
            config.sandbox.profile.head = crate::config::SandboxMode::Strict;
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
    #[cfg(unix)]
    #[path = "bash_env_tests.rs"]
    mod bash_env_tests;
}
