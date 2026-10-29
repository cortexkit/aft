//! Process-lifetime re-exec identity, independent of the deployment pathname.
//!
//! Placement may rename a new binary over that pathname while this process is
//! serving requests. Children must keep using the image that started the server.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

struct PinnedExecutable {
    executable: PathBuf,
    directory: PathBuf,
}

static PINNED: OnceLock<PinnedExecutable> = OnceLock::new();

#[cfg(target_os = "macos")]
struct StartupSource {
    path: PathBuf,
    file: std::fs::File,
}
#[cfg(target_os = "macos")]
static SOURCE: OnceLock<Result<StartupSource, String>> = OnceLock::new();

/// On macOS, hold the startup executable open without creating a persistent
/// pin. Metadata-only CLI modes may call this; other platforms need no handle.
pub fn capture_at_startup() {
    #[cfg(target_os = "macos")]
    SOURCE.get_or_init(|| {
        let path = std::env::current_exe().map_err(|error| error.to_string())?;
        let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
        Ok(StartupSource { path, file })
    });
}

/// Called once by the binary, before any serving-process child can be spawned.
pub fn initialize() -> io::Result<()> {
    if PINNED.get().is_some() {
        return Ok(());
    }
    let storage = crate::bash_background::storage_dir(None);
    let pin = prepare(&storage)?;
    PINNED
        .set(pin)
        .map_err(|_| io::Error::other("executable pin was initialized concurrently"))
}

fn prepare(storage: &Path) -> io::Result<PinnedExecutable> {
    // Resolve the image before touching storage. Keep an open source when a
    // copy is needed so a concurrent placement cannot change the copied bytes.
    #[cfg(target_os = "macos")]
    capture_at_startup();
    #[cfg(target_os = "macos")]
    let source = SOURCE
        .get()
        .expect("startup source captured")
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))?;
    #[cfg(target_os = "macos")]
    let mut source_file = source.file.try_clone()?;

    crate::private_storage::open_root(storage)?;
    let storage = storage.canonicalize()?;
    let run = storage.join("run");
    match crate::private_storage::create_dir(&run) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    // Require the storage `run` path to be a directory, not a symlink.
    if !std::fs::symlink_metadata(&run)?.file_type().is_dir() {
        return Err(io::Error::other(
            "AFT storage run domain is not a directory",
        ));
    }
    crate::private_storage::tighten_open_dir(&storage, &run);
    cleanup(&run)?;
    let pid = std::process::id();
    let start = crate::root_cache::process_start_time_ms(pid)
        .ok_or_else(|| io::Error::other("cannot determine executable pin owner start time"))?;
    let directory = run.join(format!("aft-pinned-{pid}-{start}"));
    crate::private_storage::create_dir(&directory)?;

    let result = (|| {
        #[cfg(target_os = "linux")]
        let executable = linux_image_path();
        #[cfg(target_os = "macos")]
        let executable = {
            let destination = directory.join("aft");
            materialize(&source.path, &mut source_file, &destination, |from, to| {
                std::fs::hard_link(from, to)
            })?;
            destination
        };
        // Windows holds the executable image open without write/delete sharing;
        // placement cannot replace a running .exe at its deployment pathname.
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let executable = std::env::current_exe()?;
        std::fs::metadata(&executable)?;
        Ok(PinnedExecutable {
            executable,
            directory: directory.clone(),
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(directory);
    }
    result
}

fn cleanup(run: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(run)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((pid, start)) = owner(&name.to_string_lossy()) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let alive = match crate::root_cache::process_start_time_ms(pid) {
            Some(actual) => actual == start,
            // An inaccessible start time is not proof of death.
            None => crate::fs_lock::process_alive(pid),
        };
        if !alive {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn owner(name: &str) -> Option<(u32, u64)> {
    let (pid, start) = name.strip_prefix("aft-pinned-")?.split_once('-')?;
    Some((pid.parse().ok()?, start.parse().ok()?))
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn materialize(
    source_path: &Path,
    source: &mut std::fs::File,
    destination: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match link(source_path, destination) {
        Ok(()) => {
            let held = source.metadata()?;
            let linked = std::fs::metadata(destination)?;
            if (held.dev(), held.ino()) != (linked.dev(), linked.ino()) {
                let _ = std::fs::remove_file(destination);
                return Err(io::Error::other("deployment image changed while pinning"));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(error),
        Err(_) => {
            // Storage may be on another volume. Stream the already-open image
            // unchanged: Mach-O's embedded code signature covers its bytes.
            let mut output = crate::private_storage::executable_options()
                .write(true)
                .create_new(true)
                .open(destination)?;
            io::copy(source, &mut output)?;
            output.sync_all()?;
            output.set_permissions(std::fs::Permissions::from_mode(0o500))?;
        }
    }
    Ok(())
}

/// Return the startup pin. Library-only hosts retain their platform's image
/// discovery when they have not opted into binary startup initialization.
pub fn path() -> io::Result<PathBuf> {
    if let Some(pin) = PINNED.get() {
        return Ok(pin.executable.clone());
    }
    // Library consumers do not enter main(). On Linux the kernel already holds
    // the image; on other platforms an embedding host must initialize the pin.
    #[cfg(target_os = "linux")]
    return Ok(linux_image_path());
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe()
}

pub(crate) fn shims_dir(storage: &Path) -> PathBuf {
    match PINNED.get() {
        // A shared, mutable `shims/gh` could be retargeted by a second daemon
        // running a different build. Keep each server's PATH entry immutable.
        Some(pin) => pin.directory.join("shims"),
        None => storage.join(crate::agent_child_env::SHIMS_DIR_NAME),
    }
}

#[cfg(target_os = "linux")]
fn linux_image_path() -> PathBuf {
    // When a child shell resolves /proc/self/exe, it names the shell's image,
    // not AFT's. Use the serving PID for both the supervisor and the gh link.
    PathBuf::from(format!("/proc/{}/exe", std::process::id()))
}

/// A procfs image remains executable after unlink, but realpath cannot resolve
/// its deleted deployment name. Preserve only an exact, live image alias that
/// has the same inode as this process, including in the sandbox-launch child.
pub(crate) fn running_image_read_path(path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let _image = open_running_image_read(path).ok()??;
        return Some(path.to_path_buf());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// Open only a numeric procfs image endpoint matching the executing image.
/// The launcher gives Landlock this same verified descriptor, not a reopened
/// pathname that could identify a different image by the time it is granted.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn open_running_image_read(path: &Path) -> io::Result<Option<std::fs::File>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::{ffi::CStr, fs::File};

    let Some(value) = path.to_str() else {
        return Ok(None);
    };
    let Some(pid) = value
        .strip_prefix("/proc/")
        .and_then(|value| value.strip_suffix("/exe"))
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return Ok(None);
    };
    if pid == 0 || value != format!("/proc/{pid}/exe") {
        return Ok(None);
    }
    fn require_procfs(directory: &File) -> io::Result<()> {
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the live file descriptor and writable statfs buffer are valid.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), info.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { info.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
            return Err(io::Error::other("image endpoint is not on procfs"));
        }
        Ok(())
    }
    fn open_at(parent: &File, name: &CStr, directory: bool) -> io::Result<File> {
        let flags = libc::O_PATH
            | libc::O_CLOEXEC
            | if directory {
                libc::O_DIRECTORY | libc::O_NOFOLLOW
            } else {
                0
            };
        // SAFETY: parent stays open and name is NUL-terminated. Only the known
        // procfs `exe` endpoint is opened with final-link following enabled.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor owned by this File.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn image_at(proc_root: &File, pid: u32) -> io::Result<File> {
        let name = std::ffi::CString::new(pid.to_string()).map_err(io::Error::other)?;
        let directory = open_at(proc_root, &name, true)?;
        require_procfs(&directory)?;
        open_at(&directory, c"exe", false)
    }
    let proc_root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/proc")?;
    require_procfs(&proc_root)?;
    let candidate = image_at(&proc_root, pid)?;
    let candidate_metadata = candidate.metadata()?;
    let running = image_at(&proc_root, std::process::id())?.metadata()?;
    if !candidate_metadata.is_file()
        || (candidate_metadata.dev(), candidate_metadata.ino()) != (running.dev(), running.ino())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "procfs image does not match the executing image",
        ));
    }
    Ok(Some(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_executable_explicit_gh_binary_path_wins() {
        let mut config = crate::config::Config::default();
        let explicit = std::env::current_exe()
            .unwrap()
            .with_file_name("operator-aft");
        config.gh_shim.binary_path = Some(explicit.clone());
        assert_eq!(
            crate::agent_child_env::shim_binary(&config).unwrap(),
            explicit
        );
    }

    #[test]
    fn pinned_executable_cleanup_preserves_live_owner_and_unrelated_entries() {
        let storage = tempfile::tempdir().unwrap();
        let run = storage.path();
        let pid = std::process::id();
        let start = crate::root_cache::process_start_time_ms(pid).unwrap();
        let live = run.join(format!("aft-pinned-{pid}-{start}"));
        let reused = run.join(format!("aft-pinned-{pid}-{}", start - 1));
        let unrelated = run.join("operator-state");
        for directory in [&live, &reused, &unrelated] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join("aft"), b"image").unwrap();
        }
        cleanup(run).unwrap();
        assert!(live.join("aft").exists());
        assert!(!reused.exists(), "a reused PID must not retain an old pin");
        assert!(unrelated.join("aft").exists());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_cleanup_does_not_follow_symlinks() {
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let name = format!("aft-pinned-{}-1", std::process::id());
        let link = storage.path().join(name);
        std::fs::write(outside.path().join("aft"), b"keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        cleanup(storage.path()).unwrap();
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(outside.path().join("aft")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_refuses_redirected_run_directory() {
        let storage = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), storage.path().join("run")).unwrap();
        assert!(prepare(storage.path()).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_cross_volume_fallback_preserves_signed_bytes_and_executes() {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let storage = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut source = std::fs::File::open(&executable).unwrap();
        let destination = storage.path().join("aft");
        materialize(&executable, &mut source, &destination, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        })
        .unwrap();
        let digest = |path: &Path| {
            let mut file = std::fs::File::open(path).unwrap();
            let mut hash = Sha256::new();
            io::copy(&mut file, &mut hash).unwrap();
            hash.finalize()
        };
        assert_eq!(digest(&executable), digest(&destination));
        let copied = std::fs::metadata(&destination).unwrap();
        assert_ne!(source.metadata().unwrap().ino(), copied.ino());
        assert_eq!(copied.permissions().mode() & 0o777, 0o500);
        let output = std::process::Command::new(&destination)
            .args(["--list", "pinned_executable_cleanup_preserves_live_owner"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 test"));
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_hard_link_holds_original_inode_after_replacement() {
        use std::os::unix::fs::MetadataExt;
        let storage = tempfile::tempdir().unwrap();
        let source_path = storage.path().join("deploy");
        let destination = storage.path().join("aft");
        std::fs::write(&source_path, b"original image").unwrap();
        let mut source = std::fs::File::open(&source_path).unwrap();
        materialize(&source_path, &mut source, &destination, |from, to| {
            std::fs::hard_link(from, to)
        })
        .unwrap();
        assert_eq!(
            source.metadata().unwrap().ino(),
            destination.metadata().unwrap().ino()
        );
        let replacement = storage.path().join("new-build");
        std::fs::write(&replacement, b"replacement image").unwrap();
        std::fs::rename(replacement, &source_path).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"original image");
        assert_eq!(std::fs::read(&source_path).unwrap(), b"replacement image");
    }
}
