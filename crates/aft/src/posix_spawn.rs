//! Session-isolated pipe children without a fork of the daemon.
//!
//! File actions retain only stdio and the two wrapper marker descriptors. All
//! preparation, including PATH lookup and privacy attributes, happens in the parent.
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[derive(Debug)]
enum Handle {
    Std(std::process::Child),
    Native {
        pid: libc::pid_t,
        status: Option<ExitStatus>,
    },
}

/// Like std's Child, dropping this handle does not kill or reap a running child.
#[derive(Debug)]
pub(crate) struct Child {
    handle: Handle,
    pub(crate) stdin: Option<File>,
    reaped: Option<ExitStatus>,
    // The kernel may reuse the PID as soon as wait4 returns; keep the identity
    // that was declared, rather than looking up a possibly newer child at reap.
    agent_root: Option<subc_client_rs::AgentCommandRoot>,
}
impl From<std::process::Child> for Child {
    fn from(mut child: std::process::Child) -> Self {
        let stdin = child
            .stdin
            .take()
            .map(|pipe| File::from(OwnedFd::from(pipe)));
        Self {
            handle: Handle::Std(child),
            stdin,
            reaped: None,
            agent_root: None,
        }
    }
}
impl Child {
    pub(crate) fn declare_agent(&mut self) {
        self.agent_root = crate::agent_commands::started(self.id());
    }
    #[cfg(test)]
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        if self.reaped.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child already reaped",
            ));
        }
        match &mut self.handle {
            Handle::Std(child) => child.kill(),
            Handle::Native {
                status: Some(_), ..
            } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child already reaped",
            )),
            Handle::Native { pid, .. } => {
                if unsafe { libc::kill(*pid, libc::SIGKILL) } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            }
        }
    }
    pub(crate) fn id(&self) -> u32 {
        match &self.handle {
            Handle::Std(child) => child.id(),
            Handle::Native { pid, .. } => *pid as u32,
        }
    }
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_inner(libc::WNOHANG)
    }
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        self.wait_inner(0)?
            .ok_or_else(|| io::Error::other("waitpid returned no status"))
    }
    fn wait_inner(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if self.reaped.is_none() {
            self.reaped = crate::agent_commands::wait4(self.id(), flags, self.agent_root)?;
            if let Handle::Native { status, .. } = &mut self.handle {
                *status = self.reaped;
            }
        }
        Ok(self.reaped)
    }
}

pub(crate) struct PipeIo<'a> {
    pub(crate) gated_stdin: bool,
    pub(crate) stdout: &'a File,
    pub(crate) stderr: &'a File,
    pub(crate) exit: RawFd,
    pub(crate) failure: RawFd,
}

type AddChdir =
    unsafe extern "C" fn(*mut libc::posix_spawn_file_actions_t, *const libc::c_char) -> libc::c_int;
#[cfg(not(target_os = "macos"))]
type CloseFrom =
    unsafe extern "C" fn(*mut libc::posix_spawn_file_actions_t, libc::c_int) -> libc::c_int;
#[derive(Clone, Copy)]
struct Functions {
    chdir: AddChdir,
    #[cfg(not(target_os = "macos"))]
    closefrom: CloseFrom,
}
type SpawnBackend = Result<Functions, String>;
static BACKEND: OnceLock<SpawnBackend> = OnceLock::new();
static FALLBACK_SPAWNS: AtomicU64 = AtomicU64::new(0);

// Resolve optional libc extensions at runtime so release binaries retain their
// old glibc baseline instead of acquiring GLIBC_2.29/GLIBC_2.34 symbol versions.
fn symbol(name: &CStr) -> *mut libc::c_void {
    unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
}
fn discover() -> SpawnBackend {
    if !cfg!(any(
        target_os = "macos",
        all(target_os = "linux", target_env = "gnu")
    )) {
        return Err("unsupported Unix libc".into());
    }
    #[cfg(target_os = "macos")]
    let chdir = {
        let modern = symbol(c"posix_spawn_file_actions_addchdir");
        if modern.is_null() {
            symbol(c"posix_spawn_file_actions_addchdir_np")
        } else {
            modern
        }
    };
    #[cfg(not(target_os = "macos"))]
    let chdir = symbol(c"posix_spawn_file_actions_addchdir_np");
    if chdir.is_null() {
        return Err("posix_spawn_file_actions_addchdir_np".into());
    }
    #[cfg(not(target_os = "macos"))]
    let closefrom = symbol(c"posix_spawn_file_actions_addclosefrom_np");
    #[cfg(not(target_os = "macos"))]
    if closefrom.is_null() {
        return Err("posix_spawn_file_actions_addclosefrom_np".into());
    }
    let functions = Functions {
        chdir: unsafe { std::mem::transmute::<*mut libc::c_void, AddChdir>(chdir) },
        #[cfg(not(target_os = "macos"))]
        closefrom: unsafe { std::mem::transmute::<*mut libc::c_void, CloseFrom>(closefrom) },
    };
    // Older libc may export the file actions yet reject SETSID. Probe before
    // accepting work, not by retrying a user's command after an ambiguous error.
    let null = File::options()
        .read(true)
        .write(true)
        .open("/dev/null")
        .map_err(|error| format!("capability probe open /dev/null: {error}"))?;
    let mut command = Command::new("/usr/bin/true");
    command.env_clear().current_dir("/");
    let mut child = spawn_native(
        &command,
        true,
        false,
        &PipeIo {
            gated_stdin: false,
            stdout: &null,
            stderr: &null,
            exit: null.as_raw_fd(),
            failure: null.as_raw_fd(),
        },
        functions,
    )
    .map_err(|error| format!("POSIX_SPAWN_SETSID probe: {error}"))?;
    if !child
        .wait()
        .map_err(|error| format!("POSIX_SPAWN_SETSID wait: {error}"))?
        .success()
    {
        return Err("POSIX_SPAWN_SETSID probe exited unsuccessfully".into());
    }
    Ok(functions)
}

pub(crate) fn initialize() {
    BACKEND.get_or_init(|| {
        let backend = discover();
        if let Err(reason) = &backend {
            crate::slog_warn!("bash_spawn=fork_fallback missing_capability={reason}");
        } else {
            crate::slog_info!("bash_spawn=posix_spawn");
        }
        backend
    });
}

#[cfg(test)]
thread_local! { pub(crate) static FORCE_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
fn missing_capability() -> Option<String> {
    #[cfg(test)]
    if FORCE_FALLBACK.get() {
        return Some("test-forced missing capability".into());
    }
    initialize();
    BACKEND.get().unwrap().as_ref().err().cloned()
}
pub(crate) fn health() -> serde_json::Value {
    let missing = missing_capability();
    serde_json::json!({
        "backend": if missing.is_some() { "fork_fallback" } else { "posix_spawn" },
        "missing_capability": missing,
        "fallback_spawns": FALLBACK_SPAWNS.load(Ordering::Relaxed),
        "pty_backend": "fork",
    })
}

/// None means the caller must use its existing pre_exec composition unchanged.
pub(crate) fn try_spawn(
    command: &Command,
    cleared: bool,
    disclaim: bool,
    io: &PipeIo<'_>,
) -> Option<io::Result<Child>> {
    if missing_capability().is_some() {
        FALLBACK_SPAWNS.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(spawn_native(
        command,
        cleared,
        disclaim,
        io,
        *BACKEND.get().unwrap().as_ref().unwrap(),
    ))
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum NativeTestPlatform {
    MacOs,
    LinuxGnu,
    Other,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum NativeTestDecision {
    Run,
    Skip(String),
    Fail(String),
}

#[cfg(test)]
fn native_test_decision(
    platform: NativeTestPlatform,
    glibc_version: Option<(u32, u32)>,
    missing: Option<&str>,
) -> NativeTestDecision {
    match platform {
        NativeTestPlatform::MacOs => match missing {
            Some(reason) => NativeTestDecision::Fail(format!(
                "macOS requires the native POSIX spawn backend, but it is unavailable: {reason}"
            )),
            None => NativeTestDecision::Run,
        },
        NativeTestPlatform::LinuxGnu => {
            let Some(reason) = missing else {
                return NativeTestDecision::Run;
            };
            let Some((capability, minimum)) = native_capability_minimum(reason) else {
                return NativeTestDecision::Fail(format!(
                    "glibc native POSIX spawn capability check failed unexpectedly: {reason}"
                ));
            };
            let Some(version) = glibc_version else {
                return NativeTestDecision::Fail(format!(
                    "could not determine glibc version while checking {capability}: {reason}"
                ));
            };
            if version < minimum {
                NativeTestDecision::Skip(format!(
                    "glibc {}.{} predates {capability} (requires {}.{}): {reason}",
                    version.0, version.1, minimum.0, minimum.1
                ))
            } else {
                NativeTestDecision::Fail(format!(
                    "glibc {}.{} should provide {capability} (available since {}.{}), but the native backend is unavailable: {reason}",
                    version.0, version.1, minimum.0, minimum.1
                ))
            }
        }
        NativeTestPlatform::Other => NativeTestDecision::Skip(
            "native POSIX spawn tests require macOS or Linux with glibc".into(),
        ),
    }
}

#[cfg(test)]
fn native_capability_minimum(reason: &str) -> Option<(&'static str, (u32, u32))> {
    if reason.contains("posix_spawn_file_actions_addclosefrom_np") {
        Some(("posix_spawn_file_actions_addclosefrom_np", (2, 34)))
    } else if reason.contains("posix_spawn_file_actions_addchdir_np") {
        Some(("posix_spawn_file_actions_addchdir_np", (2, 29)))
    } else if reason.contains("POSIX_SPAWN_SETSID") {
        Some(("POSIX_SPAWN_SETSID", (2, 26)))
    } else {
        None
    }
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
fn test_glibc_version() -> Option<(u32, u32)> {
    let version = unsafe { CStr::from_ptr(libc::gnu_get_libc_version()) }
        .to_str()
        .ok()?;
    let mut components = version.split('.');
    Some((
        components.next()?.parse().ok()?,
        components.next()?.parse().ok()?,
    ))
}

#[cfg(all(test, not(all(target_os = "linux", target_env = "gnu"))))]
fn test_glibc_version() -> Option<(u32, u32)> {
    None
}

/// Returns false only when native-only assertions do not apply on this libc.
#[cfg(test)]
pub(crate) fn native_backend_for_tests() -> bool {
    initialize();
    let platform = if cfg!(target_os = "macos") {
        NativeTestPlatform::MacOs
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        NativeTestPlatform::LinuxGnu
    } else {
        NativeTestPlatform::Other
    };
    let missing = BACKEND.get().unwrap().as_ref().err().map(String::as_str);
    match native_test_decision(platform, test_glibc_version(), missing) {
        NativeTestDecision::Run => true,
        NativeTestDecision::Skip(reason) => {
            eprintln!("skipping native-only spawn assertion: {reason}");
            false
        }
        NativeTestDecision::Fail(reason) => panic!("{reason}"),
    }
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc))
    }
}
fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "command or environment contains NUL",
        )
    })
}

fn default_search_path() -> io::Result<OsString> {
    let length = unsafe { libc::confstr(libc::_CS_PATH, std::ptr::null_mut(), 0) };
    if length == 0 {
        return Err(io::Error::other(
            "default executable search path unavailable",
        ));
    }
    let mut buffer = vec![0u8; length];
    let written =
        unsafe { libc::confstr(libc::_CS_PATH, buffer.as_mut_ptr().cast(), buffer.len()) };
    if written != length {
        return Err(io::Error::other(
            "default executable search path changed during lookup",
        ));
    }
    let path = CStr::from_bytes_with_nul(&buffer)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(OsStr::from_bytes(path.to_bytes()).to_owned())
}
struct Attributes(libc::posix_spawnattr_t);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawnattr_destroy(&mut self.0);
        }
    }
}
struct Actions(libc::posix_spawn_file_actions_t);
impl Drop for Actions {
    fn drop(&mut self) {
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.0);
        }
    }
}

fn high_copy(fd: RawFd) -> io::Result<File> {
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if copy < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(copy) })
    }
}
fn input(gated: bool) -> io::Result<(File, Option<File>)> {
    if !gated {
        return Ok((File::open("/dev/null")?, None));
    }
    // Use the same CLOEXEC pipe implementation as std::Command's piped stdin.
    // The spawn allowlist also closes both originals in the child after dup2.
    let (read, write) = std::io::pipe()?;
    Ok((
        File::from(OwnedFd::from(read)),
        Some(File::from(OwnedFd::from(write))),
    ))
}

fn spawn_native(
    command: &Command,
    cleared: bool,
    disclaim: bool,
    io: &PipeIo<'_>,
    functions: Functions,
) -> io::Result<Child> {
    let mut environment = if cleared {
        BTreeMap::new()
    } else {
        std::env::vars_os().collect::<BTreeMap<_, _>>()
    };
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            environment.insert(key.to_owned(), value.to_owned());
        } else {
            environment.remove(key);
        }
    }
    let cwd = command
        .get_current_dir()
        .map(Path::to_owned)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)?;
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir()?.join(cwd)
    };
    let program = command.get_program();
    // Resolve only after applying the complete child environment; posix_spawnp
    // would instead search the daemon's PATH. Preserve the requested argv[0].
    let path = if Path::new(program).is_absolute() {
        Path::new(program).to_owned()
    } else {
        // execvp searches the system default only when PATH is absent. An
        // explicitly empty PATH still searches cwd, without exporting a default.
        let search_path = environment
            .get(OsStr::new("PATH"))
            .cloned()
            .map(Ok)
            .unwrap_or_else(default_search_path)?;
        if search_path.is_empty() {
            cwd.join(program)
        } else {
            which::which_in(program, Some(search_path), &cwd)
                .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error))?
        }
    };
    let path = cstring(path.as_os_str())?;
    let cwd = cstring(cwd.as_os_str())?;
    let argv_storage = std::iter::once(program)
        .chain(command.get_args())
        .map(cstring)
        .collect::<io::Result<Vec<_>>>()?;
    let env_storage = environment
        .into_iter()
        .map(|(key, value)| {
            let mut entry = OsString::from(key);
            entry.push("=");
            entry.push(value);
            cstring(&entry)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let pointers = |values: &[CString]| {
        values
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect::<Vec<_>>()
    };
    let argv = pointers(&argv_storage);
    let env = pointers(&env_storage);
    unsafe {
        let mut attr = std::mem::zeroed();
        check(libc::posix_spawnattr_init(&mut attr))?;
        let mut attr = Attributes(attr);
        // Darwin's sys/spawn.h exposes SETSID even though libc does not name it.
        #[cfg(target_os = "macos")]
        const POSIX_SPAWN_SETSID: libc::c_int = 0x0400;
        #[cfg(target_os = "macos")]
        let flags =
            POSIX_SPAWN_SETSID | libc::POSIX_SPAWN_CLOEXEC_DEFAULT | libc::POSIX_SPAWN_SETSIGDEF;
        #[cfg(target_os = "linux")]
        let flags = libc::POSIX_SPAWN_SETSID | libc::POSIX_SPAWN_SETSIGDEF;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let flags = libc::POSIX_SPAWN_SETSIGDEF;
        let flags = flags | libc::POSIX_SPAWN_SETSIGMASK;
        check(libc::posix_spawnattr_setflags(&mut attr.0, flags as i16))?;
        let mut defaults = std::mem::zeroed();
        libc::sigemptyset(&mut defaults);
        libc::sigaddset(&mut defaults, libc::SIGPIPE);
        check(libc::posix_spawnattr_setsigdefault(&mut attr.0, &defaults))?;
        // Match std::Command: signal-handling threads may block signals that
        // must remain deliverable to children, so start with an empty mask.
        let mut mask = std::mem::zeroed();
        check(libc::sigemptyset(&mut mask))?;
        check(libc::posix_spawnattr_setsigmask(&mut attr.0, &mask))?;
        crate::privacy_spawn::apply_spawn_attribute(&mut attr.0, disclaim)?;
        let mut actions = std::mem::zeroed();
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        let mut actions = Actions(actions);
        let (stdin, parent_stdin) = input(io.gated_stdin)?;
        // Copy every source above the targets before constructing actions: even
        // when the daemon has closed stdio, no dup2 can overwrite another source.
        let sources = [
            stdin.as_raw_fd(),
            io.stdout.as_raw_fd(),
            io.stderr.as_raw_fd(),
            io.exit,
            io.failure,
        ]
        .into_iter()
        .map(high_copy)
        .collect::<io::Result<Vec<_>>>()?;
        for (target, source) in sources.iter().enumerate() {
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions.0,
                source.as_raw_fd(),
                target as RawFd,
            ))?;
        }
        check((functions.chdir)(&mut actions.0, cwd.as_ptr()))?;
        #[cfg(not(target_os = "macos"))]
        check((functions.closefrom)(&mut actions.0, 5))?;
        let mut pid = 0;
        check(libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            &actions.0,
            &attr.0,
            argv.as_ptr(),
            env.as_ptr(),
        ))?;
        Ok(Child {
            handle: Handle::Native { pid, status: None },
            stdin: parent_stdin,
            reaped: None,
            agent_root: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn spawn(command: &Command, root: &Path, gated: bool) -> (Child, [File; 4]) {
        assert!(
            native_backend_for_tests(),
            "native spawn helper called when native-only tests should be skipped"
        );
        let files =
            ["out", "err", "exit", "failure"].map(|name| File::create(root.join(name)).unwrap());
        let child = try_spawn(
            command,
            true,
            false,
            &PipeIo {
                gated_stdin: gated,
                stdout: &files[0],
                stderr: &files[1],
                exit: files[2].as_raw_fd(),
                failure: files[3].as_raw_fd(),
            },
        )
        .unwrap_or_else(|| panic!("modern libc required for native tests: {}", health()))
        .unwrap();
        (child, files)
    }

    #[test]
    fn native_test_policy_skips_only_versioned_glibc_capability_gaps() {
        assert!(matches!(
            native_test_decision(
                NativeTestPlatform::LinuxGnu,
                Some((2, 33)),
                Some("posix_spawn_file_actions_addclosefrom_np")
            ),
            NativeTestDecision::Skip(_)
        ));
        assert!(matches!(
            native_test_decision(
                NativeTestPlatform::LinuxGnu,
                Some((2, 34)),
                Some("posix_spawn_file_actions_addclosefrom_np")
            ),
            NativeTestDecision::Fail(_)
        ));
        assert!(matches!(
            native_test_decision(NativeTestPlatform::MacOs, None, None),
            NativeTestDecision::Run
        ));
        assert!(matches!(
            native_test_decision(
                NativeTestPlatform::MacOs,
                None,
                Some("posix_spawn_file_actions_addchdir_np")
            ),
            NativeTestDecision::Fail(_)
        ));
    }

    #[test]
    fn native_spawn_cwd_env_arg0_markers_and_stdin() {
        if !native_backend_for_tests() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("shell");
        std::os::unix::fs::symlink("/bin/sh", &script).unwrap();
        let mut command = Command::new("shell");
        command.current_dir(dir.path()).env_clear().env("PATH", dir.path()).env("TOKEN", "child-value")
            .args(["-c", "read release; printf '%s:%s:%s:%s' \"$0\" \"$TOKEN\" \"$PWD\" \"$release\"; printf exit >&3; printf fail >&4", "requested-zero"]);
        let (mut child, _files) = spawn(&command, dir.path(), true);
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert!(child.try_wait().unwrap().is_none());
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"released\n")
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(0));
        assert!(
            child.kill().is_err(),
            "never signal a pid once it has been reaped"
        );
        // The child's shell derives $PWD from the physical working folder, so
        // compare against the canonical path (/private/var on macOS, not /var).
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out")).unwrap(),
            format!(
                "requested-zero:child-value:{}:released",
                dir.path().canonicalize().unwrap().display()
            )
        );
        assert_eq!(std::fs::read(dir.path().join("exit")).unwrap(), b"exit");
        assert_eq!(std::fs::read(dir.path().join("failure")).unwrap(), b"fail");
    }

    #[test]
    fn native_spawn_signal_mask_probe_child() {
        if std::env::var_os("AFT_NATIVE_SIGNAL_MASK_PROBE").is_none() {
            return;
        }
        let mut mask = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        assert_eq!(
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) },
            0
        );
        let blocked = (1..=64)
            .filter(|signal| unsafe { libc::sigismember(&mask, *signal) } == 1)
            .collect::<Vec<_>>();
        println!("child blocked signals: {blocked:?}");
        assert!(
            blocked.is_empty(),
            "child inherited blocked signals: {blocked:?}"
        );
    }

    #[test]
    fn native_spawn_clears_spawning_thread_signal_mask() {
        if !native_backend_for_tests() {
            return;
        }
        struct RestoreMask(libc::sigset_t);
        impl Drop for RestoreMask {
            fn drop(&mut self) {
                assert_eq!(
                    unsafe {
                        libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut())
                    },
                    0
                );
            }
        }
        initialize();
        assert_eq!(health()["backend"], "posix_spawn");
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .current_dir(dir.path())
            .env_clear()
            .env("AFT_NATIVE_SIGNAL_MASK_PROBE", "1")
            .args([
                "--exact",
                "posix_spawn::tests::native_spawn_signal_mask_probe_child",
                "--nocapture",
            ]);
        let (mut child, _files) = {
            let mut blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            unsafe {
                assert_eq!(libc::sigemptyset(&mut blocked), 0);
                assert_eq!(libc::sigaddset(&mut blocked, libc::SIGTERM), 0);
                assert_eq!(libc::sigaddset(&mut blocked, libc::SIGINT), 0);
                assert_eq!(
                    libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous),
                    0
                );
            }
            let _restore = RestoreMask(previous);
            let mut observed = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut observed)
                },
                0
            );
            assert_eq!(unsafe { libc::sigismember(&observed, libc::SIGTERM) }, 1);
            assert_eq!(unsafe { libc::sigismember(&observed, libc::SIGINT) }, 1);
            spawn(&command, dir.path(), false)
        };
        let status = child.wait().unwrap();
        let output = std::fs::read_to_string(dir.path().join("out")).unwrap();
        assert!(
            status.success(),
            "{} {}",
            output,
            std::fs::read_to_string(dir.path().join("err")).unwrap()
        );
        assert!(
            output.contains("child blocked signals: []"),
            "probe did not report an empty mask: {output}"
        );
    }

    #[test]
    fn native_spawn_absent_path_uses_default_search_but_empty_path_uses_cwd() {
        if !native_backend_for_tests() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("env");
        crate::executable_cache::write_executable(&shadow, b"#!/bin/sh\nprintf cwd-shadow")
            .unwrap();
        for (path, expected) in [
            (None, "NATIVE_TOKEN=default-search\n"),
            (Some(""), "cwd-shadow"),
        ] {
            let mut command = Command::new("env");
            command
                .current_dir(dir.path())
                .env_clear()
                .env("NATIVE_TOKEN", "default-search");
            if let Some(path) = path {
                command.env("PATH", path);
            }
            let (mut child, _files) = spawn(&command, dir.path(), false);
            assert!(child.wait().unwrap().success());
            assert_eq!(
                std::fs::read_to_string(dir.path().join("out")).unwrap(),
                expected,
                "PATH={path:?}"
            );
        }
    }

    #[test]
    fn native_spawn_reports_exec_errors() {
        if !native_backend_for_tests() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let not_executable = dir.path().join("not-executable");
        std::fs::write(&not_executable, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644)).unwrap();
        for (program, errno) in [
            (dir.path().join("absent"), libc::ENOENT),
            (not_executable, libc::EACCES),
        ] {
            let mut command = Command::new(program);
            command.current_dir(dir.path());
            let file = File::create(dir.path().join("out")).unwrap();
            let error = try_spawn(
                &command,
                true,
                false,
                &PipeIo {
                    gated_stdin: false,
                    stdout: &file,
                    stderr: &file,
                    exit: file.as_raw_fd(),
                    failure: file.as_raw_fd(),
                },
            )
            .unwrap()
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(errno));
        }
    }

    #[test]
    fn native_spawn_fd_probe_child() {
        let Some(fd) = std::env::var_os("AFT_NATIVE_FD_PROBE") else {
            return;
        };
        let fd = fd.to_str().unwrap().parse::<i32>().unwrap();
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) },
            -1,
            "daemon fd leaked into child"
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        assert_ne!(
            unsafe { libc::fcntl(3, libc::F_GETFD) },
            -1,
            "exit marker missing"
        );
        assert_ne!(
            unsafe { libc::fcntl(4, libc::F_GETFD) },
            -1,
            "failure marker missing"
        );
    }

    #[test]
    fn native_spawn_closes_non_cloexec_daemon_fd() {
        if !native_backend_for_tests() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("secret")).unwrap();
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(raw >= 200);
        let _held = unsafe { File::from_raw_fd(raw) };
        assert_eq!(
            unsafe { libc::fcntl(raw, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .current_dir(dir.path())
            .env_clear()
            .env("AFT_NATIVE_FD_PROBE", raw.to_string())
            .args([
                "--exact",
                "posix_spawn::tests::native_spawn_fd_probe_child",
                "--nocapture",
            ]);
        let (mut child, _files) = spawn(&command, dir.path(), false);
        assert!(
            child.wait().unwrap().success(),
            "{}",
            std::fs::read_to_string(dir.path().join("out")).unwrap()
        );
    }
}
