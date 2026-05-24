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

    pub(crate) fn command(&self, command: &str) -> Command {
        let mut cmd = Command::new(self.binary().as_ref());
        cmd.args(self.args(command));
        cmd
    }

    /// Build a `Command` that runs the background wrapper script.
    ///
    /// Production background bash now writes cmd wrappers to `.bat` files and
    /// invokes them without delayed expansion, so paths containing `!` remain
    /// literal. This helper is retained for tests around legacy inline shapes.
    ///
    /// For foreground bash, callers should use [`Self::command`] instead;
    /// `/V:ON` would change the semantics of user commands containing `!`
    /// (which would otherwise be passed through literally to the user).
    // No longer called by production bg-bash (which writes the wrapper
    // to a temp file and invokes via `-File` / `cmd /C path`), but kept
    // for tests that exercise the shell-arg shape directly.
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

    /// Wrap a background command so shell termination writes an exit marker.
    /// The marker is written via temp-file + rename for PowerShell variants and
    /// via `move /Y` for cmd.exe, matching the Unix background wrapper contract.
    pub(crate) fn wrapper_script(&self, command: &str, exit_path: &Path) -> String {
        match self {
            WindowsShell::Pwsh | WindowsShell::Powershell => {
                let exit_path = powershell_single_quote(&exit_path.display().to_string());
                let command = powershell_single_quote(command);
                // The wrapper itself runs as a PowerShell script (invoked
                // via `pwsh -File <path>` by `detached_shell_command_for`),
                // so we execute the user command directly with `Invoke-Expression`
                // instead of nesting another shell. Earlier versions wrapped
                // the user command in an inner `& 'pwsh.exe' -Command ...`
                // which caused PS-on-PS recursion that silently produced
                // empty output on Windows 11 (likely a console-host issue
                // with nested non-interactive pwsh sessions).
                //
                // CRITICAL: this script must contain NO literal double-quote
                // characters. Inner `"` would break the outer-shell parse on
                // some Windows configurations even with `-File`. We use only
                // single-quoted strings and string concat (`+`) for any
                // interpolation needs.
                format!(
                    concat!(
                        "$exitPath = {exit_path}; ",
                        "$tmpPath = $exitPath + '.tmp.' + $PID; ",
                        "$global:LASTEXITCODE = $null; ",
                        "Invoke-Expression {command}; ",
                        "$success = $?; ",
                        "$nativeCode = $global:LASTEXITCODE; ",
                        "if ($null -ne $nativeCode) {{ $code = [int]$nativeCode }} ",
                        "elseif ($success) {{ $code = 0 }} ",
                        "else {{ $code = 1 }}; ",
                        "[System.IO.File]::WriteAllText($tmpPath, [string]$code); ",
                        "Move-Item -LiteralPath $tmpPath -Destination $exitPath -Force; ",
                        "exit $code"
                    ),
                    exit_path = exit_path,
                    command = command
                )
            }
            WindowsShell::Cmd => {
                // This body is written to a `.bat` file and invoked as
                // `cmd /D /C <wrapper.bat>`. Batch files expand `%ERRORLEVEL%`
                // per line, so we do not need `/V:ON` delayed expansion; paths
                // containing literal `!` survive unchanged.
                let tmp_path = format!("{}.tmp", exit_path.display());
                format!(
                    concat!(
                        "@echo off\r\n",
                        "{command}\r\n",
                        "set CODE=%ERRORLEVEL%\r\n",
                        "echo %CODE% > {tmp}\r\n",
                        "move /Y {tmp} {exit} > nul\r\n",
                        "exit /B %CODE%\r\n"
                    ),
                    command = command,
                    tmp = cmd_quote(&tmp_path),
                    exit = cmd_quote(&exit_path.display().to_string())
                )
            }
            WindowsShell::Posix(shell_path) => {
                // git-bash and friends speak POSIX, so the same temp-file +
                // mv pattern the Unix bg-bash wrapper uses applies here. The
                // wrapper writes the user command's $? to a temp file and
                // atomically renames it into place so partial writes are
                // never observable. Single-quote the user command to defang
                // any embedded `;`, `&`, or `$` — POSIX single-quotes don't
                // interpret anything except `'` itself, which we escape via
                // the `'\''` close-and-reopen idiom.
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
}

/// Resolve which Windows shell to use for `bash` invocations.
///
/// Cached after the first resolve to avoid repeated PATH probes — the user's
/// installed shells don't change mid-session, so a static cache is safe and
/// keeps bash dispatch fast.
///
/// **Note:** PATH probe via `which::which` can disagree with what
/// `Command::spawn` actually sees at runtime — antivirus / AppLocker rules,
/// PATH inheritance gaps in the spawning host, or sandbox flags can make
/// a binary "exist" to `which` but fail to spawn with NotFound. Foreground
/// bash uses [`shell_candidates`] + runtime retry to defend against this;
/// callers that take this single-result API are accepting the cached
/// outcome at face value.
// No longer called by production bg-bash (the new path uses
// `shell_candidates()` with retry directly). Kept for potential future
// use and for parity with the foreground spawn loop.
#[allow(dead_code)]
pub(crate) fn resolve_windows_shell() -> WindowsShell {
    shell_candidates()
        .first()
        .cloned()
        .unwrap_or(WindowsShell::Cmd)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // The F2 wrapper invokes the resolved shell path directly (not a bare
        // `sh -c`), so users get bash semantics (`[[ ]]`, arrays, pipefail)
        // rather than dash. It then captures `$?` via `printf` into a tmp file
        // and `mv`s atomically into place.
        assert!(
            script.contains(r"'C:\Program Files\Git\bin\bash.exe' -c 'echo hi'"),
            "wrapper must invoke the resolved shell directly: {script}",
        );
        assert!(script.contains("printf '%s' \"$?\""), "{script}");
        assert!(script.contains("mv "), "{script}");
        assert!(script.contains(r"C:\Temp\bash.exit"), "{script}");
        assert!(script.contains(r"C:\Temp\bash.exit.tmp"), "{script}");
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
