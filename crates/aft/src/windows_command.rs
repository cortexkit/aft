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
#[cfg(windows)]
const BATCH_ARGUMENT_ENV_PREFIX: &str = "AFT_BATCH_ARGUMENT_";

/// Whether `path` must be dispatched by `cmd.exe` on Windows.
#[cfg(windows)]
pub(crate) fn is_batch_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

/// Whether `key` is reserved for the batch-shim launcher.
///
/// Callers that add their own child environment must not override these values,
/// because `cmd.exe` expands the command tail using them.
#[cfg(windows)]
pub(crate) fn is_batch_internal_env(key: &str, argument_count: usize) -> bool {
    if key.eq_ignore_ascii_case(BATCH_COMMAND_ENV) {
        return true;
    }

    let Some(suffix) = key.get(BATCH_ARGUMENT_ENV_PREFIX.len()..) else {
        return false;
    };
    if !key[..BATCH_ARGUMENT_ENV_PREFIX.len()].eq_ignore_ascii_case(BATCH_ARGUMENT_ENV_PREFIX) {
        return false;
    }

    suffix
        .parse::<usize>()
        .ok()
        .is_some_and(|index| index < argument_count && suffix == index.to_string())
}

/// Build a safe `cmd.exe` invocation for a `.cmd`/`.bat` shim.
///
/// Windows cannot spawn batch files directly. Passing a batch path and its arguments
/// as separate `cmd /C` arguments is unreliable because `cmd.exe` reparses its
/// command tail. Keep the executable path and arguments in the environment (so
/// literal `%` remains literal after single-pass expansion), then pass one
/// explicitly quoted command tail.
#[cfg(windows)]
pub(crate) fn batch_command<I, S>(binary: &Path, args: I) -> io::Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    use std::os::windows::process::CommandExt;

    let command_path = binary.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch path cannot be represented safely for cmd.exe",
        )
    })?;
    if command_path.contains(['\0', '\r', '\n', '"']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "batch path cannot be represented safely for cmd.exe",
        ));
    }
    // `fs::canonicalize` produces extended-length (`\\?\`) paths on Windows.
    // CreateProcess accepts those paths, but cmd.exe does not reliably execute a
    // batch file through that namespace, and npm shims additionally derive
    // `%~dp0` paths that fail with "The system cannot find the path specified."
    // Convert only the namespace spelling; the path remains canonical.
    let command_path = cmd_compatible_path(command_path);

    let mut command_line = format!("\"\"%{BATCH_COMMAND_ENV}%\"");
    let mut argument_env = Vec::new();
    for (index, arg) in args.into_iter().enumerate() {
        let arg = arg.as_ref().to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch argument cannot be represented safely for cmd.exe",
            )
        })?;
        if arg.contains(['\0', '\r', '\n', '"']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch argument cannot be represented safely for cmd.exe",
            ));
        }
        let name = format!("{BATCH_ARGUMENT_ENV_PREFIX}{index}");
        command_line.push_str(" \"");
        command_line.push('%');
        command_line.push_str(&name);
        command_line.push_str("%\"");
        argument_env.push((name, arg.to_owned()));
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
        .env(BATCH_COMMAND_ENV, command_path)
        .envs(argument_env);
    Ok(command)
}

#[cfg(windows)]
fn cmd_compatible_path(path: &str) -> String {
    for prefix in [r"\\?\UNC\", r"\\??\UNC\", r"\??\UNC\"] {
        if let Some(tail) = strip_ascii_prefix(path, prefix) {
            let mut components = tail
                .split(['\\', '/'])
                .filter(|component| !component.is_empty());
            if components.next().is_some() && components.next().is_some() {
                return format!(r"\\{tail}");
            }
            return path.to_string();
        }
    }

    for prefix in [r"\\?\", r"\\??\", r"\??\"] {
        if let Some(tail) = strip_ascii_prefix(path, prefix) {
            let bytes = tail.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/')
            {
                return tail.to_string();
            }
            // Namespaces such as `\\?\Volume{GUID}\` cannot be safely
            // converted into a DOS path by dropping their prefix.
            return path.to_string();
        }
    }

    path.to_string()
}

#[cfg(windows)]
fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        value.get(prefix.len()..)
    } else {
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn cmd_compatible_path_only_converts_dos_and_unc_namespaces() {
        assert_eq!(
            cmd_compatible_path(r"\\?\C:\cache\server.cmd"),
            r"C:\cache\server.cmd"
        );
        assert_eq!(
            cmd_compatible_path(r"\\?\unc\host\share\server.cmd"),
            r"\\host\share\server.cmd"
        );
        assert_eq!(
            cmd_compatible_path(r"\\?\Volume{1234}\server.cmd"),
            r"\\?\Volume{1234}\server.cmd"
        );
    }

    #[test]
    fn batch_command_invokes_a_spaced_shim_with_args() {
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("language server.cmd");
        std::fs::write(&shim, "@echo off\r\necho %~1\r\n").unwrap();

        let output = batch_command(&shim, ["--stdio"]).unwrap().output().unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "--stdio");
    }

    #[test]
    fn batch_command_invokes_canonicalized_npm_style_shim() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("npm cache 100%");
        let bin = root.join("node_modules").join(".bin");
        let package = root.join("node_modules").join("language-server");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        let shim = bin.join("language-server.cmd");
        let target = package.join("server.cmd");
        std::fs::write(
            &shim,
            "@echo off\r\nset dp0=%~dp0\r\n\"%dp0%\\..\\language-server\\server.cmd\" %*\r\n",
        )
        .unwrap();
        std::fs::write(&target, "@echo off\r\necho %~1\r\n").unwrap();
        let canonical_shim = std::fs::canonicalize(&shim).unwrap();
        assert!(canonical_shim.to_string_lossy().starts_with(r"\\?\"));

        let output = batch_command(&canonical_shim, ["--stdio"])
            .unwrap()
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "--stdio");
    }

    #[test]
    fn batch_command_preserves_percent_in_argument() {
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("formatter.cmd");
        std::fs::write(&shim, "@echo off\r\necho %~1\r\n").unwrap();

        let output = batch_command(&shim, ["100%coverage%"])
            .unwrap()
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "100%coverage%"
        );
    }

    #[test]
    fn launcher_environment_names_are_reserved() {
        assert!(is_batch_internal_env("AFT_BATCH_COMMAND", 1));
        assert!(is_batch_internal_env("aft_batch_argument_0", 1));
        assert!(!is_batch_internal_env("AFT_BATCH_ARGUMENT_1", 1));
        assert!(!is_batch_internal_env("AFT_BATCH_ARGUMENT_00", 1));
        assert!(!is_batch_internal_env("AFT_BATCH_OTHER", 1));
    }

    #[test]
    fn batch_command_rejects_non_unicode_arguments() {
        use std::os::windows::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("formatter.cmd");
        let invalid = std::ffi::OsString::from_wide(&[0xd800]);

        let error = batch_command(&shim, [invalid]).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
