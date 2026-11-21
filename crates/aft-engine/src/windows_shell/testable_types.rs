use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsShell {
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

#[derive(Debug)]
pub enum CandidateFailure<E> {
    Stopped { index: usize, error: E },
    Exhausted { last: Option<(usize, E)> },
}

#[derive(Debug)]
pub enum PipedAttemptError {
    Setup(String),
    Spawn(std::io::Error),
}
