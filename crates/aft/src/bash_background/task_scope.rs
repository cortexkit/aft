use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

#[derive(Clone, Copy)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by Task 4: registry scope launch")
)]
pub(crate) struct ScopePolicy {
    pub daemon_mode: bool,
    pub enabled: bool,
    pub max_runtime: Duration,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by Task 4: registry scope owner")
)]
pub(crate) struct TaskScopes {
    daemon_mode: AtomicBool,
    max_runtime: Mutex<Duration>,
    handles: Mutex<HashMap<String, ScopeHandle>>,
}

impl Default for TaskScopes {
    fn default() -> Self {
        Self {
            daemon_mode: AtomicBool::new(false),
            max_runtime: Mutex::new(Duration::from_secs(86_400)),
            handles: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by Tasks 4 and 7: registry scope management")
)]
impl TaskScopes {
    pub(crate) fn configure_runtime(&self, daemon_mode: bool, max_runtime: Duration) {
        *self.max_runtime.lock().unwrap_or_else(|e| e.into_inner()) = max_runtime;
        self.daemon_mode.store(daemon_mode, Ordering::Release);
    }

    pub(crate) fn policy(
        &self,
        enabled: bool,
        saved_max_runtime: Option<Duration>,
        native_launcher: bool,
    ) -> Option<ScopePolicy> {
        #[cfg(target_os = "linux")]
        {
            let daemon_mode = self.daemon_mode.load(Ordering::Acquire);
            if !daemon_mode || !enabled || native_launcher {
                return None;
            }
            Some(ScopePolicy {
                daemon_mode,
                enabled,
                max_runtime: saved_max_runtime
                    .unwrap_or_else(|| *self.max_runtime.lock().unwrap_or_else(|e| e.into_inner())),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (enabled, saved_max_runtime, native_launcher);
            None
        }
    }

    pub(crate) fn register(&self, handle: ScopeHandle) {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle.task_id.clone(), handle);
    }

    pub(crate) fn get(&self, task_id: &str) -> Option<ScopeHandle> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .cloned()
    }

    pub(crate) fn kill(&self, task_id: &str) -> io::Result<()> {
        if let Some(handle) = self.get(task_id) {
            handle.kill()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by Task 4: verified scope placement")
)]
pub(crate) struct ScopeHandle {
    task_id: String,
    unit: String,
    cgroup: PathBuf,
}

impl ScopeHandle {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 6: scoped memory monitor")
    )]
    pub(crate) fn cgroup(&self) -> &Path {
        &self.cgroup
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by Task 4: named scope properties")
)]
pub(crate) fn scope_unit(task_id: &str) -> io::Result<String> {
    super::persistence::validate_task_id(task_id)?;
    Ok(format!("aft-task-{task_id}.scope"))
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by Task 4: scope launch deadline")
)]
pub(crate) fn backstop(hard_kill: super::HardKill, ceiling: Duration) -> Duration {
    match hard_kill {
        super::HardKill::After(limit) => limit.saturating_add(Duration::from_secs(60)),
        super::HardKill::Default => ceiling,
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        path::Component,
        process::{Command, Output, Stdio},
        thread,
        time::Instant,
    };

    pub(crate) struct SystemdScopeLauncher {
        path: PathBuf,
        supports_expand_environment: bool,
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "used by Task 4: named scope argv")
        )]
        supports_scope_oom_policy: bool,
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: registry scope launch")
    )]
    pub(crate) struct ScopeLaunch {
        launcher: SystemdScopeLauncher,
        task_id: String,
        unit: String,
        runtime: Duration,
        memory_bytes: Option<u64>,
    }

    fn prefix(expand: bool) -> Vec<OsString> {
        let mut args: Vec<_> = ["--user", "--scope", "--collect", "--quiet"]
            .into_iter()
            .map(OsString::from)
            .collect();
        if expand {
            args.push("--expand-environment=no".into());
        }
        args
    }

    pub(crate) fn systemd_scope_argv(
        launcher: &SystemdScopeLauncher,
        executable: &Path,
        args: &[OsString],
    ) -> (PathBuf, Vec<OsString>) {
        let mut wrapped = prefix(launcher.supports_expand_environment);
        wrapped.push(executable.as_os_str().to_owned());
        wrapped.extend_from_slice(args);
        (launcher.path.clone(), wrapped)
    }

    fn run(command: &mut Command, limit: Duration) -> Result<Output, String> {
        let deadline = Instant::now() + limit;
        if limit.is_zero() {
            return Err("task scope command timed out".into());
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::super::process::start_new_session(command);
        let mut child = command
            .spawn()
            .map_err(|e| format!("task scope command failed to start: {e}"))?;
        let mut stdout = child.stdout.take().ok_or("task scope stdout missing")?;
        let mut stderr = child.stderr.take().ok_or("task scope stderr missing")?;
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                stop_client(&mut child);
                return Err(io::Error::last_os_error().to_string());
            }
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut status = None;
        loop {
            let read = drain(&mut stdout, &mut out)
                .and_then(|a| drain(&mut stderr, &mut err).map(|b| a && b));
            match read {
                Err(e) => {
                    stop_client(&mut child);
                    return Err(e);
                }
                Ok(true) if status.is_some() => {
                    return Ok(Output {
                        status: status.unwrap(),
                        stdout: out,
                        stderr: err,
                    })
                }
                _ => {}
            }
            match child.try_wait() {
                Ok(Some(value)) => status = Some(value),
                Ok(None) => {}
                Err(e) => {
                    stop_client(&mut child);
                    return Err(e.to_string());
                }
            }
            if Instant::now() >= deadline {
                stop_client(&mut child);
                return Err("task scope command timed out".into());
            }
            thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn drain(reader: &mut impl Read, data: &mut Vec<u8>) -> Result<bool, String> {
        let mut bytes = [0; 4096];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    if data.len() + n > 65_536 {
                        return Err("task scope command output exceeded 64 KiB".into());
                    }
                    data.extend_from_slice(&bytes[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    fn stop_client(child: &mut std::process::Child) {
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: stop failed scope launch")
    )]
    fn successful(output: Output) -> Result<Vec<u8>, String> {
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(format!(
                "task scope command exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn probe_systemd_scope(launcher: &Path, limit: Duration) -> Option<bool> {
        let deadline = Instant::now() + limit;
        for expand in [true, false] {
            let mut command = Command::new(launcher);
            command.args(prefix(expand)).arg("/bin/true");
            if run(
                &mut command,
                deadline.saturating_duration_since(Instant::now()),
            )
            .is_ok_and(|out| out.status.success())
            {
                return Some(expand);
            }
        }
        None
    }

    pub(crate) fn select_systemd_scope_launcher() -> Option<SystemdScopeLauncher> {
        let path = which::which("systemd-run").ok()?;
        let supports_expand_environment = probe_systemd_scope(&path, Duration::from_secs(1))?;
        let version = run(Command::new(&path).arg("--version"), Duration::from_secs(1))
            .ok()
            .filter(|out| out.status.success());
        let supports_scope_oom_policy = version
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|text| parse_version(&text))
            .is_some_and(|version| version >= 253);
        Some(SystemdScopeLauncher {
            path,
            supports_expand_environment,
            supports_scope_oom_policy,
        })
    }

    fn parse_version(text: &str) -> Option<u32> {
        let mut words = text.split_whitespace();
        (words.next()? == "systemd")
            .then(|| words.next()?.parse().ok())
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn probe_user_manager(limit: Duration) -> Result<(), String> {
        let launcher = which::which("systemd-run").map_err(|e| e.to_string())?;
        successful(run(
            Command::new(launcher).args(["--user", "--scope", "--quiet", "/bin/true"]),
            limit.min(Duration::from_secs(1)),
        )?)
        .map(|_| ())
    }

    impl TaskScopes {
        #[cfg_attr(
            not(test),
            expect(dead_code, reason = "used by Task 4: registry scope preparation")
        )]
        pub(crate) fn prepare(
            &self,
            policy: ScopePolicy,
            task_id: &str,
            hard_kill: super::super::HardKill,
            memory_mib: Option<u64>,
            native_launcher: bool,
        ) -> Result<Option<ScopeLaunch>, String> {
            if !policy.daemon_mode || !policy.enabled || native_launcher {
                return Ok(None);
            }
            let unit = scope_unit(task_id).map_err(|e| e.to_string())?;
            let memory_bytes = memory_mib
                .map(|mib| {
                    mib.checked_mul(1024 * 1024)
                        .filter(|bytes| *bytes > 0)
                        .ok_or("task scope memory cap is invalid")
                })
                .transpose()?;
            let runtime = backstop(hard_kill, policy.max_runtime);
            if runtime.is_zero() {
                return Err("task scope runtime must be positive".into());
            }
            let Some(launcher) = select_systemd_scope_launcher() else {
                static LOGGED: AtomicBool = AtomicBool::new(false);
                if !LOGGED.swap(true, Ordering::Relaxed) {
                    crate::slog_info!("bash.linux_scope: systemd user manager unavailable; spawning without a scope");
                }
                return Ok(None);
            };
            Ok(Some(ScopeLaunch {
                launcher,
                task_id: task_id.into(),
                unit,
                runtime,
                memory_bytes,
            }))
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: registry scope placement")
    )]
    impl ScopeLaunch {
        pub(crate) fn argv(
            &self,
            executable: &Path,
            args: &[OsString],
        ) -> (PathBuf, Vec<OsString>) {
            let mut wrapped = prefix(self.launcher.supports_expand_environment);
            wrapped.extend([
                format!("--unit={}", self.unit.trim_end_matches(".scope")).into(),
                "--slice=aft.slice".into(),
                format!("--property=RuntimeMaxSec={}ms", self.runtime.as_millis()).into(),
                "--property=TimeoutStopSec=2s".into(),
            ]);
            if let Some(bytes) = self.memory_bytes {
                wrapped.extend([
                    format!("--property=MemoryMax={bytes}").into(),
                    "--property=MemorySwapMax=0".into(),
                ]);
                if self.launcher.supports_scope_oom_policy {
                    wrapped.push("--property=OOMPolicy=kill".into());
                }
            }
            wrapped.push(executable.as_os_str().to_owned());
            wrapped.extend_from_slice(args);
            (self.launcher.path.clone(), wrapped)
        }

        pub(crate) fn await_placement(
            &self,
            pid: u32,
            limit: Duration,
        ) -> Result<ScopeHandle, String> {
            let deadline = Instant::now() + limit.min(Duration::from_secs(1));
            loop {
                let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                    .map_err(|e| format!("task scope child exited before placement: {e}"))?;
                if let Some(path) = membership_path(&self.task_id, &text) {
                    return Ok(ScopeHandle {
                        task_id: self.task_id.clone(),
                        unit: self.unit.clone(),
                        cgroup: path,
                    });
                }
                if !super::super::process::is_process_alive(pid) {
                    return Err("task scope child exited before placement".into());
                }
                if Instant::now() >= deadline {
                    return Err("task scope placement timed out".into());
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        pub(crate) fn stop(&self) -> Result<(), String> {
            stop_unit(&self.unit)
        }
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: exact scope placement")
    )]
    fn cgroup_path(task_id: &str, text: &str) -> Option<PathBuf> {
        let unit = scope_unit(task_id).ok()?;
        let path = Path::new(text);
        if !path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::CurDir | Component::Prefix(_)
                )
            })
        {
            return None;
        }
        if path.file_name()? != unit.as_str() || path.parent()?.file_name()? != "aft.slice" {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(path.strip_prefix("/").ok()?))
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: exact scope placement")
    )]
    fn membership_path(task_id: &str, text: &str) -> Option<PathBuf> {
        cgroup_path(
            task_id,
            text.lines().find_map(|line| line.strip_prefix("0::"))?,
        )
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 8: replay scope identity")
    )]
    pub(crate) fn matches_membership(task_id: &str, proc_cgroup: &str) -> bool {
        membership_path(task_id, proc_cgroup).is_some()
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 8: replay scope lookup")
    )]
    fn lookup_with(systemctl: &Path, task_id: &str) -> Result<Option<ScopeHandle>, String> {
        let unit = scope_unit(task_id).map_err(|e| e.to_string())?;
        let output = run(
            Command::new(systemctl).args([
                "--user",
                "show",
                "--property=ControlGroup",
                "--value",
                &unit,
            ]),
            Duration::from_secs(1),
        )?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("could not be found") || error.contains("not found") {
                return Ok(None);
            }
        }
        let bytes = successful(output)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| e.to_string())?
            .trim();
        if text.is_empty() {
            return Ok(None);
        }
        let cgroup = cgroup_path(task_id, text)
            .ok_or("task scope ControlGroup is outside its exact aft.slice unit")?;
        Ok(Some(ScopeHandle {
            task_id: task_id.into(),
            unit,
            cgroup,
        }))
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 8: replay scope lookup")
    )]
    pub(crate) fn lookup(task_id: &str) -> Result<Option<ScopeHandle>, String> {
        lookup_with(Path::new("systemctl"), task_id)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 8: owned scope cleanup")
    )]
    fn list_with(systemctl: &Path) -> Result<Vec<String>, String> {
        let bytes = successful(run(
            Command::new(systemctl).args([
                "--user",
                "list-units",
                "--all",
                "--type=scope",
                "--no-legend",
                "--plain",
                "--no-pager",
                "aft-task-*.scope",
            ]),
            Duration::from_secs(1),
        )?)?;
        let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        let mut units = Vec::new();
        for line in text.lines() {
            let Some(unit) = line.split_whitespace().next() else {
                continue;
            };
            let Some(id) = unit
                .strip_prefix("aft-task-")
                .and_then(|s| s.strip_suffix(".scope"))
            else {
                continue;
            };
            if scope_unit(id).is_ok() && lookup_with(systemctl, id)?.is_some() {
                units.push(unit.into());
            }
        }
        units.sort();
        units.dedup();
        Ok(units)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 8: owned scope cleanup")
    )]
    pub(crate) fn list_units() -> Result<Vec<String>, String> {
        list_with(Path::new("systemctl"))
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: stop failed scope launch")
    )]
    fn stop_with(systemctl: &Path, unit: &str) -> Result<(), String> {
        let id = unit
            .strip_prefix("aft-task-")
            .and_then(|s| s.strip_suffix(".scope"))
            .ok_or("invalid task scope unit")?;
        scope_unit(id).map_err(|e| e.to_string())?;
        successful(run(
            Command::new(systemctl).args(["--user", "stop", unit]),
            Duration::from_secs(5),
        )?)
        .map(|_| ())
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by Task 4: stop failed scope launch")
    )]
    fn stop_unit(unit: &str) -> Result<(), String> {
        stop_with(Path::new("systemctl"), unit)
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "used by Tasks 6 through 8: scoped memory and termination"
        )
    )]
    impl ScopeHandle {
        pub(crate) fn kill(&self) -> io::Result<()> {
            match std::fs::OpenOptions::new()
                .write(true)
                .open(self.cgroup.join("cgroup.kill"))
            {
                Ok(mut file) => file.write_all(b"1"),
                Err(e) if e.kind() == io::ErrorKind::NotFound && !self.cgroup.try_exists()? => {
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        pub(crate) fn stop(&self) -> Result<(), String> {
            stop_unit(&self.unit)
        }
        pub(crate) fn ensure_oom_group(&self) -> Result<(), String> {
            let path = self.cgroup.join("memory.oom.group");
            if std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())?
                .trim()
                == "1"
            {
                return Ok(());
            }
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|mut file| file.write_all(b"1"))
                .map_err(|e| e.to_string())?;
            if std::fs::read_to_string(path)
                .map_err(|e| e.to_string())?
                .trim()
                != "1"
            {
                return Err("task scope memory.oom.group did not become 1".into());
            }
            Ok(())
        }
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn argv_for_test(
        id: &str,
        expand: bool,
        oom_policy: bool,
        runtime: Duration,
        cap: Option<u64>,
        executable: &Path,
        args: &[OsString],
    ) -> (PathBuf, Vec<OsString>) {
        ScopeLaunch {
            launcher: SystemdScopeLauncher {
                path: "/usr/bin/systemd-run".into(),
                supports_expand_environment: expand,
                supports_scope_oom_policy: oom_policy,
            },
            task_id: id.into(),
            unit: scope_unit(id).unwrap(),
            runtime,
            memory_bytes: cap.map(|mib| mib.checked_mul(1024 * 1024).unwrap()),
        }
        .argv(executable, args)
    }

    #[cfg(test)]
    mod command_tests {
        use super::*;
        fn fake(text: &str) -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("command");
            crate::executable_cache::write_executable(
                &path,
                format!("#!/bin/sh\n{text}\n").as_bytes(),
            )
            .unwrap();
            (dir, path)
        }
        #[test]
        fn task_scope_failed_manager_and_deadline() {
            let (_dir, path) = fake("exit 1");
            assert!(probe_systemd_scope(&path, Duration::from_secs(1)).is_none());
            let (_dir, path) = fake("sleep 10");
            let start = Instant::now();
            assert!(run(&mut Command::new(path), Duration::from_millis(20))
                .unwrap_err()
                .contains("timed out"));
            assert!(start.elapsed() < Duration::from_secs(1));
        }
        #[test]
        fn task_scope_control_group_validation() {
            let id = "bash-0123456789abcdef";
            for text in [
                "",
                "/app.slice/aft-task-bash-0123456789abcdef.scope",
                "/../aft.slice/aft-task-bash-0123456789abcdef.scope",
            ] {
                let (_dir, path) = fake(&format!("printf '%s' '{text}'"));
                let result = lookup_with(&path, id);
                if text.is_empty() {
                    assert!(result.unwrap().is_none());
                } else {
                    assert!(result.is_err());
                }
            }
        }
        #[test]
        fn task_scope_malformed_list_and_stop_errors() {
            let (_dir, path) = fake("printf '%s\\n' garbage aft-task-invalid.scope --malicious");
            assert!(list_with(&path).unwrap().is_empty());
            let (_dir, path) = fake("echo refused >&2; exit 1");
            assert!(stop_with(&path, "aft-task-bash-0123456789abcdef.scope")
                .unwrap_err()
                .contains("refused"));
            assert!(stop_with(&path, "--all").is_err());
        }
        #[test]
        fn task_scope_version_capabilities() {
            assert_eq!(parse_version("systemd 253 (253.1)\n"), Some(253));
            assert_eq!(parse_version("unknown 262"), None);
            assert_eq!(parse_version("systemd unknown"), None);
        }
        #[test]
        fn lifecycle_scope_probe_deadline_falls_back() {
            let (_dir, path) = fake("sleep .1; exit 0");
            assert!(probe_systemd_scope(&path, Duration::ZERO).is_none());
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct ScopeLaunch;

#[cfg(not(target_os = "linux"))]
impl TaskScopes {
    pub(crate) fn prepare(
        &self,
        _policy: ScopePolicy,
        _task_id: &str,
        _hard_kill: super::HardKill,
        _memory_mib: Option<u64>,
        _native_launcher: bool,
    ) -> Result<Option<ScopeLaunch>, String> {
        Ok(None)
    }
}

#[cfg(not(target_os = "linux"))]
impl ScopeLaunch {
    pub(crate) fn argv(&self, executable: &Path, args: &[OsString]) -> (PathBuf, Vec<OsString>) {
        (executable.into(), args.into())
    }
    pub(crate) fn await_placement(
        &self,
        _pid: u32,
        _limit: Duration,
    ) -> Result<ScopeHandle, String> {
        Err("task scopes are unavailable on this platform".into())
    }
    pub(crate) fn stop(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl ScopeHandle {
    pub(crate) fn kill(&self) -> io::Result<()> {
        Ok(())
    }
    pub(crate) fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    pub(crate) fn ensure_oom_group(&self) -> Result<(), String> {
        Err("task scopes are unavailable on this platform".into())
    }
}

#[cfg(all(test, target_os = "linux"))]
use linux::argv_for_test;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::bash_background::HardKill;
    use std::{ffi::OsString, path::Path, time::Duration};

    #[test]
    fn task_scope_argv_and_backstop() {
        let id = "bash-0123456789abcdef";
        assert_eq!(
            scope_unit(id).unwrap(),
            "aft-task-bash-0123456789abcdef.scope"
        );
        assert!(scope_unit("../other").is_err());
        assert_eq!(
            backstop(
                HardKill::After(Duration::from_millis(1500)),
                Duration::from_secs(86400)
            ),
            Duration::from_millis(61500)
        );
        assert_eq!(
            backstop(HardKill::Default, Duration::from_secs(86400)),
            Duration::from_secs(86400)
        );
        let (_, argv) = argv_for_test(
            id,
            true,
            true,
            Duration::from_millis(61500),
            Some(64),
            Path::new("/bin/sh"),
            &[OsString::from("${X} $$")],
        );
        let args: Vec<_> = argv
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "--expand-environment=no",
                "--unit=aft-task-bash-0123456789abcdef",
                "--slice=aft.slice",
                "--property=RuntimeMaxSec=61500ms",
                "--property=TimeoutStopSec=2s",
                "--property=MemoryMax=67108864",
                "--property=MemorySwapMax=0",
                "--property=OOMPolicy=kill",
                "/bin/sh",
                "${X} $$"
            ]
        );
        let (_, legacy) = argv_for_test(
            id,
            false,
            false,
            Duration::from_secs(86400),
            None,
            Path::new("/bin/sh"),
            &[],
        );
        assert!(!legacy.contains(&OsString::from("--expand-environment=no")));
        assert!(!legacy
            .iter()
            .any(|s| s.to_string_lossy().contains("MemoryMax")));
        assert!(!legacy
            .iter()
            .any(|s| s.to_string_lossy().contains("OOMPolicy")));
        assert!(!args.iter().any(|s| s.contains("Delegate=")));
        let (_, legacy_cap) = argv_for_test(
            id,
            false,
            false,
            Duration::from_secs(86400),
            Some(64),
            Path::new("/bin/sh"),
            &[],
        );
        let legacy_cap: Vec<_> = legacy_cap
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            legacy_cap,
            [
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "--unit=aft-task-bash-0123456789abcdef",
                "--slice=aft.slice",
                "--property=RuntimeMaxSec=86400000ms",
                "--property=TimeoutStopSec=2s",
                "--property=MemoryMax=67108864",
                "--property=MemorySwapMax=0",
                "/bin/sh"
            ]
        );
        let (_, v253) = argv_for_test(
            id,
            false,
            true,
            Duration::from_secs(86400),
            Some(64),
            Path::new("/bin/sh"),
            &[],
        );
        assert!(v253.contains(&OsString::from("--property=OOMPolicy=kill")));
        assert!(!v253.contains(&OsString::from("--expand-environment=no")));
    }

    #[test]
    fn task_scope_membership_is_exact() {
        let id = "bash-0123456789abcdef";
        assert!(matches_membership(id, "0::/user.slice/user-1000.slice/user@1000.service/aft.slice/aft-task-bash-0123456789abcdef.scope\n"));
        for text in [
            "0::/aft.slice/aft-task-bash-0123456789abcdee.scope\n",
            "0::/aft.slice/aft-task-bash-0123456789abcdef.scope/child\n",
            "0::/../aft.slice/aft-task-bash-0123456789abcdef.scope\n",
            "1:memory:/aft.slice/aft-task-bash-0123456789abcdef.scope\n",
        ] {
            assert!(!matches_membership(id, text), "{text}");
        }
    }
}
