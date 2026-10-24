//! Shared Windows shell selection for foreground and background bash commands.
//!
//! Mirrors OpenCode's resolver:
//!   1. `$SHELL` env var (typically points at git-bash on Windows dev setups).
//!   2. `pwsh.exe` (PowerShell 7+).
//!   3. `powershell.exe` (Windows PowerShell 5.1).
//!   4. Git-for-Windows `bash.exe` discovered next to `git` on PATH (catches
//!      users who installed Git for Windows but never set `$SHELL`).
//!   5. `cmd.exe` (universal floor — always reachable on every Windows SKU).
//!
//! POSIX shells (bash, sh, zsh, ksh, dash) are invoked as `<shell> -c <cmd>`
//! the same way Unix does. PowerShell variants take their `-Command` shape;
//! cmd.exe takes `/D /C`.
//!
//! Compiled on all platforms so the cross-platform retry-decision unit
//! tests in `commands::bash::try_spawn_with_fallback` (test-only — see the
//! Windows foreground bash path in `crate::commands::bash`) can run on
//! macOS/Linux dev machines. The production Windows background spawn path
//! at `bash_background::registry::detached_shell_command_for` is the live
//! caller.

#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// POSIX shells that can be invoked as `<shell> -c <command>`. Matches
/// OpenCode's `POSIX` set in `packages/opencode/src/shell/shell.ts`.
const POSIX_NAMES: &[&str] = &["bash", "sh", "zsh", "ksh", "dash"];

/// Environment variable through which the cmd background wrapper receives the
/// user command; see [`WindowsShell::wrapper_script`].
pub(crate) const CMD_WRAPPER_COMMAND_ENV: &str = "AFT_CMD_WRAPPER_COMMAND";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WindowsShell {
    /// PowerShell 7+ (cross-platform). Supports `&&` pipeline operator.
    Pwsh,
    /// Windows PowerShell 5.1 (legacy, still default on most Windows desktops
    /// but **absent on Windows 11 IoT Enterprise LTSC SKUs** — issue #27).
    /// Does NOT support `&&` in pipelines (PS 7+ only feature).
    Powershell,
    /// `cmd.exe` — the universal fallback. Present on every Windows SKU.
    /// Supports `&&` and `||` natively. Lacks PowerShell's piping/cmdlets but
    /// handles bash-style chained shell invocations correctly.
    Cmd,
    /// User-supplied POSIX shell — typically Git for Windows' bash.exe,
    /// resolved either from `$SHELL` or auto-detected next to `git` on PATH.
    /// Invoked as `<binary> -c <command>` exactly like a Unix shell, so
    /// agents that emit bash-syntax commands (`cmd /c "foo"`, `find . -name`,
    /// quoting with backslash-escapes, etc.) work the same way they would
    /// in a real bash session. The string is the absolute path to the binary.
    Posix(PathBuf),
}

impl WindowsShell {
    /// Binary path to spawn. PowerShell/cmd variants resolve via PATH lookup;
    /// `Posix` carries an already-absolute path resolved at candidate-build
    /// time so we don't accidentally pick a different bash.exe later.
    pub(crate) fn binary(&self) -> std::borrow::Cow<'_, str> {
        match self {
            WindowsShell::Pwsh => std::borrow::Cow::Borrowed("pwsh.exe"),
            WindowsShell::Powershell => std::borrow::Cow::Borrowed("powershell.exe"),
            WindowsShell::Cmd => std::borrow::Cow::Borrowed("cmd.exe"),
            WindowsShell::Posix(path) => std::borrow::Cow::Owned(path.display().to_string()),
        }
    }

    /// Argument vector to pass alongside the user's command string.
    /// PowerShell variants take `-Command <string>`; cmd takes `/D /C <string>`
    /// (`/D` disables AutoRun macros that could otherwise inject env-trust
    /// behavior into our isolated invocation); POSIX shells take `-c <string>`.
    pub(crate) fn args<'a>(&'a self, command: &'a str) -> Vec<&'a str> {
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => vec![
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            WindowsShell::Cmd => vec!["/D", "/C", command],
            WindowsShell::Posix(_) => vec!["-c", command],
        }
    }

    /// Args for invoking a wrapper file under a PTY-attached shell.
    /// Returns owned strings so callers can pass temporary wrapper paths.
    pub(crate) fn pty_wrapper_args(&self, wrapper_path: &Path) -> Vec<String> {
        match self {
            // Disable registry AutoRun before cmd reads and executes the wrapper.
            WindowsShell::Cmd => vec!["/D".into(), "/c".into(), wrapper_path.display().to_string()],
            WindowsShell::Pwsh | WindowsShell::Powershell => vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-File".into(),
                wrapper_path.display().to_string(),
            ],
            WindowsShell::Posix(_) => vec![wrapper_path.display().to_string()],
        }
    }

    /// Build a `Command` that runs an inline background wrapper script.
    ///
    /// Production background bash now writes wrappers to `.bat` / `.ps1` temp
    /// files and invokes those files directly, so paths containing `!` remain
    /// literal and cmd.exe does not have to parse a long inline wrapper. This
    /// helper is retained for tests that exercise the legacy shell-arg shape.
    #[allow(dead_code)]
    pub(crate) fn bg_command(&self, wrapper: &str) -> Command {
        let binary = self.binary();
        let mut cmd = Command::new(binary.as_ref());
        // PowerShell variants accept the wrapper string directly via
        // `-Command`; the shell's `-Command` parser is generally happy
        // with embedded quotes when the script doesn't contain literal
        // `"` (we use only single quotes in the PS wrapper for that
        // reason — see `wrapper_script` for `Pwsh|Powershell`).
        //
        // For cmd.exe the wrapper contains `cmd_quote`-quoted paths
        // which CAN survive cmd's /C parser, but only if we add `/S`
        // to enable simple-quote-stripping mode. Even with /S the
        // interaction with Rust's std-lib argument quoting is fragile,
        // so we rely on `args()` for cmd and live with the constraints.
        //
        // `/D` skips AutoRun macros; `/S` enables simple quote-stripping.
        //
        // POSIX shells (git-bash etc.) take `-c <wrapper>` and execute
        // the wrapper as a normal shell script — the wrapper's `trap` and
        // `printf "$?"` mechanics are POSIX-standard, so no special flags.
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => {
                cmd.args(self.args(wrapper));
            }
            WindowsShell::Cmd => {
                cmd.args(["/D", "/S", "/C", wrapper]);
            }
            WindowsShell::Posix(_) => {
                cmd.args(["-c", wrapper]);
            }
        }
        cmd
    }

    /// Wrap a background command so the child writes its own exit marker.
    ///
    /// On Windows the sandbox is never active (`native_sandbox_enforced` is
    /// `cfg!(unix)`-gated; strict refuses and relaxed reports unenforced denies),
    /// meaning every Windows bash child runs unconfined with full user rights
    /// and is authoritative for its own exit code. Windows also has no zombie
    /// reaping, so once the spawning daemon's child handle closes across a
    /// detach/rebind the exit code is unrecoverable daemon-side — only a
    /// child-side write records it. The write goes into the child-writable
    /// `io/` plane via temp-file + `Move-Item`/`move` rename: an in-place write
    /// is refused because the daemon retains the pre-opened `io/exit` handle,
    /// but the rename succeeds (the retained handle is opened with
    /// `FILE_SHARE_DELETE`) and atomically swaps in a fully-written file, so the
    /// daemon's fresh open in `read_exit_marker` never observes a partial value.
    /// (Verified empirically on native Windows.)
    ///
    /// PowerShell and cmd do not expose a portable per-segment pipeline status
    /// array, so Windows wrappers intentionally record only the overall exit
    /// code. Unix bash/zsh pipeline diagnostics live in the Unix payload wrapper.
    pub(crate) fn wrapper_script(&self, command: &str, exit_path: &Path) -> String {
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => {
                // CRITICAL: no literal double-quotes — inner `"` breaks the
                // outer `-File` parse on some Windows console hosts. Use only
                // single-quoted strings and `+` concat.
                let exit_path = powershell_single_quote(&exit_path.display().to_string());
                let command = powershell_single_quote(command);
                // Invoke-Expression can report success after a command-not-found
                // error. Catch invocation errors before publishing a success
                // marker, even if an earlier native command set LASTEXITCODE.
                format!(
                    concat!(
                        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); ",
                        "$OutputEncoding = [Console]::OutputEncoding; ",
                        "$exitPath = {exit_path}; ",
                        "$tmpPath = $exitPath + '.tmp.' + $PID; ",
                        "$global:LASTEXITCODE = $null; ",
                        "try {{ Invoke-Expression {command}; ",
                        "$success = $?; ",
                        "$nativeCode = $global:LASTEXITCODE; ",
                        "if ($null -ne $nativeCode) {{ $code = [int]$nativeCode }} ",
                        "elseif ($success) {{ $code = 0 }} ",
                        "else {{ $code = 1 }} ",
                        "}} catch {{ [Console]::Error.WriteLine($_.ToString()); $code = 1 }}; ",
                        "[System.IO.File]::WriteAllText($tmpPath, [string]$code); ",
                        "Move-Item -LiteralPath $tmpPath -Destination $exitPath -Force; ",
                        "exit $code"
                    ),
                    exit_path = exit_path,
                    command = command
                )
            }
            WindowsShell::Cmd => {
                // Written to a `.bat` invoked as `cmd /D /C <wrapper.bat>`.
                // The user command does not appear in the file: batch
                // parsing would give `%1` and unmatched `%` batch-parameter
                // meaning, and an `exit` in the command would end the
                // wrapper before it records the exit marker. Instead the
                // caller passes the command in `CMD_WRAPPER_COMMAND_ENV`
                // and a child `cmd /D /S /C` runs it with the same
                // command-line semantics as a foreground cmd command.
                // Delayed expansion inserts the value after the wrapper
                // line is parsed, so its `&`, `|`, `%` and quotes reach the
                // child unchanged. It is switched off again before the
                // marker lines so a literal `!` in their paths survives.
                let tmp_path = format!("{}.tmp", exit_path.display());
                format!(
                    concat!(
                        "@echo off\r\n",
                        "setlocal EnableDelayedExpansion\r\n",
                        "cmd.exe /D /S /C \"!{env}!\"\r\n",
                        "endlocal & set CODE=%ERRORLEVEL%\r\n",
                        "echo %CODE% > {tmp}\r\n",
                        "move /Y {tmp} {exit} > nul\r\n",
                        "exit /B %CODE%\r\n"
                    ),
                    env = CMD_WRAPPER_COMMAND_ENV,
                    tmp = batch_quote(&tmp_path),
                    exit = batch_quote(&exit_path.display().to_string())
                )
            }
            WindowsShell::Posix(shell_path) => {
                // git-bash speaks POSIX: same temp-file + `mv` rename. Single-
                // quote the user command so embedded `;`/`&`/`$` stay literal.
                let exit_str = exit_path.display().to_string();
                let tmp_path = format!("{}.tmp", exit_str);
                format!(
                    "{} -c {} ; printf '%s' \"$?\" > {} && mv {} {}",
                    posix_single_quote(&shell_path.display().to_string()),
                    posix_single_quote(command),
                    posix_single_quote(&tmp_path),
                    posix_single_quote(&tmp_path),
                    posix_single_quote(&exit_str),
                )
            }
        }
    }

    /// Environment the wrapper written by [`Self::wrapper_script`] needs at
    /// run time. Only the cmd wrapper reads its command from the environment;
    /// callers apply this after the request environment so it cannot be
    /// overridden.
    pub(crate) fn wrapper_environment<'a>(
        &self,
        command: &'a str,
    ) -> Option<(&'static str, &'a str)> {
        matches!(self, WindowsShell::Cmd).then_some((CMD_WRAPPER_COMMAND_ENV, command))
    }

    pub(crate) fn wrapper_script_bytes(&self, command: &str, exit_path: &Path) -> Vec<u8> {
        let script = self.wrapper_script(command, exit_path);
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => {
                let mut bytes = vec![0xEF, 0xBB, 0xBF];
                bytes.extend_from_slice(script.as_bytes());
                bytes
            }
            WindowsShell::Cmd | WindowsShell::Posix(_) => script.into_bytes(),
        }
    }

    pub(crate) fn memory_gated_wrapper_bytes(&self, command: &str, exit_path: &Path) -> Vec<u8> {
        let gate = match self {
            // PowerShell console reads can block during ConPTY initialization.
            // A private file gate leaves terminal input available for the command.
            Self::Pwsh | Self::Powershell => format!("$aftMemoryDeadline = [DateTime]::UtcNow.AddSeconds(30)\r\nwhile (-not (Test-Path -LiteralPath '{}')) {{ if ([DateTime]::UtcNow -ge $aftMemoryDeadline) {{ exit 125 }}; Start-Sleep -Milliseconds 10 }}\r\n", memory_gate_path(exit_path).display().to_string().replace('\'', "''")),
            Self::Cmd => "@set /p _aft_memory_ready=\r\n@if not \"%_aft_memory_ready%\"==\"aft-memory-ready\" exit /B 125\r\n".into(),
            Self::Posix(_) => "read -r _aft_memory_ready && [ \"$_aft_memory_ready\" = aft-memory-ready ] || exit 125\n".into(),
        };
        let script = format!("{gate}{}", self.wrapper_script(command, exit_path));
        let mut bytes = if matches!(self, Self::Pwsh | Self::Powershell) {
            vec![0xEF, 0xBB, 0xBF]
        } else {
            Vec::new()
        };
        bytes.extend_from_slice(script.as_bytes());
        bytes
    }

    pub(crate) fn uses_memory_file_gate(&self) -> bool {
        matches!(self, Self::Pwsh | Self::Powershell)
    }
}

fn memory_gate_path(exit_path: &Path) -> PathBuf {
    exit_path.with_extension("memory-ready")
}

pub(crate) fn release_memory_file_gate(exit_path: &Path) -> Result<(), String> {
    crate::private_storage::write(&memory_gate_path(exit_path), b"")
        .map_err(|error| format!("memory cap launch gate failed: {error}"))
}

/// All Windows shells that the PATH probe believes are reachable, returned
/// in priority order. Always non-empty on Windows because cmd.exe is the
/// floor. Order:
///
///   1. `$SHELL` env var (typically points at git-bash on Windows dev setups).
///   2. `pwsh.exe`.
///   3. `powershell.exe`.
///   4. Git-for-Windows `bash.exe` discovered next to `git` on PATH.
///   5. `cmd.exe`.
///
/// Used by the foreground bash spawn site to retry with the next candidate
/// if the first one fails to spawn at runtime. Cached after the first
/// resolve.
pub(crate) fn shell_candidates() -> Vec<WindowsShell> {
    static CACHED: OnceLock<Vec<WindowsShell>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            shell_candidates_with(
                |binary| which::which(binary).ok(),
                || std::env::var_os("SHELL").map(PathBuf::from),
            )
        })
        .clone()
}

/// Test seam for [`shell_candidates`]. The two closures let unit tests inject
/// a fake `which`-like resolver and a fake `$SHELL` env value.
///
/// `which_for(binary)` should return `Some(absolute_path)` if the binary is
/// reachable, `None` otherwise — matching the contract of `which::which`.
pub(crate) fn shell_candidates_with<W, S>(which_for: W, shell_env: S) -> Vec<WindowsShell>
where
    W: Fn(&str) -> Option<PathBuf>,
    S: FnOnce() -> Option<PathBuf>,
{
    let mut candidates: Vec<WindowsShell> = Vec::with_capacity(5);

    // 1. $SHELL env var — typically points at git-bash on Windows dev
    //    setups (`/c/Program Files/Git/bin/bash.exe` style or a normal
    //    Windows path). Mirrors OpenCode's preferred() resolution.
    //    Only honored when the named binary is recognized as POSIX
    //    (bash/sh/zsh/ksh/dash) — we don't want SHELL=cmd.exe pinning us
    //    to cmd when the user already gets cmd as the floor candidate.
    if let Some(shell_path) = shell_env() {
        if let Some(resolved) = resolve_user_shell(&shell_path, &which_for) {
            crate::slog_info!(
                "bash candidate: $SHELL = {} (POSIX, invoked as -c)",
                resolved.display()
            );
            candidates.push(WindowsShell::Posix(resolved));
        }
    }

    // 2-3. PowerShell variants.
    if which_for("pwsh.exe").is_some() {
        crate::slog_info!(
            "bash candidate: pwsh.exe (PowerShell 7+; supports && pipeline operator)"
        );
        candidates.push(WindowsShell::Pwsh);
    }
    if which_for("powershell.exe").is_some() {
        crate::slog_info!("bash candidate: powershell.exe (Windows PowerShell 5.1; && in pipelines unsupported, will surface as parse error)");
        candidates.push(WindowsShell::Powershell);
    }

    // 4. Git for Windows auto-detect — find bash.exe next to git on PATH.
    //    Catches the common case of "user installed Git for Windows but
    //    didn't set $SHELL". Skipped when $SHELL already produced a POSIX
    //    candidate (no point adding the same git-bash twice).
    let already_posix = candidates
        .iter()
        .any(|c| matches!(c, WindowsShell::Posix(_)));
    if !already_posix {
        if let Some(git_bash) = locate_git_bash(&which_for) {
            crate::slog_info!(
                "bash candidate: git-bash auto-detected at {} (POSIX, invoked as -c)",
                git_bash.display()
            );
            candidates.push(WindowsShell::Posix(git_bash));
        }
    }

    // 5. cmd.exe is always added as the floor, regardless of PATH probe
    //    result. It lives in a Windows search-path location that PATH
    //    inheritance issues, ASR rules, and sandboxing generally cannot
    //    remove. Without this floor, foreground bash retry would have
    //    nowhere to fall back to when other shells fail to spawn at runtime.
    candidates.push(WindowsShell::Cmd);

    let only_cmd = candidates.len() == 1;
    if only_cmd {
        crate::slog_warn!(
            "No bash, PowerShell, or git-bash is reachable from this \
         aft process — using cmd.exe only. This can occur even when \
         PowerShell is installed if PATH inheritance is restricted, \
         antivirus / AppLocker / Defender ASR rules block PowerShell as a \
         child process, or you're on a stripped Windows SKU. Bash-style \
         commands using && and || still work; PowerShell-only cmdlets and \
         POSIX-only commands will not. Details: \
         https://github.com/cortexkit/aft/issues/27"
        );
    }
    candidates
}

/// Resolve a `$SHELL` value into an absolute path to a POSIX shell binary,
/// or `None` if the value is unusable on Windows. Handles three input
/// shapes that show up in the wild:
///
///   - Full Windows path: `C:\Program Files\Git\bin\bash.exe`
///   - MSYS/git-bash style: `/c/Program Files/Git/bin/bash.exe` or `/usr/bin/bash`
///   - Bare name: `bash` or `bash.exe` (resolve via `which`)
///
/// Returns `None` if the resolved binary's filename isn't in `POSIX_NAMES`,
/// so that someone with `SHELL=cmd.exe` doesn't accidentally pin us to a
/// `Posix(cmd.exe)` invocation that breaks the `-c` contract.
fn resolve_user_shell<W>(raw: &Path, which_for: &W) -> Option<PathBuf>
where
    W: Fn(&str) -> Option<PathBuf>,
{
    // Convert MSYS-style /c/foo/bar paths to C:\foo\bar so std::fs::metadata
    // and Command::new can find them. Pure Windows paths and POSIX paths on
    // a MSYS root pass through with /-to-\ normalization.
    let resolved = normalize_shell_path(raw);

    // If the (possibly-normalized) path is absolute and exists on disk,
    // use it as-is. Otherwise treat it as a bare name and try PATH lookup.
    let candidate = if resolved.is_absolute() && resolved.exists() {
        resolved
    } else {
        let name = resolved.file_name()?.to_str()?.to_string();
        which_for(&name)?
    };

    if !is_posix_shell_name(&candidate) {
        crate::slog_info!(
            "$SHELL points at {} which isn't a recognized POSIX shell; \
         falling back to PowerShell/cmd resolution.",
            candidate.display()
        );
        return None;
    }
    Some(candidate)
}

/// Look for git-bash next to `git` on PATH. Mirrors OpenCode's `gitbash()`:
/// resolves `git`, then checks `<git_dir>/../../bin/bash.exe`. Returns
/// `None` if git isn't on PATH, the expected bash.exe doesn't exist, or
/// the file is empty.
fn locate_git_bash<W>(which_for: &W) -> Option<PathBuf>
where
    W: Fn(&str) -> Option<PathBuf>,
{
    let git = which_for("git.exe").or_else(|| which_for("git"))?;
    // git.exe typically lives at <install>/cmd/git.exe; bash.exe lives at
    // <install>/bin/bash.exe. The two `parent()` calls walk up from
    // `cmd/git.exe` to `<install>`, then we descend into `bin/bash.exe`.
    let candidate = git.parent()?.parent()?.join("bin").join("bash.exe");
    let metadata = std::fs::metadata(&candidate).ok()?;
    if metadata.len() == 0 {
        return None;
    }
    Some(candidate)
}

/// Normalize an MSYS / git-bash POSIX path to a Windows path, leaving
/// already-Windows paths and bare names alone. This mirrors the relevant
/// subset of OpenCode's `windowsPath()` for `$SHELL` values.
fn normalize_shell_path(raw: &Path) -> PathBuf {
    let s = raw.to_string_lossy();

    // MSYS drive-letter form: /c/Foo/Bar  ->  C:\Foo\Bar
    if let Some(rest) = s.strip_prefix('/') {
        if let Some((drive, after)) = rest.split_once('/') {
            if drive.len() == 1
                && drive
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
            {
                let drive_upper = drive.to_uppercase();
                let win = format!("{}:\\{}", drive_upper, after.replace('/', "\\"));
                return PathBuf::from(win);
            }
        }
    }

    PathBuf::from(s.as_ref())
}

/// True when the file name (without extension) is in `POSIX_NAMES`.
fn is_posix_shell_name(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    POSIX_NAMES.iter().any(|name| *name == stem)
}

#[derive(Debug)]
pub(crate) enum CandidateFailure<E> {
    Stopped { index: usize, error: E },
    Exhausted { last: Option<(usize, E)> },
}

enum Attempt<E> {
    Next(E),
    Stop(E),
}

fn spawn_candidates<C, E>(
    candidates: &[WindowsShell],
    mut spawn: impl FnMut(&WindowsShell) -> Result<C, Attempt<E>>,
) -> Result<(C, usize), CandidateFailure<E>> {
    let mut last = None;
    for (index, shell) in candidates.iter().enumerate() {
        match spawn(shell) {
            Ok(child) => return Ok((child, index)),
            Err(Attempt::Next(error)) => last = Some((index, error)),
            Err(Attempt::Stop(error)) => return Err(CandidateFailure::Stopped { index, error }),
        }
    }
    Err(CandidateFailure::Exhausted { last })
}

#[derive(Debug)]
pub(crate) enum PipedAttemptError {
    Setup(String),
    Spawn(std::io::Error),
}

pub(crate) fn spawn_piped_candidates<C>(
    candidates: &[WindowsShell],
    mut spawn: impl FnMut(&WindowsShell, bool) -> Result<C, PipedAttemptError>,
) -> Result<(C, usize, bool), CandidateFailure<PipedAttemptError>> {
    spawn_candidates(candidates, |shell| {
        for breakaway in [true, false] {
            match spawn(shell, breakaway) {
                Ok(child) => return Ok((child, !breakaway)),
                Err(PipedAttemptError::Spawn(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Err(Attempt::Next(PipedAttemptError::Spawn(error)));
                }
                Err(PipedAttemptError::Spawn(error))
                    if breakaway && error.raw_os_error() == Some(5) =>
                {
                    // A restrictive Job Object can reject breakaway even though
                    // this shell exists. Retry the same shell without that flag.
                    continue;
                }
                Err(error) => return Err(Attempt::Stop(error)),
            }
        }
        unreachable!("the without-breakaway attempt always returns")
    })
    .map(|((child, without_breakaway), index)| (child, index, without_breakaway))
}

pub(crate) enum PtyAttemptError {
    Wrapper(String),
    Spawn(String),
}

pub(crate) fn spawn_pty_candidates<C>(
    candidates: &[WindowsShell],
    mut spawn: impl FnMut(&WindowsShell) -> Result<C, PtyAttemptError>,
) -> Result<(C, usize), CandidateFailure<String>> {
    spawn_candidates(candidates, |shell| match spawn(shell) {
        Ok(child) => Ok(child),
        Err(PtyAttemptError::Wrapper(error)) => Err(Attempt::Next(error)),
        Err(PtyAttemptError::Spawn(error)) => {
            let message = format!("{shell:?}: {error}");
            // portable-pty reports Windows launch failures as text. Preserve
            // its existing missing-shell policy, separately from piped I/O errors.
            if message.contains("NotFound") || message.contains("not recognized") {
                Err(Attempt::Next(message))
            } else {
                Err(Attempt::Stop(message))
            }
        }
    })
}

fn powershell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Single-quote a value for POSIX `sh -c`, escaping inner single quotes via
/// the standard `'\''` close-and-reopen idiom. Used by the bg-bash wrapper
/// for [`WindowsShell::Posix`] (git-bash) and matches the Unix wrapper's
/// quoting contract.
#[cfg_attr(not(windows), allow(dead_code))]
fn posix_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// Used by `wrapper_script` for `WindowsShell::Cmd`; that wrapper is
// only invoked from `bash_background::registry::detached_shell_command_for`
// which is `#[cfg(windows)]`. The function compiles on all platforms so
// `wrapper_script` stays cross-platform-testable.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Quote a literal path for a line of a `.bat` file. Batch files expand `%`
/// even inside quotes, so a literal percent sign must be written as `%%`.
fn batch_quote(value: &str) -> String {
    cmd_quote(value).replace('%', "%%")
}

#[cfg(test)]
mod tests {
    #[test]
    fn production_notfound_retry_skips_breakaway_and_advances_candidate() {
        let candidates = [super::WindowsShell::Pwsh, super::WindowsShell::Powershell];
        let mut attempts = Vec::new();
        let (child, index, without) =
            super::spawn_piped_candidates(&candidates, |shell, breakaway| {
                attempts.push((shell.clone(), breakaway));
                if matches!(shell, super::WindowsShell::Pwsh) {
                    Err(super::PipedAttemptError::Spawn(
                        std::io::ErrorKind::NotFound.into(),
                    ))
                } else {
                    Ok(7)
                }
            })
            .unwrap();
        assert_eq!((child, index, without), (7, 1, false));
        assert_eq!(
            attempts,
            vec![
                (super::WindowsShell::Pwsh, true),
                (super::WindowsShell::Powershell, true)
            ]
        );
    }

    #[test]
    fn production_breakaway_denial_retries_only_the_same_shell() {
        let candidates = [super::WindowsShell::Pwsh, super::WindowsShell::Cmd];
        let mut attempts = Vec::new();
        let (_, index, without) = super::spawn_piped_candidates(&candidates, |shell, breakaway| {
            attempts.push((shell.clone(), breakaway));
            if breakaway {
                Err(super::PipedAttemptError::Spawn(
                    std::io::Error::from_raw_os_error(5),
                ))
            } else {
                Ok(7)
            }
        })
        .unwrap();
        assert_eq!((index, without), (0, true));
        assert_eq!(
            attempts,
            vec![
                (super::WindowsShell::Pwsh, true),
                (super::WindowsShell::Pwsh, false)
            ]
        );
    }

    #[test]
    fn production_nonmissing_error_after_breakaway_stops_candidates() {
        let candidates = [super::WindowsShell::Pwsh, super::WindowsShell::Cmd];
        let mut count = 0;
        let result: Result<((), usize, bool), _> =
            super::spawn_piped_candidates(&candidates, |_, breakaway| {
                count += 1;
                Err(super::PipedAttemptError::Spawn(if breakaway {
                    std::io::Error::from_raw_os_error(5)
                } else {
                    std::io::ErrorKind::PermissionDenied.into()
                }))
            });
        assert!(matches!(
            result,
            Err(super::CandidateFailure::Stopped { index: 0, .. })
        ));
        assert_eq!(count, 2);
    }

    #[test]
    fn production_pty_wrapper_and_missing_shell_failures_advance() {
        let candidates = [
            super::WindowsShell::Pwsh,
            super::WindowsShell::Powershell,
            super::WindowsShell::Cmd,
        ];
        let mut count = 0;
        let (child, index) = super::spawn_pty_candidates(&candidates, |shell| {
            count += 1;
            match shell {
                super::WindowsShell::Pwsh => Err(super::PtyAttemptError::Wrapper(
                    "write wrapper failed".into(),
                )),
                super::WindowsShell::Powershell => {
                    Err(super::PtyAttemptError::Spawn("NotFound".into()))
                }
                _ => Ok(7),
            }
        })
        .unwrap();
        assert_eq!((child, index, count), (7, 2, 3));
    }

    #[test]
    fn production_pty_nonmissing_failure_stops_candidates() {
        let candidates = [super::WindowsShell::Pwsh, super::WindowsShell::Cmd];
        let mut count = 0;
        let result: Result<((), usize), _> = super::spawn_pty_candidates(&candidates, |_| {
            count += 1;
            Err(super::PtyAttemptError::Spawn("AccessDenied".into()))
        });
        assert!(matches!(
            result,
            Err(super::CandidateFailure::Stopped { index: 0, .. })
        ));
        assert_eq!(count, 1);
    }
    use super::*;

    #[cfg(windows)]
    fn assert_powershell_wrapper(shell: WindowsShell, command: &str, code: i32, stdout: &str) {
        let Ok(binary) = which::which(shell.binary().as_ref()) else {
            eprintln!("skipping {shell:?}: shell is not installed");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("wrapper.ps1");
        let marker = temp.path().join("exit.txt");
        std::fs::write(&wrapper, shell.wrapper_script_bytes(command, &marker)).unwrap();
        let output = Command::new(binary)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&wrapper)
            .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{shell:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(marker).unwrap().trim(),
            code.to_string()
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), stdout);
        if code == 1 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("aft_missing_command_7961"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn pwsh_wrapper_reports_command_not_found() {
        for command in [
            "aft_missing_command_7961",
            "cmd.exe /d /c exit 42; aft_missing_command_7961",
        ] {
            assert_powershell_wrapper(WindowsShell::Pwsh, command, 1, "");
        }
    }

    #[cfg(windows)]
    #[test]
    fn powershell_5_wrapper_reports_command_not_found() {
        for command in [
            "aft_missing_command_7961",
            "cmd.exe /d /c exit 42; aft_missing_command_7961",
        ] {
            assert_powershell_wrapper(WindowsShell::Powershell, command, 1, "");
        }
    }

    #[cfg(windows)]
    #[test]
    fn pwsh_wrapper_preserves_native_output_and_exit_code() {
        assert_powershell_wrapper(
            WindowsShell::Pwsh,
            "cmd.exe /d /c 'echo hello & exit 42'",
            42,
            "hello",
        );
    }

    #[cfg(windows)]
    #[test]
    fn powershell_5_wrapper_preserves_native_output_and_exit_code() {
        assert_powershell_wrapper(
            WindowsShell::Powershell,
            "cmd.exe /d /c 'echo hello & exit 42'",
            42,
            "hello",
        );
    }

    #[test]
    fn cmd_wrapper_keeps_the_command_out_of_batch_parsing() {
        let script = WindowsShell::Cmd
            .wrapper_script("echo %1 & exit 3", Path::new(r"C:\Temp\100%\bash.exit"));
        assert!(!script.contains("echo %1"), "{script}");
        assert!(
            script.contains(&format!("cmd.exe /D /S /C \"!{CMD_WRAPPER_COMMAND_ENV}!\"")),
            "{script}"
        );
        assert!(script.contains(r"C:\Temp\100%%\bash.exit"), "{script}");
        assert!(!script.contains(r"100%\"), "{script}");
        assert_eq!(
            WindowsShell::Cmd.wrapper_environment("echo hi"),
            Some((CMD_WRAPPER_COMMAND_ENV, "echo hi"))
        );
        assert_eq!(WindowsShell::Pwsh.wrapper_environment("echo hi"), None);
    }

    /// The wrapper must run a command exactly as `cmd /D /S /C <command>`
    /// would: batch-parameter text and unmatched percent signs stay literal,
    /// and an `exit` in the command still lets the wrapper write its marker.
    #[cfg(windows)]
    #[test]
    fn cmd_wrapper_matches_direct_command_semantics_and_writes_marker() {
        use std::process::Command;
        let temp = tempfile::tempdir().unwrap();
        let wrapper = temp.path().join("wrapper.bat");
        let marker = temp.path().join("exit%AFT_TEST_PERCENT%!.txt");
        for command in [
            "echo %1",
            "echo 100%",
            "exit 37",
            "echo caret^^ bang! & echo second",
        ] {
            let _ = std::fs::remove_file(&marker);
            let expected = Command::new("cmd.exe")
                .args(["/D", "/S", "/C", command])
                .env("AFT_TEST_PERCENT", "expanded")
                .output()
                .unwrap();
            std::fs::write(
                &wrapper,
                WindowsShell::Cmd.wrapper_script_bytes(command, &marker),
            )
            .unwrap();
            let (key, value) = WindowsShell::Cmd.wrapper_environment(command).unwrap();
            let actual = Command::new("cmd.exe")
                .args(["/D", "/C"])
                .arg(&wrapper)
                .env(key, value)
                .env("AFT_TEST_PERCENT", "expanded")
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&expected.stdout),
                "direct command semantics: {command}"
            );
            assert_eq!(
                actual.status.code(),
                expected.status.code(),
                "status: {command}"
            );
            assert_eq!(
                std::fs::read_to_string(&marker)
                    .unwrap()
                    .trim()
                    .parse::<i32>()
                    .unwrap(),
                expected.status.code().unwrap(),
                "marker: {command}"
            );
        }
    }

    /// Helper: build a `which`-like closure that returns Some for the
    /// listed binaries (mapping each to a synthetic absolute path) and
    /// None for everything else. The synthetic path layout matches a
    /// realistic Git for Windows install when `git.exe` is present,
    /// so [`locate_git_bash`] can synthesize a sibling bash.exe path —
    /// but the returned path won't exist on disk, so `locate_git_bash`
    /// will bail at the metadata check, which is what the no-Posix-via-
    /// auto-detect tests actually want.
    fn fake_which(binaries: Vec<&'static str>) -> impl Fn(&str) -> Option<PathBuf> {
        move |query| {
            if binaries.contains(&query) {
                match query {
                    "git.exe" | "git" => Some(PathBuf::from(r"C:\Program Files\Git\cmd\git.exe")),
                    _ => Some(PathBuf::from(format!(r"C:\fake\{}", query))),
                }
            } else {
                None
            }
        }
    }

    #[test]
    fn pty_wrapper_args_are_owned_and_shell_specific() {
        let wrapper = PathBuf::from(r"C:\tmp\task.bat");
        assert_eq!(
            WindowsShell::Cmd.pty_wrapper_args(&wrapper),
            vec![
                "/D".to_string(),
                "/c".to_string(),
                wrapper.display().to_string()
            ]
        );
        assert_eq!(
            WindowsShell::Pwsh.pty_wrapper_args(&wrapper),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-File".to_string(),
                wrapper.display().to_string(),
            ]
        );
        assert_eq!(
            WindowsShell::Posix(PathBuf::from(r"C:\Git\bin\bash.exe")).pty_wrapper_args(&wrapper),
            vec![wrapper.display().to_string()]
        );
    }

    #[test]
    fn pty_cmd_wrapper_disables_autorun_before_the_wrapper() {
        let wrapper = PathBuf::from(r"C:\tmp\task.bat");
        assert_eq!(
            WindowsShell::Cmd.pty_wrapper_args(&wrapper),
            vec![
                "/D".to_string(),
                "/c".to_string(),
                wrapper.display().to_string()
            ]
        );
    }

    // ---------------------------------------------------------------
    // Fix for user report: $SHELL must be respected on Windows so
    // git-bash (and other POSIX shells) can run agent-emitted bash
    // syntax instead of getting routed to PowerShell where escaping
    // breaks. Mirrors OpenCode's behavior.
    // ---------------------------------------------------------------

    #[test]
    fn user_shell_pointing_at_bash_wins_over_powershell() {
        // SHELL=C:\Program Files\Git\bin\bash.exe
        // pwsh.exe also reachable.
        // Expect: Posix(bash.exe) is the first candidate, pwsh second.
        let tmp = tempfile::tempdir().expect("tempdir");
        let bash = tmp.path().join("bash.exe");
        std::fs::write(&bash, b"shebang").unwrap();

        let candidates = shell_candidates_with(fake_which(vec!["pwsh.exe"]), || Some(bash.clone()));

        assert!(matches!(candidates[0], WindowsShell::Posix(_)));
        if let WindowsShell::Posix(p) = &candidates[0] {
            assert_eq!(p, &bash);
        }
        assert_eq!(candidates[1], WindowsShell::Pwsh);
    }

    #[test]
    fn user_shell_pointing_at_non_posix_binary_is_ignored() {
        // SHELL=C:\Windows\System32\cmd.exe — not in POSIX_NAMES, so
        // we should fall back to PowerShell/cmd resolution.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmd = tmp.path().join("cmd.exe");
        std::fs::write(&cmd, b"").unwrap();

        let candidates = shell_candidates_with(fake_which(vec!["pwsh.exe"]), || Some(cmd));

        // No Posix candidate; pwsh wins.
        assert!(!candidates
            .iter()
            .any(|c| matches!(c, WindowsShell::Posix(_))));
        assert_eq!(candidates[0], WindowsShell::Pwsh);
    }

    #[test]
    fn user_shell_msys_drive_letter_path_is_normalized() {
        // SHELL=/c/Program Files/Git/bin/bash.exe — git-bash style.
        // Without normalization this won't exist at all, so the
        // resolver should at least *try* the normalized form before
        // falling through.
        //
        // We can't easily fake an existing file at C:\... in a unit
        // test, so we directly assert the normalization output here.
        let raw = PathBuf::from("/c/Program Files/Git/bin/bash.exe");
        let normalized = normalize_shell_path(&raw);
        assert_eq!(
            normalized,
            PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")
        );
    }

    #[test]
    fn user_shell_already_windows_path_passes_through() {
        let raw = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let normalized = normalize_shell_path(&raw);
        assert_eq!(normalized, raw);
    }

    /// Note: this test runs on every platform but uses platform-native
    /// path separators because `Path::file_stem()` only recognizes the
    /// host OS's separator. On macOS/Linux that means a forward-slash
    /// fake path (`/fake/bash`); on Windows the equivalent backslash
    /// path. The production code only runs on Windows where backslash
    /// works correctly, so the test's job is to verify the resolution
    /// flow, not the path syntax.
    #[test]
    fn user_shell_bare_name_resolves_via_which() {
        // SHELL=bash → not absolute → which("bash") returns the fake
        // resolver's path → recognized as POSIX.
        #[cfg(unix)]
        let expected = PathBuf::from("/fake/bash");
        #[cfg(windows)]
        let expected = PathBuf::from(r"C:\fake\bash");

        // Pre-translate the fake_which return so it uses the host's
        // separator. We can't share fake_which here because that helper
        // is hard-coded to Windows-style paths.
        let expected_clone = expected.clone();
        let which_for = move |query: &str| -> Option<PathBuf> {
            if query == "bash" {
                Some(expected_clone.clone())
            } else {
                None
            }
        };

        let candidates = shell_candidates_with(which_for, || Some(PathBuf::from("bash")));
        assert!(
            matches!(&candidates[0], WindowsShell::Posix(p) if p == &expected),
            "expected Posix({}) as first candidate, got {:?}",
            expected.display(),
            candidates
        );
    }

    #[test]
    fn no_user_shell_and_no_git_falls_back_to_pwsh_powershell_cmd() {
        let candidates =
            shell_candidates_with(fake_which(vec!["pwsh.exe", "powershell.exe"]), || None);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0], WindowsShell::Pwsh);
        assert_eq!(candidates[1], WindowsShell::Powershell);
        assert_eq!(candidates[2], WindowsShell::Cmd);
    }

    #[test]
    fn cmd_is_always_the_floor() {
        // Nothing reachable, no $SHELL — only cmd.exe should be in the list.
        let candidates = shell_candidates_with(|_| None, || None);
        assert_eq!(candidates, vec![WindowsShell::Cmd]);
    }

    // ---------------------------------------------------------------
    // git-bash auto-detect: when $SHELL is unset but the user installed
    // Git for Windows, we should still pick up the bundled bash.exe.
    // ---------------------------------------------------------------

    #[test]
    fn git_bash_auto_detect_when_shell_unset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Mirror the Git for Windows layout: <root>/cmd/git.exe and
        // <root>/bin/bash.exe.
        std::fs::create_dir_all(tmp.path().join("cmd")).unwrap();
        std::fs::create_dir_all(tmp.path().join("bin")).unwrap();
        let git = tmp.path().join("cmd").join("git.exe");
        std::fs::write(&git, b"git").unwrap();
        let bash = tmp.path().join("bin").join("bash.exe");
        std::fs::write(&bash, b"shebang").unwrap();

        let which = |query: &str| -> Option<PathBuf> {
            match query {
                "git.exe" | "git" => Some(git.clone()),
                _ => None,
            }
        };
        let candidates = shell_candidates_with(which, || None);

        // First candidate is the auto-detected git-bash.
        assert!(matches!(&candidates[0], WindowsShell::Posix(p) if p == &bash));
        // cmd.exe is still the floor.
        assert_eq!(*candidates.last().unwrap(), WindowsShell::Cmd);
    }

    #[test]
    fn git_bash_skipped_when_user_shell_already_posix() {
        // $SHELL points at git-bash → no need to auto-detect a second
        // POSIX candidate. The candidate list should not contain two
        // Posix entries.
        let tmp = tempfile::tempdir().expect("tempdir");
        let bash = tmp.path().join("bash.exe");
        std::fs::write(&bash, b"shebang").unwrap();

        let candidates = shell_candidates_with(
            // git is reachable, but git-bash should NOT be added because
            // we already have a Posix from $SHELL.
            |query: &str| match query {
                "git.exe" | "git" => Some(PathBuf::from(r"C:\Program Files\Git\cmd\git.exe")),
                _ => None,
            },
            || Some(bash.clone()),
        );

        let posix_count = candidates
            .iter()
            .filter(|c| matches!(c, WindowsShell::Posix(_)))
            .count();
        assert_eq!(
            posix_count, 1,
            "exactly one Posix candidate when $SHELL is already set: got {:?}",
            candidates
        );
    }

    // ---------------------------------------------------------------
    // Spawn-shape tests: Posix(bash) must be invoked as `bash -c <cmd>`
    // exactly the way Unix bash works.
    // ---------------------------------------------------------------

    #[test]
    fn posix_shell_uses_dash_c_invocation() {
        let bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let shell = WindowsShell::Posix(bash);
        let args = shell.args("ls -la /tmp");
        assert_eq!(args, vec!["-c", "ls -la /tmp"]);
    }

    #[test]
    fn posix_shell_binary_returns_full_path() {
        let bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let shell = WindowsShell::Posix(bash.clone());
        assert_eq!(shell.binary().as_ref(), &bash.display().to_string());
    }

    #[test]
    fn pwsh_args_unchanged() {
        // Regression guard: refactor must not have altered PowerShell
        // arg shape.
        let shell = WindowsShell::Pwsh;
        let args = shell.args("Get-ChildItem");
        assert_eq!(
            args,
            vec![
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "Get-ChildItem"
            ]
        );
    }

    #[test]
    fn cmd_args_unchanged() {
        let shell = WindowsShell::Cmd;
        let args = shell.args("dir");
        assert_eq!(args, vec!["/D", "/C", "dir"]);
    }

    // ---------------------------------------------------------------
    // POSIX wrapper script: bg-bash exit-marker contract for git-bash.
    // ---------------------------------------------------------------

    #[test]
    fn posix_wrapper_writes_exit_marker_atomically() {
        let bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let shell = WindowsShell::Posix(bash);
        let script = shell.wrapper_script("echo hi", Path::new(r"C:\Temp\bash.exit"));
        // The child runs the resolved shell directly (bash semantics) and then
        // records its own exit code into the io/ marker via temp-file + rename,
        // so detached tasks whose spawning daemon is gone still report exit.
        assert!(
            script.contains(r"'C:\Program Files\Git\bin\bash.exe' -c 'echo hi'"),
            "wrapper must invoke the resolved shell directly: {script}",
        );
        assert!(
            script.contains(r#"printf '%s' "$?" >"#),
            "wrapper must capture $? for the exit marker: {script}"
        );
        assert!(
            script.contains("bash.exit.tmp") && script.contains("mv "),
            "wrapper must write the marker atomically via temp-file + rename: {script}"
        );
    }

    #[test]
    fn posix_wrapper_escapes_embedded_single_quotes() {
        // User command contains a single quote — wrapper must use the
        // standard `'\''` close-and-reopen idiom.
        let bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let shell = WindowsShell::Posix(bash);
        let script = shell.wrapper_script("echo 'hi'", Path::new(r"C:\Temp\bash.exit"));
        assert!(
            script.contains(r"'echo '\''hi'\'''"),
            "embedded single quote must be escaped: got {script}"
        );
    }

    // ---------------------------------------------------------------
    // is_posix_shell_name: case-insensitive, .exe-tolerant lookup.
    // ---------------------------------------------------------------

    #[test]
    fn is_posix_shell_name_recognizes_known_shells() {
        for name in ["bash", "BASH", "bash.exe", "Bash.Exe", "sh", "zsh.exe"] {
            assert!(
                is_posix_shell_name(Path::new(name)),
                "{name} should be POSIX"
            );
        }
    }

    #[test]
    fn is_posix_shell_name_rejects_non_posix() {
        for name in ["cmd.exe", "powershell.exe", "pwsh.exe", "fish", "nu.exe"] {
            assert!(
                !is_posix_shell_name(Path::new(name)),
                "{name} must NOT be POSIX"
            );
        }
    }
}
