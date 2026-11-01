//! Process-lifetime re-exec identity, independent of the deployment pathname.
//!
//! Placement may rename a new binary over the daemon's fixed deployment pathname
//! while it serves requests. Its children must keep using the startup image.
//! Standalone hosts use versioned paths and retain normal executable discovery.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

struct PinnedExecutable {
    executable: PathBuf,
    directory: PathBuf,
    state: Mutex<PinState>,
}

struct PinState {
    owner: Option<std::fs::File>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    source: std::fs::File,
}

static PINNED: OnceLock<PinnedExecutable> = OnceLock::new();

thread_local! {
    static TASK_IMAGE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

const OWNER_LOCK: &str = "owner.lock";
const TASK_IMAGE_DIR: &str = "startup-image";

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
        let path = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| error.to_string())?;
        let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
        Ok(StartupSource { path, file })
    });
}

/// Called once by the daemon, before any serving-process child can be spawned.
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
    let started = std::time::Instant::now();
    let trace = |phase: &str| {
        if cfg!(debug_assertions) && std::env::var_os("AFT_TEST_SUBC_STARTUP_TRACE").is_some() {
            eprintln!(
                "[aft-pin] pid={} phase={phase} elapsed_ms={}",
                std::process::id(),
                started.elapsed().as_millis()
            );
        }
    };
    trace("source_open_start");
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
    #[cfg(target_os = "macos")]
    let source_path = source.path.clone();
    // Landlock shells cannot dereference an unsandboxed daemon's procfs exe
    // link. Materialize that kernel-held image once, rather than per command.
    #[cfg(target_os = "linux")]
    let source_path = linux_image_path();
    #[cfg(target_os = "linux")]
    let mut source_file = std::fs::File::open(&source_path)?;

    trace("source_open_done");
    trace("storage_open_start");
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
    trace("storage_open_done");
    trace("cleanup_start");
    cleanup(&run)?;
    trace("cleanup_done");
    let pid = std::process::id();
    let start = crate::root_cache::process_start_time_ms(pid)
        .ok_or_else(|| io::Error::other("cannot determine executable pin owner start time"))?;
    let directory = run.join(format!("aft-pinned-{pid}-{start}"));
    crate::private_storage::create_dir(&directory)?;

    let result = (|| {
        trace("owner_lock_start");
        let owner = owner_lock(&directory)?;
        trace("owner_lock_done");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let executable = {
            let destination = directory.join("aft");
            trace("materialize_start");
            materialize(&source_path, &mut source_file, &destination, link_image)?;
            trace("materialize_done");
            #[cfg(target_os = "macos")]
            warm_new_image(&destination);
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
            state: Mutex::new(PinState {
                owner: Some(owner),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                source: source_file,
            }),
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
            // The kernel lease is authoritative even if another PID namespace
            // or a changed boot-clock conversion reports a different owner.
            let lease_path = entry.path().join(OWNER_LOCK);
            let lease = if lease_path.try_exists()? {
                let mut options = crate::private_storage::options();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW);
                }
                let Ok(file) = options.read(true).write(true).open(&lease_path) else {
                    continue;
                };
                if file.try_lock().is_err() {
                    continue;
                }
                Some(file)
            } else {
                None
            };
            // Windows cannot unlink an open lock file. The owner is already
            // dead and pin names are never reused, so release before removal.
            drop(lease);
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn owner_lock(directory: &Path) -> io::Result<std::fs::File> {
    let file = crate::private_storage::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.join(OWNER_LOCK))?;
    file.lock_shared()?;
    Ok(file)
}

impl PinnedExecutable {
    fn path(&self) -> io::Result<PathBuf> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if self.executable.is_file() && self.directory.is_dir() {
            return Ok(self.executable.clone());
        }
        crate::slog_warn!(
            "startup executable pin missing; restoring {}",
            self.executable.display()
        );
        crate::private_storage::create_dir_all(&self.directory)?;
        if !self.directory.join(OWNER_LOCK).try_exists()? {
            state.owner = Some(owner_lock(&self.directory)?);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let temporary = self.directory.join("aft.repin");
            let _ = std::fs::remove_file(&temporary);
            // The deployment pathname may now contain a different build. Only
            // the held startup image is a valid source for repair.
            materialize(&self.executable, &mut state.source, &temporary, |_, _| {
                Err(io::Error::other("repair from held startup image"))
            })?;
            std::fs::rename(temporary, &self.executable)?;
        }
        std::fs::metadata(&self.executable)?;
        Ok(self.executable.clone())
    }

    fn cleanup_at_exit(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.owner.take();
        let _ = std::fs::remove_dir_all(&self.directory);
    }

    fn task_image(&self, task_io: &Path) -> io::Result<PathBuf> {
        let executable = self.path()?;
        let directory = task_io.join(TASK_IMAGE_DIR);
        crate::private_storage::create_dir(&directory)?;
        let destination = task_executable(task_io);
        #[cfg(unix)]
        {
            let mut source = std::fs::File::open(&executable)?;
            materialize(&executable, &mut source, &destination, |from, to| {
                std::fs::hard_link(from, to)
            })?;
            let shims = self.directory.join("shims");
            if shims.join("gh").is_symlink() {
                let target = std::fs::read_link(shims.join("gh"))?;
                let target = if target == self.executable {
                    destination
                } else {
                    target
                };
                crate::private_storage::create_dir(&directory.join("shims"))?;
                std::os::unix::fs::symlink(target, directory.join("shims/gh"))?;
            }
        }
        #[cfg(windows)]
        {
            // Windows keeps the running deployment image locked, but not the
            // daemon-owned gh.cmd pathname. Retain both in the task's lifetime.
            if std::fs::hard_link(&executable, &destination).is_err() {
                std::fs::copy(&executable, &destination)?;
            }
            let shim = self.directory.join("shims/gh.cmd");
            if shim.is_file() {
                let contents = std::fs::read(&shim)?;
                let contents = if contents == crate::agent_child_env::windows_gh_cmd(&executable) {
                    crate::agent_child_env::windows_gh_cmd(&destination)
                } else {
                    contents
                };
                crate::private_storage::create_dir(&directory.join("shims"))?;
                std::fs::write(directory.join("shims/gh.cmd"), contents)?;
            }
        }
        Ok(directory)
    }
}

/// Freeze the launcher and shim into the task's own artifact lifetime before
/// constructing a native profile or launching an unconfined shell. Foreground
/// tasks need the same lifetime because they can be promoted to background.
pub(crate) fn with_task_image<R>(task_io: &Path, run: impl FnOnce() -> R) -> io::Result<R> {
    let Some(pin) = PINNED.get() else {
        return Ok(run());
    };
    let directory = pin.task_image(task_io)?;
    struct Scope(Option<PathBuf>);
    impl Drop for Scope {
        fn drop(&mut self) {
            TASK_IMAGE.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _scope = Scope(TASK_IMAGE.with(|slot| slot.replace(Some(directory))));
    Ok(run())
}

pub(crate) fn task_executable(task_io: &Path) -> PathBuf {
    task_io
        .join(TASK_IMAGE_DIR)
        .join(if cfg!(windows) { "aft.exe" } else { "aft" })
}

/// Non-launcher plans have no launcher pathname to carry the retained image.
/// Redirect their request overrides before either the pipe or PTY adapter runs.
pub(crate) fn retain_task_environment(
    task_io: &Path,
    environment: &mut std::collections::HashMap<String, String>,
) -> io::Result<()> {
    with_task_image(task_io, || {
        let mut values = environment
            .iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        task_environment(&task_executable(task_io), &mut values);
        *environment = values
            .into_iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect();
    })
}

pub(crate) fn is_dead_pin_path(path: &Path) -> bool {
    path.is_absolute()
        && path.ancestors().any(|ancestor| {
            ancestor
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(owner)
                .is_some()
                && ancestor.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("run"))
        })
        && matches!(path.try_exists(), Ok(false))
}

pub(crate) fn task_environment(
    launcher: &Path,
    environment: &mut std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) {
    let Some(pin) = PINNED.get() else {
        return;
    };
    let Some(directory) = launcher
        .parent()
        .filter(|path| path.file_name() == Some(std::ffi::OsStr::new(TASK_IMAGE_DIR)))
    else {
        return;
    };
    let redirect = |path: &Path| {
        if path == pin.executable {
            return launcher.to_path_buf();
        }
        path.strip_prefix(&pin.directory)
            .map(|suffix| directory.join(suffix))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    for key in [
        crate::agent_child_env::GH_SHIMS_DIR_ENV,
        crate::agent_child_env::GH_SHIM_BINARY_ENV,
    ] {
        if let Some(value) = environment.get_mut(std::ffi::OsStr::new(key)) {
            *value = redirect(Path::new(value)).into_os_string();
        }
    }
    if let Some(value) = environment.get_mut(std::ffi::OsStr::new("PATH")) {
        let entries = std::env::split_paths(value)
            .map(|path| redirect(&path))
            .filter(|path| !is_dead_pin_path(path))
            .collect::<Vec<_>>();
        if let Ok(path) = std::env::join_paths(entries) {
            *value = path;
        }
    }
}

fn owner(name: &str) -> Option<(u32, u64)> {
    let (pid, start) = name.strip_prefix("aft-pinned-")?.split_once('-')?;
    Some((pid.parse().ok()?, start.parse().ok()?))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn link_image(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let source =
            std::ffi::CString::new(source.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let destination =
            std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(io::Error::other)?;
        // SAFETY: both absolute path strings are valid and NUL-terminated.
        // Follow procfs's exe link to the held executable, not the symlink inode.
        let result = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    // Never give the running daemon's executable a second name on macOS. The
    // privacy controls name the program that asks for protected files (a
    // removable volume, Documents, ...) by the path the kernel reports for the
    // responsible process's executable, and with several hard links that can
    // be any of them. Every task image then looked like a different "aft", so
    // an Allow never stuck and the prompt blocked the volume until someone
    // answered. Returning an error makes `materialize` clone or copy the held
    // image into a separate file, leaving the daemon at its one placed path.
    #[cfg(target_os = "macos")]
    {
        let _ = (source, destination);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the daemon's executable is never hard-linked on macOS",
        ))
    }
}

#[cfg(unix)]
fn materialize(
    source_path: &Path,
    source: &mut std::fs::File,
    destination: &Path,
    link: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let started = std::time::Instant::now();
    let trace = |phase: &str| {
        if cfg!(debug_assertions) && std::env::var_os("AFT_TEST_SUBC_STARTUP_TRACE").is_some() {
            eprintln!(
                "[aft-pin-image] pid={} phase={phase} elapsed_ms={}",
                std::process::id(),
                started.elapsed().as_millis()
            );
        }
    };
    trace("link_start");
    let linked = link(source_path, destination);
    if cfg!(debug_assertions) && std::env::var_os("AFT_TEST_SUBC_STARTUP_TRACE").is_some() {
        eprintln!(
            "[aft-pin-image] pid={} link_result={linked:?} source={:?} destination_parent={:?}",
            std::process::id(),
            source.metadata().map(|m| (m.dev(), m.len())),
            destination
                .parent()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.dev())
        );
    }
    match linked {
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
            // On APFS a clone of the held image is a separate file that shares
            // its blocks, so it costs no disk and keeps the signed bytes.
            #[cfg(target_os = "macos")]
            if clone_held_image(source, destination).is_ok() {
                return Ok(());
            }
            // Storage may be on another volume. Stream the already-open image
            // unchanged so embedded code signatures on macOS remain valid.
            let mut output = crate::private_storage::executable_options()
                .write(true)
                .create_new(true)
                .open(destination)?;
            trace("copy_start");
            // Cloned startup handles share a cursor. Positional reads let
            // concurrent task copies and later repairs read the complete image.
            use std::io::Write;
            use std::os::unix::fs::FileExt;
            let mut offset = 0;
            let mut buffer = [0; 64 * 1024];
            loop {
                let count = match source.read_at(&mut buffer, offset) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                };
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
                offset += count as u64;
            }
            trace("copy_done");
            trace("sync_start");
            output.sync_all()?;
            trace("sync_done");
            output.set_permissions(std::fs::Permissions::from_mode(0o500))?;
        }
    }
    Ok(())
}

/// Run a newly created image once, so its first launch is paid here.
///
/// On macOS the pin is a new file, not another name for the running image,
/// and the first launch of a new executable can be slow (the system assesses
/// it, and a large debug build pays for its size). The gh shim's version
/// probe allows only two seconds, so without this a slow first launch would
/// refuse the shim and leave shells without `gh`. A failure here is only
/// logged: the probe still decides whether the image is usable.
#[cfg(target_os = "macos")]
fn warm_new_image(image: &Path) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    const WARM_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
    let started = std::time::Instant::now();
    let mut child = match Command::new(image)
        .env_clear()
        .args(["gh-shim", "--shim-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            log::warn!("pinned image first launch failed to start: {error}");
            return;
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < WARM_BUDGET => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => {
                // SAFETY: the child leads its own process group (spawned above).
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                log::warn!(
                    "pinned image first launch did not finish within {}s",
                    WARM_BUDGET.as_secs()
                );
                return;
            }
        }
    }
    let elapsed = started.elapsed();
    if elapsed > std::time::Duration::from_secs(1) {
        log::info!("pinned image first launch took {}ms", elapsed.as_millis());
    }
}

/// Clone the held (already open) image into `destination` as a new file, then
/// make it owner-only like every other file under AFT's storage.
#[cfg(target_os = "macos")]
fn clone_held_image(source: &std::fs::File, destination: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::io::AsRawFd;
    let target =
        std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: the descriptor is open for this call's duration and the path is
    // NUL-terminated. Cloning from the descriptor, not a path, copies the image
    // this process started from even if the deployment path was replaced.
    if unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, target.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) =
        std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o500))
    {
        let _ = std::fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

/// Remove this daemon's pin on a controlled exit, including exits that skip Drop.
/// Startup cleanup handles crashes and any best-effort removal failures.
pub fn cleanup_at_exit() {
    if let Some(pin) = PINNED.get() {
        pin.cleanup_at_exit();
    }
}

/// Return the daemon's startup pin, or normal executable discovery for standalone
/// and library-only hosts that have not initialized a pin.
pub fn path() -> io::Result<PathBuf> {
    if let Some(directory) = TASK_IMAGE.with(|slot| slot.borrow().clone()) {
        return Ok(directory.join(if cfg!(windows) { "aft.exe" } else { "aft" }));
    }
    if let Some(pin) = PINNED.get() {
        return pin.path();
    }
    std::env::current_exe()
}

pub(crate) fn shims_dir(storage: &Path) -> PathBuf {
    if let Some(directory) = TASK_IMAGE.with(|slot| slot.borrow().clone()) {
        return directory.join("shims");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_executable_unpinned_path_is_current_exe() {
        assert!(PINNED.get().is_none());
        assert_eq!(path().unwrap(), std::env::current_exe().unwrap());
    }

    #[test]
    fn pinned_executable_unpinned_shims_use_storage() {
        assert!(PINNED.get().is_none());
        let storage = tempfile::tempdir().unwrap();
        assert_eq!(
            shims_dir(storage.path()),
            storage.path().join(crate::agent_child_env::SHIMS_DIR_NAME)
        );
    }

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
    fn pinned_executable_cleanup_preserves_another_live_process() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let storage = tempfile::tempdir().unwrap();
        let pid = child.id();
        let start = crate::root_cache::process_start_time_ms(pid).unwrap();
        let directory = storage.path().join(format!("aft-pinned-{pid}-{start}"));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("aft"), b"live image").unwrap();
        let result = cleanup(storage.path());
        let retained = directory.join("aft").exists();
        let alive = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
        assert!(
            alive && retained,
            "cleanup removed a live process's executable"
        );
        cleanup(storage.path()).unwrap();
        assert!(!directory.exists());
    }

    #[test]
    fn pinned_executable_cleanup_lease_overrides_mismatched_process_clock() {
        let storage = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let start = crate::root_cache::process_start_time_ms(pid).unwrap();
        let directory = storage
            .path()
            .join(format!("aft-pinned-{pid}-{}", start - 1));
        std::fs::create_dir(&directory).unwrap();
        let lease = owner_lock(&directory).unwrap();
        cleanup(storage.path()).unwrap();
        assert!(
            directory.exists(),
            "a held lease must override an apparent PID reuse"
        );
        drop(lease);
        cleanup(storage.path()).unwrap();
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_task_image_and_shim_outlive_daemon_pin() {
        let storage = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let pin = prepare(storage.path()).unwrap();
        std::fs::create_dir(pin.directory.join("shims")).unwrap();
        std::os::unix::fs::symlink(&pin.executable, pin.directory.join("shims/gh")).unwrap();
        let image = pin.task_image(task.path()).unwrap();
        pin.cleanup_at_exit();
        assert!(!pin.directory.exists());
        assert_eq!(
            std::fs::read_link(image.join("shims/gh")).unwrap(),
            image.join("aft")
        );
        let output = std::process::Command::new(image.join("shims/gh"))
            .args(["--list", "pinned_executable_cleanup_preserves_live_owner"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 test"));
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_unsandboxed_background_shim_outlives_daemon_pin() {
        use crate::bash_background::{
            persistence, registry::BgTaskRegistry, BashShell, HardKill, TaskSlot,
        };
        use crate::sandbox_spawn::SpawnPlan;
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        const CHILD: &str = "AFT_TEST_PIN_TASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // PINNED is process-global; exercise the normal task-launch path without
            // changing executable discovery for other tests in this process.
            let fixture = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args([
                "--exact",
                "pinned_executable::tests::pinned_executable_unsandboxed_background_shim_outlives_daemon_pin",
                "--nocapture",
            ]);
            for key in [
                "HOME",
                "XDG_DATA_HOME",
                "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME",
                "AFT_STORAGE_DIR",
            ] {
                let directory = fixture.path().join(key);
                std::fs::create_dir(&directory).unwrap();
                child.env(key, directory);
            }
            let output = child.env(CHILD, "1").output().unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }

        let storage = PathBuf::from(std::env::var_os("AFT_STORAGE_DIR").unwrap());
        PINNED
            .set(prepare(&storage).unwrap())
            .ok()
            .expect("isolated pin");
        let pin = PINNED.get().unwrap();
        std::fs::create_dir(pin.directory.join("shims")).unwrap();
        std::os::unix::fs::symlink(&pin.executable, pin.directory.join("shims/gh")).unwrap();
        let registry = BgTaskRegistry::default();
        struct Shutdown<'a>(&'a BgTaskRegistry);
        impl Drop for Shutdown<'_> {
            fn drop(&mut self) {
                self.0.shutdown();
            }
        }
        let _shutdown = Shutdown(&registry);
        let mut tasks = Vec::new();
        for mode in ["background", "promoted", "pty", "host"] {
            let environment = HashMap::from([
                (
                    "PATH".to_owned(),
                    std::env::join_paths([
                        pin.directory.join("shims"),
                        storage.join("run/aft-pinned-99-99/shims"),
                        PathBuf::from("/usr/bin"),
                        PathBuf::from("/bin"),
                    ])
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                ),
                (
                    crate::agent_child_env::GH_SHIMS_DIR_ENV.to_owned(),
                    pin.directory.join("shims").to_string_lossy().into_owned(),
                ),
                (
                    crate::agent_child_env::GH_SHIM_BINARY_ENV.to_owned(),
                    pin.executable.to_string_lossy().into_owned(),
                ),
            ]);
            let command = format!(
                ": > ready-{mode}; while [ ! -f release ]; do /bin/sleep 0.02; done; \
                 printf '%s\\n' \"$(command -v gh)\" \"$AFT_GH_SHIMS_DIR\" \"$AFT_GH_SHIM_BINARY\" \"$PATH\" > resolved-{mode}; \
                 gh --list pinned_executable_cleanup_preserves_live_owner > output-{mode} 2>&1; \
                 shim_status=$?; \"$AFT_GH_SHIM_BINARY\" --list pinned_executable_cleanup_preserves_live_owner > binary-output-{mode} 2>&1; \
                 printf '%s:%s' \"$shim_status\" \"$?\" > status-{mode}.tmp; /bin/mv status-{mode}.tmp status-{mode}"
            );
            let plan = if mode == "host" {
                use crate::sandbox_spawn::{
                    AuthenticatedPrincipal, HostEscalationAttempt, RequestedSandboxTier,
                    SandboxTaskKind,
                };
                let mut config = crate::config::Config {
                    project_root: Some(storage.clone()),
                    storage_dir: Some(storage.clone()),
                    ..crate::config::Config::default()
                };
                config.sandbox.profile.head = crate::config::SandboxMode::Strict;
                let ctx = crate::context::AppContext::new(
                    Box::new(crate::parser::TreeSitterProvider::new()),
                    config,
                );
                let approved = environment
                    .iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect();
                let attempt = HostEscalationAttempt {
                    grant_id: crate::sandbox_spawn::mint_host_escalation_grant(
                        &ctx,
                        &AuthenticatedPrincipal::FirstParty,
                        command.as_bytes(),
                        &storage,
                        &storage,
                        Path::new("/bin/bash"),
                        &approved,
                        &storage,
                        "pin-lifetime",
                    )
                    .unwrap(),
                    command: command.as_bytes().to_vec(),
                    root: storage.clone(),
                    cwd: storage.clone(),
                    shell_path: PathBuf::from("/bin/bash"),
                    environment: approved,
                };
                let plan = crate::sandbox_spawn::resolve_sandbox_spawn(
                    &ctx,
                    &AuthenticatedPrincipal::FirstParty,
                    RequestedSandboxTier::Host,
                    SandboxTaskKind::BashBackground,
                    &storage,
                    Some(&attempt),
                );
                assert!(
                    plan.refusal_code().is_none(),
                    "host grant rejected: {plan:?}"
                );
                plan
            } else {
                SpawnPlan::Unsandboxed
            };
            let task_id = if mode == "pty" {
                registry.spawn_pty_with_shell(
                    plan,
                    &command,
                    BashShell::Bash,
                    PathBuf::from("/bin/bash"),
                    "pin-lifetime".into(),
                    storage.clone(),
                    environment,
                    HardKill::Default,
                    None,
                    storage.clone(),
                    8,
                    false,
                    false,
                    Some(storage.clone()),
                    24,
                    80,
                )
            } else {
                registry.spawn_with_shell_in_slot(
                    plan,
                    &command,
                    BashShell::Bash,
                    PathBuf::from("/bin/bash"),
                    "pin-lifetime".into(),
                    storage.clone(),
                    environment,
                    HardKill::Default,
                    None,
                    storage.clone(),
                    if mode == "promoted" {
                        TaskSlot::Foreground
                    } else {
                        TaskSlot::Background { max: 8 }
                    },
                    false,
                    false,
                    Some(storage.clone()),
                )
            }
            .unwrap();
            if mode == "promoted" {
                assert!(registry.promote(&task_id, "pin-lifetime").unwrap());
            }
            let session = persistence::session_tasks_dir(&storage, "pin-lifetime");
            let task = persistence::resolve_task_layout(&session, &task_id).unwrap();
            tasks.push((mode, task.paths.io_dir));
        }
        let wait_for_file = |name: &str| {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !storage.join(name).is_file() {
                assert!(Instant::now() < deadline, "task never wrote {name}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        for (mode, _) in &tasks {
            wait_for_file(&format!("ready-{mode}"));
        }
        pin.cleanup_at_exit();
        assert!(!pin.directory.exists());
        std::fs::write(storage.join("release"), b"go").unwrap();
        for (mode, task_io) in tasks {
            wait_for_file(&format!("status-{mode}"));
            assert_eq!(
                std::fs::read_to_string(storage.join(format!("status-{mode}"))).unwrap(),
                "0:0",
                "{mode}: {:?}",
                std::fs::read_to_string(storage.join(format!("output-{mode}")))
            );
            let image = task_io.join(TASK_IMAGE_DIR);
            assert_eq!(
                std::fs::read_to_string(storage.join(format!("resolved-{mode}"))).unwrap(),
                format!(
                    "{}\n{}\n{}\n{}:/usr/bin:/bin\n",
                    image.join("shims/gh").display(),
                    image.join("shims").display(),
                    image.join("aft").display(),
                    image.join("shims").display()
                ),
                "{mode}",
            );
            assert!(
                std::fs::read_to_string(storage.join(format!("output-{mode}")))
                    .unwrap()
                    .contains("1 test"),
                "{mode} did not execute the retained shim"
            );
            assert!(
                std::fs::read_to_string(storage.join(format!("binary-output-{mode}")))
                    .unwrap()
                    .contains("1 test"),
                "{mode} did not execute the retained hook binary"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn pinned_executable_windows_task_wrapper_outlives_daemon_pin() {
        let storage = tempfile::tempdir().unwrap();
        let task = tempfile::tempdir().unwrap();
        let pin = prepare(storage.path()).unwrap();
        std::fs::create_dir(pin.directory.join("shims")).unwrap();
        std::fs::write(
            pin.directory.join("shims/gh.cmd"),
            crate::agent_child_env::windows_gh_cmd(&pin.executable),
        )
        .unwrap();
        let image = pin.task_image(task.path()).unwrap();
        pin.cleanup_at_exit();
        assert!(!pin.directory.exists());
        assert_eq!(
            std::fs::read(image.join("shims/gh.cmd")).unwrap(),
            crate::agent_child_env::windows_gh_cmd(&image.join("aft.exe"))
        );
        assert!(image.join("aft.exe").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_missing_pin_is_repaired_from_complete_held_image() {
        use sha2::{Digest, Sha256};
        use std::io::Seek;
        let storage = tempfile::tempdir().unwrap();
        let pin = prepare(storage.path()).unwrap();
        let digest = |path: &Path| {
            let mut hash = Sha256::new();
            io::copy(&mut std::fs::File::open(path).unwrap(), &mut hash).unwrap();
            hash.finalize()
        };
        let original = digest(&pin.executable);
        pin.state
            .lock()
            .unwrap()
            .source
            .seek(io::SeekFrom::End(0))
            .unwrap();
        std::fs::remove_dir_all(&pin.directory).unwrap();
        assert_eq!(pin.path().unwrap(), pin.executable);
        assert_eq!(original, digest(&pin.executable));
        assert!(pin.directory.join(OWNER_LOCK).exists());
        pin.cleanup_at_exit();
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

    /// The daemon's own executable keeps exactly one name: its pin is a separate
    /// file, so macOS cannot report the daemon by a pin or task-image path.
    #[cfg(target_os = "macos")]
    #[test]
    fn pinned_executable_macos_pin_is_a_separate_file_from_the_running_image() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let storage = tempfile::tempdir().unwrap();
        let executable = storage.path().join("deploy");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let mut source = std::fs::File::open(&executable).unwrap();
        let destination = storage.path().join("aft");
        materialize(&executable, &mut source, &destination, link_image).unwrap();
        let held = source.metadata().unwrap();
        let pinned = std::fs::metadata(&destination).unwrap();
        assert_ne!(held.ino(), pinned.ino(), "the pin is a hard link");
        assert_eq!(held.nlink(), 1, "the running image gained a second name");
        assert_eq!(pinned.permissions().mode() & 0o777, 0o500);
        assert_eq!(
            std::fs::read(&executable).unwrap(),
            std::fs::read(&destination).unwrap()
        );
        let output = std::process::Command::new(&destination)
            .args(["--list", "pinned_executable_cleanup_preserves_live_owner"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_executable_procfs_image_on_same_device_is_linked_without_copying() {
        use std::os::unix::fs::MetadataExt;
        let executable = std::env::current_exe().unwrap();
        let storage = tempfile::tempdir_in(executable.parent().unwrap()).unwrap();
        let proc_image = linux_image_path();
        let mut source = std::fs::File::open(&proc_image).unwrap();
        let destination = storage.path().join("pin");
        materialize(&proc_image, &mut source, &destination, link_image).unwrap();
        let held = source.metadata().unwrap();
        let pinned = std::fs::metadata(&destination).unwrap();
        assert_eq!(held.dev(), pinned.dev());
        assert_eq!(
            held.ino(),
            pinned.ino(),
            "same-device procfs image was copied"
        );
        assert_eq!(held.len(), pinned.len());
    }

    #[cfg(unix)]
    #[test]
    fn pinned_executable_rejects_a_link_to_a_replaced_image() {
        let storage = tempfile::tempdir().unwrap();
        let image = storage.path().join("image");
        std::fs::write(&image, b"original").unwrap();
        let mut source = std::fs::File::open(&image).unwrap();
        std::fs::rename(&image, storage.path().join("held-original")).unwrap();
        std::fs::write(&image, b"replacement").unwrap();
        let destination = storage.path().join("pin");
        let error = materialize(&image, &mut source, &destination, |source, destination| {
            std::fs::hard_link(source, destination)
        })
        .unwrap_err();
        assert!(error.to_string().contains("deployment image changed"));
        assert!(!destination.exists(), "mismatched image was retained");
        assert_eq!(std::fs::read(image).unwrap(), b"replacement");
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
