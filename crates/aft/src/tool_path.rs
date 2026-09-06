//! Cross-platform tool binary resolution on PATH and well-known install dirs.
//!
//! PATH walking follows the same contract as cortexkit/magic-context
//! `packages/cli/src/lib/find-on-path.ts` (PR #75): probe filesystem entries
//! without shelling out to `which`/`where`, and on Windows try
//! `.exe` → `.cmd` → `.bat` → `.com` per PATH directory.

use std::path::{Path, PathBuf};

/// Resolve `binary` on the process `PATH` (PATHEXT-aware on Windows via `which`).
pub(crate) fn resolve_on_path(binary: &str) -> Option<PathBuf> {
    if let Ok(path) = which::which(binary) {
        return Some(path);
    }
    find_on_path_manual(binary)
}

/// Walk `PATH` left-to-right without spawning a subprocess.
pub(crate) fn find_on_path_manual(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if let Some(found) = probe_tool_in_dir(&dir, binary) {
            return Some(found);
        }
    }
    None
}

fn path_looks_like_tool(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

/// Check `dir/<binary>` and, on Windows, `dir/<binary>.exe|.cmd|.bat|.com`.
pub(crate) fn probe_tool_in_dir(dir: &Path, binary: &str) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }

    let direct = dir.join(binary);
    if path_looks_like_tool(&direct) {
        return Some(direct);
    }

    if cfg!(windows) {
        for ext in ["exe", "cmd", "bat", "com"] {
            let candidate = dir.join(format!("{binary}.{ext}"));
            if path_looks_like_tool(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

/// Extra bin directories GUI-launched hosts often omit from `PATH`.
#[cfg(windows)]
pub(crate) fn well_known_windows_bin_dirs(userprofile: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::with_capacity(10);
    dirs.push(PathBuf::from(r"C:\Go\bin"));
    dirs.push(PathBuf::from(r"C:\Program Files\Go\bin"));
    dirs.push(PathBuf::from(r"C:\Program Files\nodejs"));
    if let Some(appdata) = crate::environment::non_empty_os_var("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    if let Some(local) = crate::environment::non_empty_os_var("LOCALAPPDATA") {
        let local_path = PathBuf::from(local);
        dirs.push(local_path.join("pnpm"));
        dirs.push(local_path.join("Programs").join("Python"));
    }
    if let Some(up) = userprofile {
        let up_path = PathBuf::from(up);
        dirs.push(up_path.join(r".cargo\bin"));
        dirs.push(up_path.join(r"go\bin"));
        dirs.push(up_path.join("scoop").join("shims"));
    }
    dirs
}

#[cfg(not(windows))]
pub(crate) fn well_known_windows_bin_dirs(_userprofile: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn find_on_path_manual_returns_null_when_path_unset() {
        let _guard = crate::test_env::process_env_lock();
        let saved = std::env::var_os("PATH");
        std::env::remove_var("PATH");
        assert!(find_on_path_manual("aft-nonexistent-tool-xyzzy").is_none());
        if let Some(path) = saved {
            std::env::set_var("PATH", path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_on_path_manual_finds_executable_in_single_dir() {
        let _guard = crate::test_env::process_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let bin_path = dir.path().join("opencode-test-bin");
        fs::write(&bin_path, "#!/bin/sh\necho ok\n").unwrap();
        let mut perms = fs::metadata(&bin_path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        fs::set_permissions(&bin_path, perms).unwrap();

        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.path());
        let found = find_on_path_manual("opencode-test-bin");
        if let Some(path) = saved {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }

        assert_eq!(found.as_deref(), Some(bin_path.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn find_on_path_manual_skips_non_executable_file() {
        let _guard = crate::test_env::process_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let bin_path = dir.path().join("opencode-test-bin");
        fs::write(&bin_path, "not executable\n").unwrap();

        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.path());
        let found = find_on_path_manual("opencode-test-bin");
        if let Some(path) = saved {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }

        assert!(found.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn probe_tool_in_dir_finds_cmd_shim() {
        let dir = tempfile::tempdir().unwrap();
        let cmd_path = dir.path().join("biome.cmd");
        fs::write(&cmd_path, "@echo off\n").unwrap();
        assert_eq!(
            probe_tool_in_dir(dir.path(), "biome").as_deref(),
            Some(cmd_path.as_path())
        );
    }
}
