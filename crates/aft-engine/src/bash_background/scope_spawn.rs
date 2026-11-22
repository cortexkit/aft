use super::*;

/// Spawn a detached background bash child process.
///
/// On Unix this is a single spawn against `/bin/sh`. On Windows it walks
/// `WindowsShell::shell_candidates()` (pwsh.exe → powershell.exe →
/// cmd.exe) and retries with the next candidate when the previous one
/// fails to spawn with `NotFound` — the same runtime safety net the
/// foreground bash path has, so callers landing on cmd.exe
/// fallback can also use background bash. The wrapper script is
/// regenerated per attempt because PowerShell wrappers embed the shell
/// binary by name; the stdout/stderr capture handles are also reopened
/// per attempt because `Command::spawn()` consumes them.
///
/// Errors other than `NotFound` (PermissionDenied, OutOfMemory, etc.)
/// return immediately without retry — they indicate a problem with the
/// resolved shell that retrying with a different shell won't fix.
pub(super) fn spawn_detached_child(
    spawn_plan: &SpawnPlan,
    command: &str,
    shell: super::super::BashShell,
    shell_path: &Path,
    paths: &TaskPaths,
    workdir: &Path,
    env: &HashMap<String, String>,
    io_handles: &mut TaskIoHandles,
    capture_pipeline_status: bool,
    memory_gated: bool,
    scope: Option<&super::super::task_scope::ScopeLaunch>,
    #[cfg(unix)] identity_gate: LocalIdentityGate,
) -> Result<Child, String> {
    #[cfg(windows)]
    let _ = capture_pipeline_status;
    #[cfg(not(windows))]
    let _ = (command, shell, memory_gated);
    #[cfg(not(windows))]
    {
        use std::os::fd::AsRawFd;

        let stdout = io_handles
            .clone_file(TaskArtifact::Stdout)
            .map_err(|e| format!("failed to clone stdout capture handle: {e}"))?;
        let stderr = io_handles
            .clone_file(TaskArtifact::Stderr)
            .map_err(|e| format!("failed to clone stderr capture handle: {e}"))?;
        let prepared = spawn_plan
            .prepared_task()
            .ok_or_else(|| "background task payload was not prepared".to_string())?;
        let payload = prepared.invocation()?;
        let exit = io_handles
            .inheritable_file(TaskArtifact::Exit)
            .map_err(|e| format!("failed to inherit exit marker handle: {e}"))?;
        let failure = io_handles
            .inheritable_file(TaskArtifact::SandboxUnavailable)
            .map_err(|e| format!("failed to inherit sandbox failure marker handle: {e}"))?;
        let shell_path = spawn_plan.host_shell_path().unwrap_or(shell_path);
        let pipeline_shell = super::super::process::pipeline_shell_kind(shell_path).unwrap_or("");
        // Native launchers disable path-based pipeline capture. Ordinary/host
        // children can publish in io after the user's pipeline exits without
        // giving native shells a path grant to their capture files.
        let pipeline_status_path = if capture_pipeline_status {
            paths.pipeline_status.as_os_str().to_owned()
        } else {
            OsString::new()
        };
        let base_executable = PathBuf::from("/bin/sh");
        let base_args = vec![
            OsString::from("-c"),
            payload.wrapper_text.clone(),
            OsString::from("aft-payload-wrapper"),
            shell_path.as_os_str().to_os_string(),
            payload.command_text.clone(),
            OsString::from(crate::sandbox_spawn::CHILD_EXIT_FD.to_string()),
            pipeline_status_path,
            OsString::from(pipeline_shell),
            OsString::from(match identity_gate {
                LocalIdentityGate::Required => "wait-for-identity",
                LocalIdentityGate::CommittedFallback => "",
            }),
            #[cfg(target_os = "linux")]
            super::super::process::oom_priority_marker(spawn_plan.temp_dir(), &paths.io_dir)
                .into_os_string(),
        ];
        let (executable, args) = if let Some(scope) = scope {
            scope.argv(&base_executable, &base_args)
        } else {
            (base_executable, base_args)
        };
        let (mut child_command, profile_handle) = crate::sandbox_spawn::detached_command_for_plan(
            spawn_plan,
            executable.as_os_str(),
            &args,
            &paths.json,
            crate::sandbox_spawn::CHILD_EXIT_FD,
            crate::sandbox_spawn::CHILD_FAILURE_FD,
        )?;
        child_command.current_dir(workdir);
        crate::agent_child_env::apply_to_command(&mut child_command, env);
        crate::sandbox_spawn::apply_sandbox_environment(spawn_plan, &mut child_command, env);
        let child = crate::sandbox_spawn::spawn_detached_for_plan(
            spawn_plan,
            &mut child_command,
            env,
            &crate::posix_spawn::PipeIo {
                gated_stdin: matches!(identity_gate, LocalIdentityGate::Required),
                stdout: &stdout,
                stderr: &stderr,
                exit: exit.as_raw_fd(),
                failure: failure.as_raw_fd(),
            },
        )
        .map_err(|error| {
            super::super::format_spawn_failure(
                "failed to spawn background bash command",
                Path::new(child_command.get_program()),
                child_command.get_current_dir().unwrap_or(workdir),
                error,
            )
        });
        drop((payload, exit, failure, profile_handle));
        child
    }
    #[cfg(windows)]
    {
        let _ = (shell_path, scope);
        use crate::windows_shell::shell_candidates;
        match spawn_plan {
            SpawnPlan::Unsandboxed | SpawnPlan::Unenforced { .. } | SpawnPlan::Host { .. } => {}
            SpawnPlan::Refused { code, .. } => return Err((*code).to_string()),
            SpawnPlan::Launcher { .. } => return Err("sandbox_unavailable".to_string()),
        }
        // Spawn priority: pwsh → powershell → git-bash → cmd. Same as the
        // legacy foreground bash spawn path. v0.20 routes ALL bash through
        // this background spawn helper, including foreground tool calls
        // where the model writes PowerShell-syntax (`$var = ...`,
        // `Start-Sleep`, `Add-Content`) — those fail outright under cmd.
        // The earlier v0.18-era cmd-first override worked around a
        // PowerShell detached-output bug; that bug is fixed at the
        // process-flag layer (CREATE_NO_WINDOW instead of DETACHED_PROCESS,
        // see flag block below), so we no longer need to misroute PS
        // commands through cmd.
        let candidates: Vec<crate::windows_shell::WindowsShell> = if shell.is_powershell() {
            vec![crate::windows_shell::WindowsShell::Pwsh]
        } else {
            shell_candidates()
        };
        // Win32 process creation flags. We try with CREATE_BREAKAWAY_FROM_JOB
        // first (so the bg child outlives the AFT process when AFT is killed),
        // then fall back without it for environments where the parent is in a
        // Job Object that doesn't grant `JOB_OBJECT_LIMIT_BREAKAWAY_OK`. CI
        // runners (GitHub Actions windows-2022) and some MDM-managed corp
        // environments hit this — `CreateProcess` returns Access Denied (5).
        // Without breakaway, the child still runs detached but will be torn
        // down with the parent if the parent process group is signaled.
        //
        // CREATE_NO_WINDOW avoids a visible console while retaining the
        // hidden console services PowerShell needs for reliable redirected
        // stdout/stderr. DETACHED_PROCESS can drop redirected output.
        const FLAG_CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const FLAG_CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const FLAG_CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let with_breakaway =
            FLAG_CREATE_NO_WINDOW | FLAG_CREATE_NEW_PROCESS_GROUP | FLAG_CREATE_BREAKAWAY_FROM_JOB;
        let without_breakaway = FLAG_CREATE_NO_WINDOW | FLAG_CREATE_NEW_PROCESS_GROUP;
        use crate::windows_shell::{CandidateFailure, PipedAttemptError};
        let attempt = crate::windows_shell::spawn_piped_candidates(
            &candidates,
            |shell, breakaway| {
                let flags = if breakaway {
                    with_breakaway
                } else {
                    without_breakaway
                };
                // Each attempt consumes its Stdio wrappers, so clone the retained
                // exclusive capture handles rather than reopening output paths.
                let stdout = io_handles.clone_file(TaskArtifact::Stdout).map_err(|e| {
                    PipedAttemptError::Setup(format!("failed to clone stdout capture handle: {e}"))
                })?;
                let stderr = io_handles.clone_file(TaskArtifact::Stderr).map_err(|e| {
                    PipedAttemptError::Setup(format!("failed to clone stderr capture handle: {e}"))
                })?;
                let (mut cmd, waiter_environment, memory_file_gate) = detached_shell_command_for(
                    shell.clone(),
                    command,
                    &paths.exit,
                    paths,
                    flags,
                    memory_gated,
                    workdir,
                    env,
                )
                .map_err(PipedAttemptError::Setup)?;
                cmd.current_dir(workdir)
                    .stdin(if memory_gated && !memory_file_gate {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    })
                    .stdout(Stdio::from(stdout))
                    .stderr(Stdio::from(stderr));
                crate::agent_child_env::apply_to_command(&mut cmd, env);
                crate::sandbox_spawn::apply_sandbox_environment(spawn_plan, &mut cmd, env);
                if let Some((key, value)) = waiter_environment {
                    cmd.env(key, value);
                }
                if let Some((key, value)) = shell.wrapper_environment(command) {
                    cmd.env(key, value);
                }
                cmd.spawn().map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    crate::slog_warn!("background bash spawn: {} returned NotFound at runtime — trying next candidate", shell.binary());
                } else if breakaway && error.raw_os_error() == Some(5) {
                    crate::slog_warn!("background bash spawn: CREATE_BREAKAWAY_FROM_JOB rejected with Access Denied — retrying {} without breakaway", shell.binary());
                }
                PipedAttemptError::Spawn(error)
            })
            },
        );
        match attempt {
            Ok((child, index, without_breakaway)) => {
                if index > 0 {
                    crate::slog_warn!("background bash spawn fell back to {} after {} earlier candidate(s) failed; \
                        the cached PATH probe disagreed with runtime spawn — likely PATH \
                        inheritance, antivirus / AppLocker / Defender ASR, or sandbox policy.", candidates[index].binary(), index);
                }
                if without_breakaway {
                    crate::slog_warn!(
                        "background bash spawn: CREATE_BREAKAWAY_FROM_JOB rejected \
                        (likely a restrictive Job Object — CI sandbox or MDM policy). \
                        Spawned without breakaway; the bg task will be torn down if the \
                        AFT process group is killed."
                    );
                }
                Ok(child.into())
            }
            Err(CandidateFailure::Stopped {
                error: PipedAttemptError::Setup(message),
                ..
            }) => Err(message),
            Err(CandidateFailure::Stopped {
                index,
                error: PipedAttemptError::Spawn(error),
            }) => Err(super::super::format_spawn_failure(
                &format!(
                    "failed to spawn background bash command via {}",
                    candidates[index].binary()
                ),
                Path::new(candidates[index].binary().as_ref()),
                workdir,
                error,
            )),
            Err(CandidateFailure::Exhausted { last }) => {
                let names = candidates
                    .iter()
                    .map(|shell| shell.binary())
                    .collect::<Vec<_>>();
                let context = format!("failed to spawn background bash command: no Windows shell could be spawned. PATH-probed candidates: {names:?}");
                match last {
                    Some((index, PipedAttemptError::Spawn(error))) => {
                        Err(super::super::format_spawn_failure(
                            &context,
                            Path::new(candidates[index].binary().as_ref()),
                            workdir,
                            format!("{}: {error}", candidates[index].binary()),
                        ))
                    }
                    _ => Err(format!("{context}: no candidates were attempted")),
                }
            }
        }
    }
}
