//! macOS privacy boundary for agent commands, not AFT's own file/index work.
//!
//! Keep Rust's child ownership, error pipe, stdio, cwd and existing pre-exec
//! setup. The final pre-exec hook uses posix_spawn(SETEXEC) instead of execve:
//! it replaces that same PID with responsibility disclaimed. There is no
//! intermediate executable, nor agent-controlled code before the boundary.

use std::collections::HashMap;
use std::process::Command;

/// Private request-snapshot control, removed before executing any child.
pub(crate) const CONTROL_ENV: &str = "AFT_AGENT_DISCLAIM_PRIVACY";

pub(crate) fn requested(environment: &HashMap<String, String>) -> bool {
    environment
        .get(CONTROL_ENV)
        .is_some_and(|value| value == "1")
}

pub(crate) fn install(
    command: &mut Command,
    enabled: bool,
    environment_cleared: bool,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if enabled {
        return macos::install(command, environment_cleared, false);
    }
    let _ = (command, enabled, environment_cleared);
    Ok(())
}

pub(crate) fn note_session(session: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::collections::HashSet;
        use std::sync::{Mutex, OnceLock};
        static NOTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        if NOTED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(session.to_owned())
        {
            crate::slog_info!(
                "agent shells run disclaimed (bash.disclaim_privacy=true, session={session})"
            );
        }
    }
    let _ = session;
}

/// portable-pty does not expose spawn attributes or pre-exec hooks. Keep its
/// master (resize/read/write) and use a Rust child with the same terminal setup
/// for the opt-in path. SETEXEC keeps the session leader's PID and terminal.
#[cfg(target_os = "macos")]
pub(crate) fn spawn_pty(
    builder: portable_pty::CommandBuilder,
    master: &dyn portable_pty::MasterPty,
) -> Result<Box<dyn portable_pty::Child + Send + Sync>, String> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let argv = builder.get_argv();
    let program = argv
        .first()
        .ok_or("privacy disclaim unavailable: PTY program missing")?;
    let mut command = Command::new(program);
    command.args(&argv[1..]);
    if let Some(cwd) = builder.get_cwd() {
        command.current_dir(cwd);
    }
    command.env_clear().env("SHELL", builder.get_shell());
    // Recover non-UTF8 inherited entries too; the builder's iterator is UTF8-only.
    let mut keys = std::env::vars_os()
        .map(|(key, _)| key)
        .collect::<BTreeSet<_>>();
    keys.extend(builder.iter_full_env_as_str().map(|(key, _)| key.into()));
    for key in keys {
        if let Some(value) = builder.get_env(&key) {
            command.env(key, value);
        }
    }
    let tty = master
        .tty_name()
        .ok_or("privacy disclaim unavailable: PTY slave name missing")?;
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty)
        .map_err(|error| format!("open PTY slave failed: {error}"))?;
    command
        .stdin(Stdio::from(
            slave.try_clone().map_err(|error| error.to_string())?,
        ))
        .stdout(Stdio::from(
            slave.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(slave));
    let controlling_tty = builder.get_controlling_tty();
    // AFT's PTY factory does not override umask; preserve the inherited mask.
    unsafe {
        command.pre_exec(move || {
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            let empty = std::mem::zeroed::<libc::sigset_t>();
            libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if controlling_tty && libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    macos::install(&mut command, true, true)?;
    let spawn_program = std::path::Path::new(command.get_program()).to_path_buf();
    let spawn_workdir = command
        .get_current_dir()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    command
        .spawn()
        .map(|child| Box::new(child) as Box<dyn portable_pty::Child + Send + Sync>)
        .map_err(|error| {
            crate::bash_background::format_spawn_failure(
                "privacy disclaim unavailable: PTY spawn failed",
                &spawn_program,
                &spawn_workdir,
                error,
            )
        })
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::BTreeMap;
    use std::ffi::{CString, OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    pub(super) type Disclaim =
        unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

    #[cfg(test)]
    thread_local! {
        pub(super) static HOOK: std::cell::Cell<Option<Option<Disclaim>>> = const { std::cell::Cell::new(None) };
    }

    fn resolve() -> Result<Disclaim, String> {
        #[cfg(test)]
        if let Some(hook) = HOOK.get() {
            return hook
                .ok_or_else(|| unavailable("responsibility_spawnattrs_setdisclaim missing"));
        }
        let symbol = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_spawnattrs_setdisclaim".as_ptr(),
            )
        };
        if symbol.is_null() {
            return Err(unavailable("responsibility_spawnattrs_setdisclaim missing"));
        }
        // The private libSystem ABI is the same one used by terminal launchers.
        Ok(unsafe { std::mem::transmute::<*mut libc::c_void, Disclaim>(symbol) })
    }

    fn unavailable(cause: impl std::fmt::Display) -> String {
        format!("privacy disclaim unavailable: {cause}")
    }

    fn check(rc: libc::c_int) -> Result<(), String> {
        if rc == 0 {
            Ok(())
        } else {
            Err(unavailable(std::io::Error::from_raw_os_error(rc)))
        }
    }

    struct Attributes(libc::posix_spawnattr_t);
    impl Attributes {
        fn new(close_extra: bool) -> Result<Self, String> {
            let mut attr = std::ptr::null_mut();
            check(unsafe { libc::posix_spawnattr_init(&mut attr) })?;
            let mut attrs = Self(attr);
            let flags = libc::POSIX_SPAWN_SETEXEC
                | if close_extra {
                    libc::POSIX_SPAWN_CLOEXEC_DEFAULT
                } else {
                    0
                };
            check(unsafe { libc::posix_spawnattr_setflags(&mut attrs.0, flags as i16) })?;
            // Resolve and apply in the parent: refusal must precede even fork.
            let disclaim = resolve()?;
            check(unsafe { disclaim(&mut attrs.0, 1) })?;
            Ok(attrs)
        }
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe {
                libc::posix_spawnattr_destroy(&mut self.0);
            }
        }
    }

    extern "C" {
        fn posix_spawn_file_actions_addinherit_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            fd: libc::c_int,
        ) -> libc::c_int;
    }
    struct Actions(libc::posix_spawn_file_actions_t);
    impl Actions {
        fn stdio_only() -> Result<Self, String> {
            let mut actions = std::ptr::null_mut();
            check(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
            let mut actions = Self(actions);
            // PTYs historically close every extra descriptor. Let the kernel
            // do that atomically, including fds another thread opened after
            // setup, while retaining the already-redirected stdio descriptors.
            for fd in 0..=2 {
                check(unsafe { posix_spawn_file_actions_addinherit_np(&mut actions.0, fd) })?;
            }
            Ok(actions)
        }
    }
    impl Drop for Actions {
        fn drop(&mut self) {
            unsafe {
                libc::posix_spawn_file_actions_destroy(&mut self.0);
            }
        }
    }

    struct Image {
        attrs: Attributes,
        actions: Option<Actions>,
        path: CString,
        _argv: Vec<CString>,
        _env: Vec<CString>,
        argv: Vec<*mut libc::c_char>,
        env: Vec<*mut libc::c_char>,
    }
    // All pointers reference immutable, owned allocations, live as long as Image.
    // Attributes are prepared only in the parent; posix_spawn reads them in the
    // forked child. No shared mutation or allocator operation occurs after fork.
    unsafe impl Send for Image {}
    unsafe impl Sync for Image {}

    impl Image {
        unsafe fn execute(&self) -> std::io::Result<()> {
            let mut pid = 0;
            let rc = libc::posix_spawn(
                &mut pid,
                self.path.as_ptr(),
                self.actions
                    .as_ref()
                    .map_or(std::ptr::null(), |actions| &actions.0),
                &self.attrs.0,
                self.argv.as_ptr(),
                self.env.as_ptr(),
            );
            // SETEXEC never returns on success. Never fall back to execve.
            Err(std::io::Error::from_raw_os_error(if rc == 0 {
                libc::EIO
            } else {
                rc
            }))
        }
    }

    fn cstring(value: &OsStr) -> Result<CString, String> {
        CString::new(value.as_bytes())
            .map_err(|_| unavailable("command or environment contains NUL"))
    }

    pub(super) fn install(
        command: &mut Command,
        cleared: bool,
        close_extra: bool,
    ) -> Result<(), String> {
        let attrs = Attributes::new(close_extra)?;
        let actions = if close_extra {
            Some(Actions::stdio_only()?)
        } else {
            None
        };
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
            .map(ToOwned::to_owned)
            .unwrap_or(std::env::current_dir().map_err(unavailable)?);
        let path = which::which_in(
            command.get_program(),
            environment.get(OsStr::new("PATH")),
            &cwd,
        )
        .map_err(|error| unavailable(format!("resolve executable: {error}")))?;
        let argv = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(cstring)
            .collect::<Result<Vec<_>, _>>()?;
        let env = environment
            .into_iter()
            .map(|(key, value)| {
                let mut entry = OsString::from(key);
                entry.push("=");
                entry.push(value);
                cstring(&entry)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let pointers = |values: &Vec<CString>| {
            values
                .iter()
                .map(|value| value.as_ptr().cast_mut())
                .chain(std::iter::once(std::ptr::null_mut()))
                .collect()
        };
        let image = Image {
            attrs,
            actions,
            path: cstring(path.as_os_str())?,
            argv: pointers(&argv),
            env: pointers(&env),
            _argv: argv,
            _env: env,
        };
        unsafe {
            command.pre_exec(move || image.execute());
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use macos::{Disclaim, HOOK};
    use std::sync::atomic::{AtomicI32, Ordering};

    fn hook<T>(value: Option<Disclaim>, test: impl FnOnce() -> T) -> T {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                HOOK.set(None);
            }
        }
        HOOK.set(Some(value));
        let _reset = Reset;
        test()
    }
    static FLAG: AtomicI32 = AtomicI32::new(-1);
    unsafe extern "C" fn capture(attrs: *mut libc::posix_spawnattr_t, flag: i32) -> i32 {
        FLAG.store(flag, Ordering::SeqCst);
        // Capture the flag while still exercising the real libSystem attribute.
        HOOK.set(None);
        let real = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_spawnattrs_setdisclaim".as_ptr(),
        );
        assert!(!real.is_null());
        let real: Disclaim = std::mem::transmute(real);
        real(attrs, flag)
    }
    unsafe extern "C" fn reject(_: *mut libc::posix_spawnattr_t, _: i32) -> i32 {
        libc::EINVAL
    }

    #[test]
    fn switch_on_sets_disclaim_attribute_to_one() {
        hook(Some(capture), || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf disclaimed; exit 23"]);
            install(&mut command, true, false).unwrap();
            assert_eq!(
                FLAG.load(Ordering::SeqCst),
                1,
                "disclaim attribute was not set to 1"
            );
            let output = command.output().unwrap();
            assert_eq!(output.stdout, b"disclaimed");
            assert_eq!(output.status.code(), Some(23));
        });
    }
    #[test]
    fn missing_symbol_refuses_before_any_process_is_spawned() {
        hook(None, || {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("executed");
            let mut command = Command::new("/usr/bin/touch");
            command.arg(&marker);
            let result = install(&mut command, true, false);
            if result.is_ok() {
                command.status().unwrap();
            }
            assert!(
                result.is_err(),
                "missing symbol allowed the command to spawn"
            );
            assert!(result
                .unwrap_err()
                .starts_with("privacy disclaim unavailable:"));
            assert!(!marker.exists());
        });
    }
    #[test]
    fn rejected_attribute_refuses_before_spawn() {
        hook(Some(reject), || {
            let mut command = Command::new("/usr/bin/true");
            let error = install(&mut command, true, false).unwrap_err();
            assert!(
                error.starts_with("privacy disclaim unavailable:"),
                "{error}"
            );
            assert!(error.contains("Invalid argument"), "{error}");
        });
    }
    #[test]
    fn switch_off_does_not_resolve_or_change_the_command() {
        hook(None, || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "printf unchanged; exit 17"]);
            install(&mut command, false, false).unwrap();
            let output = command.output().unwrap();
            assert_eq!(output.stdout, b"unchanged");
            assert_eq!(output.status.code(), Some(17));
        });
    }
    #[test]
    fn responsible_child() {
        if std::env::var_os("AFT_RESPONSIBILITY_CHILD").is_none() {
            return;
        }
        // Process attribution is prompt-free; no TCC-protected API is called.
        let symbol = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            )
        };
        assert!(!symbol.is_null());
        let responsible: unsafe extern "C" fn(i32) -> i32 = unsafe { std::mem::transmute(symbol) };
        let pid = unsafe { libc::getpid() };
        assert_eq!(
            unsafe { responsible(pid) },
            pid,
            "child is not its own responsible process"
        );
        println!("responsible-child={pid}");
    }
    #[test]
    fn real_spawn_is_its_own_responsible_process() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "privacy_spawn::tests::responsible_child",
            "--nocapture",
        ]);
        command.env("AFT_RESPONSIBILITY_CHILD", "1");
        install(&mut command, true, false).unwrap();
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("responsible-child="));
    }
}
