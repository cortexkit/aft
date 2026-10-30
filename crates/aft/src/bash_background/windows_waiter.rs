//! Cheap Windows parent that records PowerShell's real process exit code.

#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use super::persistence::TaskPaths;

pub(crate) struct Launch {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub environment: (String, String),
}

pub(crate) fn available() -> bool {
    cfg!(windows) && which::which("cmd.exe").is_ok()
}

fn quote(value: &str) -> String {
    format!("\"{value}\"")
}

fn script(key: &str, absent_defaults: &[&str], pty: bool, memory_gated: bool) -> String {
    // This file is ASCII even when every path contains Unicode. Paths arrive
    // as batch arguments through a late-expanded environment value, avoiding
    // percent expansion and cmd's batch-file text encoding altogether.
    let clear = absent_defaults
        .iter()
        .map(|name| format!("set \"{name}=\"\r\n"))
        .collect::<String>();
    // A piped cmd has no console. START /B prevents its PowerShell child from
    // allocating a new console host; ConPTY children instead inherit the PTY.
    let launch = if pty { "" } else { "start \"\" /B /WAIT " };
    // This barrier runs in cmd itself: a PowerShell child created before Job
    // Object assignment would not retroactively join its parent's job.
    let gate = if memory_gated {
        format!("set /p {key}=\r\nif not \"%{key}%\"==\"aft-memory-ready\" exit /B 125\r\nset \"{key}=\"\r\n")
    } else {
        String::new()
    };
    format!(
        concat!(
            "@echo off\r\n",
            "setlocal EnableExtensions DisableDelayedExpansion\r\n",
            "set \"{key}=\"\r\n",
            "{clear}",
            "{gate}",
            "{launch}\"%~1\" %~2 %~3 \"%~4\"\r\n",
            // A user-supplied ERRORLEVEL variable hides cmd's dynamic value. Delete
            // it only after PowerShell exits; successful SET preserves the real code.
            "set \"ERRORLEVEL=\"\r\n",
            "set \"CODE=%ERRORLEVEL%\"\r\n",
            "> \"%~5\" echo %CODE%\r\n",
            // cmd's MOVE cannot replace the daemon's open empty placeholder.
            // Move only that empty file aside, then publish the complete result.
            "for %%F in (\"%~6\") do if %%~zF==0 move /Y \"%~6\" \"%~6.waiter.empty\" >nul\r\n",
            "move /Y \"%~5\" \"%~6\" >nul\r\n",
            "del /F /Q \"%~6.waiter.empty\" >nul 2>nul\r\n",
            "exit /B %CODE%\r\n",
        ),
        key = key,
        clear = clear,
        launch = launch,
        gate = gate
    )
}

/// Wrap only the generated shell invocation, never user command text. The
/// temporary environment key is removed before PowerShell starts.
pub(crate) fn wrap(
    program: &OsStr,
    args: &[OsString],
    paths: &TaskPaths,
    workdir: &Path,
    environment: &HashMap<String, String>,
    pty: bool,
    memory_gated: bool,
) -> std::io::Result<Option<Launch>> {
    let Ok(cmd) = which::which("cmd.exe") else {
        return Ok(None);
    };
    // cmd searches its working folder before PATH. Resolve the selected shell
    // here so a project-local executable cannot replace the daemon's shell.
    let Ok(program) = which::which(program) else {
        return Ok(None);
    };
    // cmd cannot reliably keep UNC or legacy-limit working folders. Keep the
    // direct PowerShell path rather than silently changing a command's folder.
    let cwd = workdir.to_string_lossy();
    if cwd.starts_with("\\\\") || cwd.starts_with("//") || cwd.encode_utf16().count() >= 248 {
        return Ok(None);
    }
    let Some(split) = args.len().checked_sub(2) else {
        return Ok(None);
    };
    let mode = args[split].to_string_lossy();
    if !matches!(mode.as_ref(), "-Command" | "-File") {
        return Ok(None);
    }
    let flags = args[..split]
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>();
    if flags
        .iter()
        .any(|s| !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    {
        return Ok(None);
    }
    let mut key = format!(
        "__AFT_WAITER_{}",
        paths.task_id.replace('-', "_").to_ascii_uppercase()
    );
    while std::env::var_os(&key).is_some()
        || environment.keys().any(|k| k.eq_ignore_ascii_case(&key))
    {
        key.push('_');
    }
    let waiter = paths.dir.join("powershell-waiter.bat");
    // cmd supplies defaults of its own. Remove only variables absent from the
    // original environment, so PowerShell sees no helper additions.
    let mut absent_defaults = Vec::new();
    for name in ["PROMPT", "COMSPEC", "PATHEXT"] {
        let original = environment
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| OsString::from(value))
            .or_else(|| std::env::var_os(name));
        match original {
            Some(value) if value.is_empty() => return Ok(None),
            None => absent_defaults.push(name),
            _ => {}
        }
    }
    crate::private_storage::write(&waiter, script(&key, &absent_defaults, pty, memory_gated))?;
    let temporary = paths.exit.with_extension("waiter.tmp");
    let parts = [
        waiter.display().to_string(),
        program.to_string_lossy().into_owned(),
        flags.join(" "),
        mode.into_owned(),
        args[split + 1].to_string_lossy().into_owned(),
        temporary.display().to_string(),
        paths.exit.display().to_string(),
    ];
    if parts.iter().any(|part| part.contains(['\"', '\r', '\n'])) {
        return Ok(None);
    }
    let line = parts.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ");
    if line.encode_utf16().count() + key.len() >= 32_760 {
        return Ok(None);
    }
    Ok(Some(Launch {
        program: cmd.into_os_string(),
        args: ["/D", "/E:ON", "/V:ON", "/C"]
            .into_iter()
            .map(OsString::from)
            .chain(std::iter::once(format!("!{key}!").into()))
            .collect(),
        environment: (key, line),
    }))
}

#[cfg(test)]
pub(crate) fn test_task_layout(
    root: &Path,
    session: &str,
    command: &str,
) -> super::persistence::ResolvedTask {
    use super::persistence::{allocate_task_layout, write_task, PersistedTask};
    let layout = allocate_task_layout(root, session).unwrap();
    let metadata = PersistedTask::starting(
        layout.paths.task_id.clone(),
        session.into(),
        command.into(),
        root.into(),
        Some(root.into()),
        Some(30_000),
        true,
        false,
    );
    write_task(&layout.paths.json, &metadata).unwrap();
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiter_script_is_ascii_and_clears_transport_before_shell() {
        let text = script("__AFT_WAITER_TEST", &[], false, false);
        assert!(text.is_ascii());
        assert!(text.find("set \"__AFT_WAITER_TEST=\"").unwrap() < text.find("\"%~1\"").unwrap());
        assert!(text.find("set \"ERRORLEVEL=\"").unwrap() > text.find("\"%~1\"").unwrap());
        assert!(text.contains("move /Y \"%~5\" \"%~6\" >nul"));
    }

    #[test]
    fn memory_gate_precedes_powershell_creation() {
        let text = script("__AFT_WAITER_TEST", &[], false, true);
        assert!(
            text.find("set /p __AFT_WAITER_TEST=").unwrap()
                < text.find("start \"\" /B /WAIT").unwrap()
        );
        assert!(text.contains("if not \"%__AFT_WAITER_TEST%\"==\"aft-memory-ready\" exit /B 125"));
    }
}
