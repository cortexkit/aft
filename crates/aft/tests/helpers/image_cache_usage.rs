//! Process and filesystem probes for conservative fixture-image eviction.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(unix)]
type FileKey = (u64, u64);
#[cfg(windows)]
type FileKey = PathBuf;

#[derive(Default)]
pub struct Usage {
    pub live_pids: Option<HashSet<u32>>,
    files: HashSet<FileKey>,
}

impl Usage {
    pub fn pid_alive(&self, pid: u32) -> bool {
        self.live_pids
            .as_ref()
            .map_or_else(|| pid_alive(pid), |pids| pids.contains(&pid))
    }

    pub fn in_use(&self, directory: &Path) -> io::Result<bool> {
        for child in fs::read_dir(directory)? {
            let child = child?;
            let kind = child.file_type()?;
            if (kind.is_file() && self.files.contains(&file_key(&child.path())?))
                || (kind.is_dir() && self.in_use(&child.path())?)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn record(&mut self, path: &Path) {
        if let Ok(key) = file_key(path) {
            self.files.insert(key);
        }
    }
}

#[cfg(unix)]
fn file_key(path: &Path) -> io::Result<FileKey> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_key(path: &Path) -> io::Result<FileKey> {
    Ok(PathBuf::from(
        path.canonicalize()?.to_string_lossy().to_lowercase(),
    ))
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal zero only checks existence and permission; it sends no signal.
    unsafe {
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: the query-only handle is closed here and never used to alter a process.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            GetLastError() != ERROR_INVALID_PARAMETER
        } else {
            CloseHandle(handle);
            true
        }
    }
}

#[cfg(target_os = "linux")]
fn scan_process(
    mut scan: impl FnMut() -> io::Result<()>,
    alive: impl Fn() -> bool,
) -> io::Result<()> {
    for attempt in 0..3 {
        match scan() {
            Ok(()) => return Ok(()),
            Err(_) if !alive() => return Ok(()),
            Err(error) if attempt == 2 => return Err(error),
            Err(_) => {
                // Linux's /proc filesystem can briefly deny access during exec or process exit.
                // Persistent refusal still prevents eviction rather than
                // guessing that the process has no image open.
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
    unreachable!()
}

#[cfg(target_os = "linux")]
pub fn snapshot() -> io::Result<Usage> {
    use std::os::unix::fs::MetadataExt;
    let mut usage = Usage::default();
    // Other users cannot enter the private temp directory. Inspect all of our
    // processes, including orphaned children, rather than only test descendants.
    for process in fs::read_dir("/proc")? {
        let process = process?;
        let Ok(pid) = process.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(metadata) = process.metadata() else {
            continue;
        };
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let mut scan = || -> io::Result<()> {
            usage.record(&process.path().join("exe"));
            for fd in fs::read_dir(process.path().join("fd"))? {
                usage.record(&fd?.path());
            }
            let maps = fs::read(process.path().join("maps"))?;
            for line in String::from_utf8_lossy(&maps).lines() {
                let fields: Vec<_> = line.split_whitespace().take(5).collect();
                if fields.len() != 5 {
                    continue;
                }
                let Some((major, minor)) = fields[3].split_once(':') else {
                    continue;
                };
                if let (Ok(major), Ok(minor), Ok(inode)) = (
                    u32::from_str_radix(major, 16),
                    u32::from_str_radix(minor, 16),
                    fields[4].parse::<u64>(),
                ) {
                    if inode != 0 {
                        usage
                            .files
                            .insert((libc::makedev(major, minor) as u64, inode));
                    }
                }
            }
            Ok(())
        };
        scan_process(&mut scan, || {
            if !pid_alive(pid) {
                return false;
            }
            let stat = match fs::read_to_string(process.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) => return error.kind() != io::ErrorKind::NotFound,
            };
            !matches!(
                stat.rsplit_once(") ")
                    .and_then(|(_, rest)| rest.chars().next()),
                Some('Z' | 'X')
            )
        })?;
    }
    Ok(usage)
}

#[cfg(target_os = "macos")]
pub fn snapshot() -> io::Result<Usage> {
    use std::process::Command;
    // lsof includes executable mappings and open descriptors. Inode comparison
    // also catches a running image reached through a different hard-link path.
    let output = Command::new("/usr/sbin/lsof")
        .args(["-nP", "-Fpn", "-a", "-u"])
        // SAFETY: geteuid has no preconditions.
        .arg(unsafe { libc::geteuid() }.to_string())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("cannot enumerate open fixture images"));
    }
    let mut usage = Usage::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(path) = line.strip_prefix('n') {
            usage.record(Path::new(path));
        }
    }
    Ok(usage)
}

#[cfg(windows)]
pub fn snapshot() -> io::Result<Usage> {
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process | ForEach-Object { $_.ExecutablePath }",
        ])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("cannot enumerate running fixture images"));
    }
    let mut usage = Usage::default();
    for path in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
    {
        usage.record(Path::new(path));
    }
    Ok(usage)
}

#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the NUL-terminated path and writable output live through the call.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs initialized the output on success.
    let stat = unsafe { stat.assume_init() };
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(windows)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    // SAFETY: wide is NUL-terminated and available is a live writable u64.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(test)]
impl Usage {
    pub fn hold(&mut self, path: &Path) {
        self.record(path);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn image_cache_process_probe_retries_exec_transition_refusal() {
        let mut attempts = 0;
        scan_process(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            },
            || true,
        )
        .unwrap();
        assert_eq!(attempts, 3);
    }

    #[test]
    fn image_cache_process_probe_fails_closed_for_persistent_refusal() {
        let mut attempts = 0;
        let error = scan_process(
            || {
                attempts += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            },
            || true,
        )
        .unwrap_err();
        assert_eq!(attempts, 3);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
