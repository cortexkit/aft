//! Safe Windows process invocation for `.cmd` and `.bat` wrapper scripts.

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::io;
#[cfg(windows)]
use std::path::Path;
#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
const BATCH_COMMAND_ENV: &str = "AFT_BATCH_COMMAND";

/// Whether `path` must be dispatched by `cmd.exe` on Windows.
#[cfg(windows)]
pub(crate) fn is_batch_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

/// Build a safe `cmd.exe` invocation for a `.cmd`/`.bat` shim.
///
/// Windows cannot spawn batch files directly. Passing a batch path and its arguments
/// as separate `cmd /C` arguments is unreliable because `cmd.exe` reparses its
/// command tail. Keep the executable path in the environment (so literal `%` in a
/// path is not expanded), then pass one explicitly quoted command tail.
#[cfg(windows)]
pub(crate) fn batch_command<I, S>(binary: &Path, args: I) -> io::Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::os::windows::process::CommandExt;

    let command_path = binary.to_string_lossy();
    if command_path.contains(['\0', '\r', '\n', '"']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch path cannot be represented safely for cmd.exe",
        ));
    }

    let mut command_line = format!("\"\"%{BATCH_COMMAND_ENV}%\"");
    for arg in args {
        let arg = arg.as_ref().to_string_lossy();
        if arg.contains(['\0', '\r', '\n', '"', '%']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch argument cannot be represented safely for cmd.exe",
            ));
        }
        command_line.push_str(" \"");
        command_line.push_str(&arg);
        command_line.push('"');
    }
    command_line.push('"');

    let mut command = Command::new(
        std::env::var_os("ComSpec")
            .or_else(|| std::env::var_os("COMSPEC"))
            .unwrap_or_else(|| "cmd.exe".into()),
    );
    command
        .args(["/d", "/s", "/v:off", "/c"])
        // `cmd.exe` must receive the complete command tail verbatim. Normal
        // argument escaping would add another quoting layer and break paths
        // containing spaces.
        .raw_arg(command_line)
        .env(BATCH_COMMAND_ENV, binary);
    Ok(command)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn batch_command_invokes_a_spaced_shim_with_args() {
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("language server.cmd");
        std::fs::write(&shim, "@echo off\r\necho %~1\r\n").unwrap();

        let output = batch_command(&shim, ["--stdio"]).unwrap().output().unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "--stdio");
    }
}
