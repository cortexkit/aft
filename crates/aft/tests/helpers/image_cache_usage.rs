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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageUse {
    Found,
    Absent,
    Unknown,
}

#[derive(Debug)]
pub struct UnreadableProcess {
    pub pid: u32,
    pub paths: Vec<std::path::PathBuf>,
}

#[derive(Default)]
pub struct Usage {
    pub live_pids: Option<HashSet<u32>>,
    files: HashSet<FileKey>,
    pub unreadable: Vec<UnreadableProcess>,
    // Restricted procfs mounts can hide live PIDs entirely, not just deny file reads.
    pub procfs_incomplete: Option<String>,
}

impl Usage {
    pub fn pid_alive(&self, pid: u32) -> bool {
        self.live_pids
            .as_ref()
            .map_or_else(|| pid_alive(pid), |pids| pids.contains(&pid))
    }

    pub fn in_use(&self, directory: &Path) -> io::Result<ImageUse> {
        if self.contains_file(directory)? {
            Ok(ImageUse::Found)
        } else if self.unreadable.is_empty() && self.procfs_incomplete.is_none() {
            Ok(ImageUse::Absent)
        } else {
            Ok(ImageUse::Unknown)
        }
    }

    fn contains_file(&self, directory: &Path) -> io::Result<bool> {
        for child in fs::read_dir(directory)? {
            let child = child?;
            let kind = child.file_type()?;
            if (kind.is_file() && self.files.contains(&file_key(&child.path())?))
                || (kind.is_dir() && self.contains_file(&child.path())?)
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

    #[cfg(target_os = "linux")]
    fn record_probe(&mut self, path: &Path, unreadable: &mut Vec<(std::path::PathBuf, io::Error)>) {
        match file_key(path) {
            Ok(key) => {
                self.files.insert(key);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => unreadable.push((path.to_path_buf(), error)),
        }
    }

    #[cfg(target_os = "linux")]
    fn record_unreadable(
        &mut self,
        pid: u32,
        mut paths: Vec<std::path::PathBuf>,
        _error: &io::Error,
    ) {
        paths.sort();
        paths.dedup();
        self.unreadable.push(UnreadableProcess { pid, paths });
        #[cfg(test)]
        {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                .unwrap_or_else(|_| "<unavailable>".into());
            eprintln!(
                "AFT image-cache probe: unreadable_processes={} pid={pid} comm={:?} paths={:?} error={_error}",
                self.unreadable.len(), comm.trim_end(), self.unreadable.last().unwrap().paths,
            );
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
fn procfs_restriction(mountinfo: Option<&str>) -> Option<String> {
    let Some(mountinfo) = mountinfo else {
        return Some("cannot read /proc/self/mountinfo".into());
    };
    let mut found_proc = false;
    for line in mountinfo.lines() {
        let Some((mount, filesystem)) = line.split_once(" - ") else {
            continue;
        };
        let mut mount = mount.split_whitespace();
        if mount.nth(4) != Some("/proc") {
            continue;
        }
        let options = mount.next().unwrap_or("");
        let mut filesystem = filesystem.split_whitespace();
        if filesystem.next() != Some("proc") {
            continue;
        }
        found_proc = true;
        let _source = filesystem.next();
        let super_options = filesystem.next().unwrap_or("");
        for option in options.split(',').chain(super_options.split(',')) {
            if let Some(value) = option.strip_prefix("hidepid=") {
                if value != "0" && value != "off" {
                    return Some(format!("/proc is mounted with hidepid={value}"));
                }
            }
        }
    }
    if found_proc {
        None
    } else {
        Some("cannot identify the /proc mount in mountinfo".into())
    }
}

#[cfg(target_os = "linux")]
type ProbeFailures = Vec<(std::path::PathBuf, io::Error)>;

#[cfg(target_os = "linux")]
fn snapshot_with_processes(
    processes: impl IntoIterator<Item = std::path::PathBuf>,
    mountinfo: Option<&str>,
    mut probe: impl FnMut(&mut Usage, u32, &Path) -> ProbeFailures,
) -> Usage {
    let mut usage = Usage::default();
    usage.procfs_incomplete = procfs_restriction(mountinfo);
    #[cfg(test)]
    if let Some(reason) = &usage.procfs_incomplete {
        static LOG_RESTRICTION: std::sync::Once = std::sync::Once::new();
        LOG_RESTRICTION.call_once(|| {
            eprintln!("AFT image-cache probe: {reason}; unseen images are Unknown");
        });
    }
    for process in processes {
        let Some(pid) = process
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let failures = probe(&mut usage, pid, &process);
        if let Some((_, error)) = failures.first() {
            usage.record_unreadable(
                pid,
                failures.iter().map(|(path, _)| path.clone()).collect(),
                error,
            );
        }
    }
    usage
}

#[cfg(target_os = "linux")]
fn probe_process(usage: &mut Usage, pid: u32, process: &Path) -> ProbeFailures {
    use std::os::unix::fs::MetadataExt;
    let metadata = match process.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            return if pid_alive(pid) {
                vec![(process.to_path_buf(), error)]
            } else {
                Vec::new()
            }
        }
    };
    // Other users cannot enter the private temp directory. Inspect all of our
    // processes, including orphaned children, rather than only test descendants.
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Vec::new();
    }
    let mut unreadable = Vec::new();
    let mut scan = || -> io::Result<()> {
        unreadable.clear();
        usage.record_probe(&process.join("exe"), &mut unreadable);
        let fd_path = process.join("fd");
        match fs::read_dir(&fd_path) {
            Ok(fds) => {
                for fd in fds {
                    match fd {
                        Ok(fd) => usage.record_probe(&fd.path(), &mut unreadable),
                        Err(error) => unreadable.push((fd_path.clone(), error)),
                    }
                }
            }
            Err(error) => unreadable.push((fd_path, error)),
        }
        let maps_path = process.join("maps");
        let maps = match fs::read(&maps_path) {
            Ok(maps) => maps,
            Err(error) => {
                unreadable.push((maps_path, error));
                Vec::new()
            }
        };
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
        match unreadable.first() {
            Some((_, error)) => Err(io::Error::new(error.kind(), error.to_string())),
            None => Ok(()),
        }
    };
    let result = scan_process(&mut scan, || {
        if !pid_alive(pid) {
            return false;
        }
        let stat = match fs::read_to_string(process.join("stat")) {
            Ok(stat) => stat,
            Err(error) => return error.kind() != io::ErrorKind::NotFound,
        };
        !matches!(
            stat.rsplit_once(") ")
                .and_then(|(_, rest)| rest.chars().next()),
            Some('Z' | 'X')
        )
    });
    if result.is_err() {
        unreadable
    } else {
        Vec::new()
    }
}

#[cfg(target_os = "linux")]
pub fn snapshot() -> io::Result<Usage> {
    let processes = fs::read_dir("/proc")?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok();
    Ok(snapshot_with_processes(
        processes,
        mountinfo.as_deref(),
        probe_process,
    ))
}

#[cfg(all(test, target_os = "linux"))]
pub fn snapshot_process_paths(
    processes: impl IntoIterator<Item = std::path::PathBuf>,
    mountinfo: &str,
) -> Usage {
    snapshot_with_processes(processes, Some(mountinfo), probe_process)
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
pub struct NonDumpableChild {
    pub pid: u32,
    release: std::os::unix::net::UnixStream,
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for NonDumpableChild {
    fn drop(&mut self) {
        let _ = self.release.shutdown(std::net::Shutdown::Both);
        // SAFETY: pid names only the child created by non_dumpable_child.
        unsafe {
            while libc::waitpid(self.pid as i32, std::ptr::null_mut(), 0) == -1
                && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
            {}
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
pub fn non_dumpable_child() -> NonDumpableChild {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
    let parent_fd = parent.as_raw_fd();
    let child_fd = child.as_raw_fd();
    // SAFETY: the child uses only direct syscalls and exits without running Rust destructors.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            libc::close(parent_fd);
            libc::prctl(libc::PR_SET_NAME, c"aft-cache-probe".as_ptr());
            let ready = u8::from(libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) == 0);
            libc::write(child_fd, std::ptr::addr_of!(ready).cast(), 1);
            if ready == 1 {
                let mut release = 0u8;
                libc::read(child_fd, std::ptr::addr_of_mut!(release).cast(), 1);
            }
            libc::_exit(if ready == 1 { 0 } else { 1 });
        }
    }
    drop(child);
    let mut guard = NonDumpableChild {
        pid: pid as u32,
        release: parent,
    };
    let mut ready = [0];
    guard.release.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [1], "child could not disable dumpability");
    guard
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn image_cache_mount_options_make_unseen_images_unknown() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("image"), b"not observed").unwrap();
        for (options, expected) in [
            ("", ImageUse::Absent),
            (",hidepid=0", ImageUse::Absent),
            (",hidepid=off", ImageUse::Absent),
            (",hidepid=invisible", ImageUse::Unknown),
            (",hidepid=2", ImageUse::Unknown),
            (",hidepid=1", ImageUse::Unknown),
        ] {
            let mountinfo = format!("42 1 0:4 / /proc rw - proc proc rw{options}\n");
            let snapshot = snapshot_with_processes([], Some(&mountinfo), |_, _, _| unreachable!());
            assert!(snapshot.unreadable.is_empty());
            assert_eq!(snapshot.in_use(root.path()).unwrap(), expected, "{options}");
        }
    }

    #[test]
    fn image_cache_partial_snapshot_preserves_found_with_hidden_or_denied_processes() {
        let held = tempfile::tempdir().unwrap();
        let held_path = held.path().join("image");
        fs::write(&held_path, b"open image").unwrap();
        let _open = fs::File::open(held_path).unwrap();
        let unseen = tempfile::tempdir().unwrap();
        fs::write(unseen.path().join("image"), b"not observed").unwrap();
        let child = non_dumpable_child();
        assert!(pid_alive(child.pid));
        let own_path = std::path::PathBuf::from(format!("/proc/{}", std::process::id()));
        let child_path = std::path::PathBuf::from(format!("/proc/{}", child.pid));
        let mountinfo = "42 1 0:4 / /proc rw - proc proc rw\n";
        for (paths, kind) in [
            (vec![child_path.clone()], io::ErrorKind::NotFound),
            (
                vec![child_path.join("fd"), child_path.join("maps")],
                io::ErrorKind::PermissionDenied,
            ),
        ] {
            let snapshot = snapshot_with_processes(
                [own_path.clone(), child_path.clone()],
                Some(mountinfo),
                |usage, pid, path| {
                    if pid == child.pid {
                        paths
                            .iter()
                            .map(|path| (path.clone(), io::Error::from(kind)))
                            .collect()
                    } else {
                        probe_process(usage, pid, path)
                    }
                },
            );
            assert_eq!(snapshot.unreadable.len(), 1);
            assert_eq!(snapshot.unreadable[0].pid, child.pid);
            assert_eq!(snapshot.unreadable[0].paths, paths);
            assert_eq!(snapshot.in_use(held.path()).unwrap(), ImageUse::Found);
            assert_eq!(snapshot.in_use(unseen.path()).unwrap(), ImageUse::Unknown);
        }
    }

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
