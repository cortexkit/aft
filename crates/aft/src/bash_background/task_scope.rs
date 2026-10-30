#[cfg(target_os = "linux")]
pub(crate) use linux::*;
#[cfg(not(target_os = "linux"))]
pub(crate) use unsupported::*;

#[cfg(target_os = "linux")]
mod linux {
    use super::super::HardKill;
    const TASK_SLICE: &str = "aft.slice";
    const TASK_UNIT_PREFIX: &str = "aft-task-";
    const TIMEOUT_MARGIN: Duration = Duration::from_secs(60);
    const SCOPE_OOM_POLICY_VERSION: u32 = 253;
    const EXPAND_ENVIRONMENT_VERSION: u32 = 254;
    const _: () = assert!(SCOPE_OOM_POLICY_VERSION < EXPAND_ENVIRONMENT_VERSION);
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
    pub(crate) struct ScopePolicy {
        pub max_runtime: Duration,
    }

    pub(crate) struct TaskScopes {
        daemon_mode: AtomicBool,
        max_runtime: Mutex<Duration>,
        handles: Mutex<HashMap<String, ScopeHandle>>,
    }

    impl Default for TaskScopes {
        fn default() -> Self {
            Self {
                daemon_mode: AtomicBool::new(false),
                max_runtime: Mutex::new(Duration::from_secs(
                    crate::config::DEFAULT_LINUX_SCOPE_MAX_RUNTIME_SECS,
                )),
                handles: Mutex::new(HashMap::new()),
            }
        }
    }

    impl TaskScopes {
        pub(crate) fn is_daemon(&self) -> bool {
            self.daemon_mode.load(Ordering::Acquire)
        }
        pub(crate) fn configure_runtime(&self, daemon_mode: bool, max_runtime: Duration) {
            *self.max_runtime.lock().unwrap_or_else(|e| e.into_inner()) = max_runtime;
            self.daemon_mode.store(daemon_mode, Ordering::Release);
        }

        pub(crate) fn max_runtime(&self) -> Duration {
            *self.max_runtime.lock().unwrap_or_else(|e| e.into_inner())
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
                if daemon_mode && enabled && native_launcher {
                    static LOGGED: std::sync::Once = std::sync::Once::new();
                    LOGGED.call_once(|| crate::slog_info!("bash.linux_scope: native sandbox launcher cannot reach the user manager; spawning without a scope"));
                }
                if !daemon_mode || !enabled || native_launcher {
                    return None;
                }
                Some(ScopePolicy {
                    max_runtime: saved_max_runtime.unwrap_or_else(|| {
                        *self.max_runtime.lock().unwrap_or_else(|e| e.into_inner())
                    }),
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

        pub(crate) fn kill(&self, task_id: &str, identity: Option<(u32, u64)>) -> io::Result<bool> {
            if let Some(handle) = self.resolve_handle(task_id, identity) {
                handle.kill()?;
                return Ok(true);
            }
            Ok(false)
        }

        pub(crate) fn resolve_handle(
            &self,
            task_id: &str,
            identity: Option<(u32, u64)>,
        ) -> Option<ScopeHandle> {
            if let Some(handle) = self.get(task_id) {
                return Some(handle);
            }
            if let Some((pid, start)) = identity {
                if let Ok(Some(handle)) = self.adopt(task_id, pid, start) {
                    return Some(handle);
                }
            } else {
                let _ = lookup(task_id);
            }
            log_unconfirmed_scope();
            None
        }

        pub(crate) fn adopt(
            &self,
            task_id: &str,
            pid: u32,
            start: u64,
        ) -> Result<Option<ScopeHandle>, String> {
            let handle = adopt_with(
                task_id,
                pid,
                start,
                lookup,
                |pid| {
                    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                        .map_err(|error| error.to_string())
                },
                |pid| {
                    crate::root_cache::process_start_time_ms(pid)
                        .ok_or_else(|| "task scope process identity is unavailable".into())
                },
            )?;
            if let Some(scope) = &handle {
                self.register(scope.clone());
            }
            Ok(handle)
        }

        pub(crate) fn cleanup_owned(
            &self,
            records: &[ScopeRecord],
            terminal_only: bool,
            mut recheck: impl FnMut(&str) -> bool,
        ) -> Result<usize, String> {
            let units = list_units()?;
            let mut stopped = 0;
            for unit in select_owned_units(&units, records, terminal_only) {
                let Some(id) = task_unit_id(&unit) else {
                    log_unconfirmed_scope();
                    continue;
                };
                match lookup(id) {
                    Ok(Some(scope)) if recheck(id) => {
                        scope.stop()?;
                        stopped += 1;
                    }
                    Err(_) => log_unconfirmed_scope(),
                    _ => {}
                }
            }
            Ok(stopped)
        }
    }

    pub(crate) fn log_unconfirmed_scope() {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            crate::slog_info!("bash task scope could not be confirmed; skipping scope operation")
        });
    }

    pub(crate) struct ScopeRecord {
        pub task_id: String,
        pub terminal: bool,
    }

    pub(crate) fn select_owned_units(
        units: &[String],
        records: &[ScopeRecord],
        terminal_only: bool,
    ) -> Vec<String> {
        let mut owned = std::collections::HashMap::new();
        for record in records {
            owned
                .entry(record.task_id.as_str())
                .and_modify(|value| *value = None)
                .or_insert(Some(record.terminal));
        }
        let mut seen = std::collections::HashSet::new();
        units
            .iter()
            .filter(|unit| {
                task_unit_id(unit)
                    .and_then(|id| owned.get(id))
                    .is_some_and(|terminal| {
                        terminal.is_some_and(|terminal| !terminal_only || terminal)
                    })
                    && seen.insert(unit.as_str())
            })
            .cloned()
            .collect()
    }

    fn adopt_with(
        task_id: &str,
        pid: u32,
        start: u64,
        lookup: impl FnOnce(&str) -> Result<Option<ScopeHandle>, String>,
        mut membership: impl FnMut(u32) -> Result<String, String>,
        mut identity: impl FnMut(u32) -> Result<u64, String>,
    ) -> Result<Option<ScopeHandle>, String> {
        let before = identity(pid).and_then(|actual| membership(pid).map(|text| (actual, text)));
        let Some(scope) = lookup(task_id)? else {
            return Ok(None);
        };
        let (actual, text) = before?;
        if actual != start
            || !matches_membership(task_id, &text)
            || membership_path(task_id, &text).as_deref() != Some(scope.cgroup())
        {
            return Err("task scope replay identity mismatch".into());
        }
        let actual = identity(pid)?;
        let text = membership(pid)?;
        if actual != start
            || !matches_membership(task_id, &text)
            || membership_path(task_id, &text).as_deref() != Some(scope.cgroup())
        {
            return Err("task scope replay identity changed".into());
        }
        Ok(Some(scope))
    }

    #[derive(Clone, Debug)]
    pub(crate) struct ScopeHandle {
        task_id: String,
        unit: String,
        cgroup: PathBuf,
    }

    impl ScopeHandle {
        pub(crate) fn cgroup(&self) -> &Path {
            &self.cgroup
        }
    }

    pub(crate) fn scope_unit(task_id: &str) -> io::Result<String> {
        super::super::persistence::validate_task_id(task_id)?;
        Ok(format!("{TASK_UNIT_PREFIX}{task_id}.scope"))
    }

    pub(crate) fn backstop(hard_kill: HardKill, ceiling: Duration) -> Duration {
        match hard_kill {
            HardKill::After(limit) => limit.saturating_add(TIMEOUT_MARGIN),
            HardKill::Default => ceiling,
        }
    }

    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        process::{Command, Output, Stdio},
        thread,
        time::Instant,
    };

    pub(crate) struct SystemdScopeLauncher {
        path: PathBuf,
        supports_expand_environment: bool,
        supports_scope_oom_policy: bool,
    }

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

    pub(crate) fn user_manager_command(executable: &Path) -> Command {
        let mut command = Command::new(executable);
        // Use only the session endpoint for the user manager, not daemon credentials.
        command.env_clear().env("PATH", "/usr/bin:/bin");
        for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
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
        // A probe must stop its descendants at its deadline.
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

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
            // Before systemd 254, argv is literal. More dollar characters change the shell input.
            let mut command = user_manager_command(launcher);
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
        let version = run(
            user_manager_command(&path).arg("--version"),
            Duration::from_secs(1),
        )
        .ok()
        .filter(|out| out.status.success());
        let supports_scope_oom_policy = version
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|text| parse_version(&text))
            .is_some_and(|version| version >= SCOPE_OOM_POLICY_VERSION);
        // Scope OOMPolicy starts at 253. Probe expansion independently.
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
            user_manager_command(&launcher).args(["--user", "--scope", "--quiet", "/bin/true"]),
            limit.min(Duration::from_secs(1)),
        )?)
        .map(|_| ())
    }

    impl TaskScopes {
        pub(crate) fn prepare(
            &self,
            policy: ScopePolicy,
            task_id: &str,
            hard_kill: super::super::HardKill,
            memory_mib: Option<u64>,
        ) -> Result<Option<ScopeLaunch>, String> {
            static UNIFIED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if !*UNIFIED.get_or_init(|| current_cgroup(std::process::id()).is_some()) {
                static LOGGED: std::sync::Once = std::sync::Once::new();
                LOGGED.call_once(|| crate::slog_info!("bash.linux_scope: unified cgroup hierarchy is not visible; spawning without a scope"));
                return Ok(None);
            }
            let Some(launcher) = select_systemd_scope_launcher() else {
                // If the manager probe cannot run a command, run the task without a scope.
                static LOGGED: std::sync::Once = std::sync::Once::new();
                LOGGED.call_once(|| {
                    crate::slog_info!("bash.linux_scope: systemd user manager unavailable; spawning without a scope");
                });
                return Ok(None);
            };
            ScopeLaunch::new(launcher, policy, task_id, hard_kill, memory_mib).map(Some)
        }
    }

    impl ScopeLaunch {
        fn new(
            launcher: SystemdScopeLauncher,
            policy: ScopePolicy,
            task_id: &str,
            hard_kill: HardKill,
            memory_mib: Option<u64>,
        ) -> Result<Self, String> {
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
            Ok(Self {
                launcher,
                task_id: task_id.into(),
                unit,
                runtime,
                memory_bytes,
            })
        }
        pub(crate) fn argv(
            &self,
            executable: &Path,
            args: &[OsString],
        ) -> (PathBuf, Vec<OsString>) {
            let mut wrapped = prefix(self.launcher.supports_expand_environment);
            wrapped.extend([
                format!(
                    "--unit={}",
                    self.unit
                        .strip_suffix(".scope")
                        .expect("validated scope unit")
                )
                .into(),
                format!("--slice={TASK_SLICE}").into(),
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
                let text = read_membership(pid, &self.task_id)?;
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

    fn cgroup_path(task_id: &str, text: &str) -> Option<PathBuf> {
        let unit = scope_unit(task_id).ok()?;
        let path = normalize_cgroup_path(text)?;
        if path.file_name()? != unit.as_str() || path.parent()?.file_name()? != TASK_SLICE {
            return None;
        }
        Some(path)
    }

    fn membership_path(task_id: &str, text: &str) -> Option<PathBuf> {
        cgroup_path(
            task_id,
            text.lines().find_map(|line| line.strip_prefix("0::"))?,
        )
    }

    pub(crate) fn unified_cgroup_path(text: &str) -> Option<PathBuf> {
        let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
        normalize_cgroup_path(relative)
    }

    fn normalize_cgroup_path(relative: &str) -> Option<PathBuf> {
        let path = Path::new(relative);
        if !path.is_absolute() || relative.split('/').any(|part| matches!(part, "." | "..")) {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(path.strip_prefix("/").ok()?))
    }

    pub(crate) fn current_cgroup(pid: u32) -> Option<PathBuf> {
        unified_cgroup_path(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)
    }

    fn task_unit_id(unit: &str) -> Option<&str> {
        let id = unit
            .strip_prefix(TASK_UNIT_PREFIX)?
            .strip_suffix(".scope")?;
        scope_unit(id).ok()?;
        Some(id)
    }

    fn read_membership(pid: u32, task_id: &str) -> Result<String, String> {
        #[cfg(test)]
        if let Some(result) =
            PLACEMENT_READER.with(|reader| reader.borrow().as_ref().map(|read| read(pid, task_id)))
        {
            return result;
        }
        #[cfg(not(test))]
        let _ = task_id;
        std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .map_err(|e| format!("task scope child exited before placement: {e}"))
    }

    #[cfg(test)]
    type PlacementReader = Box<dyn Fn(u32, &str) -> Result<String, String>>;

    #[cfg(test)]
    thread_local! {
        static PLACEMENT_READER: std::cell::RefCell<Option<PlacementReader>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    pub(crate) fn with_placement_reader_for_test<T>(
        reader: impl Fn(u32, &str) -> Result<String, String> + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Restore(Option<PlacementReader>);
        impl Drop for Restore {
            fn drop(&mut self) {
                PLACEMENT_READER.with(|reader| {
                    *reader.borrow_mut() = self.0.take();
                });
            }
        }
        let _restore = Restore(PLACEMENT_READER.with(|slot| slot.replace(Some(Box::new(reader)))));
        run()
    }

    pub(crate) fn matches_membership(task_id: &str, proc_cgroup: &str) -> bool {
        membership_path(task_id, proc_cgroup).is_some()
    }

    fn lookup_with(systemctl: &Path, task_id: &str) -> Result<Option<ScopeHandle>, String> {
        let unit = scope_unit(task_id).map_err(|e| e.to_string())?;
        let output = run(
            user_manager_command(systemctl).args([
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

    pub(crate) fn lookup(task_id: &str) -> Result<Option<ScopeHandle>, String> {
        lookup_with(Path::new("systemctl"), task_id)
    }

    fn list_with(systemctl: &Path) -> Result<Vec<String>, String> {
        let bytes = successful(run(
            user_manager_command(systemctl).args([
                "--user",
                "list-units",
                "--all",
                "--type=scope",
                "--no-legend",
                "--plain",
                "--no-pager",
                &format!("{TASK_UNIT_PREFIX}*.scope"),
            ]),
            Duration::from_secs(1),
        )?)?;
        let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
        let mut units = Vec::new();
        for line in text.lines() {
            let Some(unit) = line.split_whitespace().next() else {
                continue;
            };
            let Some(id) = task_unit_id(unit) else {
                log_unconfirmed_scope();
                continue;
            };
            match lookup_with(systemctl, id) {
                Ok(Some(_)) => units.push(unit.into()),
                Err(_) => log_unconfirmed_scope(),
                _ => {}
            }
        }
        units.sort();
        units.dedup();
        Ok(units)
    }

    pub(crate) fn list_units() -> Result<Vec<String>, String> {
        list_with(Path::new("systemctl"))
    }

    fn stop_with(systemctl: &Path, unit: &str) -> Result<(), String> {
        task_unit_id(unit).ok_or("invalid task scope unit")?;
        successful(run(
            user_manager_command(systemctl).args(["--user", "stop", unit]),
            Duration::from_secs(5),
        )?)
        .map(|_| ())
    }

    fn stop_unit(unit: &str) -> Result<(), String> {
        #[cfg(test)]
        if STOP_OBSERVER.with(|slot| {
            if let Some(observe) = slot.borrow().as_ref() {
                observe(unit);
                true
            } else {
                false
            }
        }) {
            return Ok(());
        }
        stop_with(Path::new("systemctl"), unit)
    }

    #[cfg(test)]
    type StopObserver = Box<dyn Fn(&str)>;
    #[cfg(test)]
    thread_local! { static STOP_OBSERVER: std::cell::RefCell<Option<StopObserver>> = const { std::cell::RefCell::new(None) }; }
    #[cfg(test)]
    pub(crate) fn with_stop_observer_for_test<T>(
        observer: impl Fn(&str) + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Restore(Option<StopObserver>);
        impl Drop for Restore {
            fn drop(&mut self) {
                STOP_OBSERVER.with(|slot| {
                    *slot.borrow_mut() = self.0.take();
                });
            }
        }
        let _restore = Restore(STOP_OBSERVER.with(|slot| slot.replace(Some(Box::new(observer)))));
        run()
    }

    impl ScopeHandle {
        pub(crate) fn kill(&self) -> io::Result<()> {
            #[cfg(test)]
            KILL_EVENTS.with(|slot| {
                if let Some(events) = slot.borrow().as_ref() {
                    events.lock().unwrap().push("scope-kill");
                }
            });
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
            #[cfg(test)]
            if let Some(error) = OOM_GROUP_ERROR.with(|slot| slot.borrow().clone()) {
                return Err(error);
            }
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
    thread_local! { static KILL_EVENTS: std::cell::RefCell<Option<std::sync::Arc<Mutex<Vec<&'static str>>>>> = const { std::cell::RefCell::new(None) }; }
    #[cfg(test)]
    pub(crate) fn with_kill_events_for_test<T>(
        events: std::sync::Arc<Mutex<Vec<&'static str>>>,
        run: impl FnOnce() -> T,
    ) -> T {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                KILL_EVENTS.with(|slot| slot.take());
            }
        }
        KILL_EVENTS.with(|slot| slot.replace(Some(events)));
        let _restore = Restore;
        run()
    }

    #[cfg(test)]
    thread_local! { static OOM_GROUP_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }
    #[cfg(test)]
    pub(crate) fn with_oom_group_error_for_test<T>(run: impl FnOnce() -> T) -> T {
        struct Restore;
        impl Drop for Restore {
            fn drop(&mut self) {
                OOM_GROUP_ERROR.with(|slot| slot.take());
            }
        }
        OOM_GROUP_ERROR
            .with(|slot| slot.replace(Some("task scope memory.oom.group write denied".into())));
        let _restore = Restore;
        run()
    }

    #[cfg(test)]
    pub(crate) fn handle_for_test(task_id: &str, cgroup: PathBuf) -> ScopeHandle {
        ScopeHandle {
            task_id: task_id.into(),
            unit: scope_unit(task_id).unwrap(),
            cgroup,
        }
    }

    #[cfg(test)]
    pub(crate) fn launch_for_test(id: &str) -> ScopeLaunch {
        ScopeLaunch::new(
            SystemdScopeLauncher {
                path: "/usr/bin/systemd-run".into(),
                supports_expand_environment: true,
                supports_scope_oom_policy: true,
            },
            ScopePolicy {
                max_runtime: Duration::from_secs(86400),
            },
            id,
            HardKill::Default,
            None,
        )
        .unwrap()
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
        fn task_scope_listing_skips_foreign_units() {
            let foreign = "aft-task-bash-0000000000000001.scope";
            let owned = "aft-task-bash-0000000000000002.scope";
            let (_dir, path) = fake(&format!("case \"$2\" in\nlist-units) printf '%s\\n' '{foreign}' '{owned}' ;;\nshow) for unit in \"$@\"; do :; done; case \"$unit\" in\n'{foreign}') printf '/app.slice/{foreign}\\n' ;;\n*) printf '/aft.slice/{owned}\\n' ;;\nesac ;;\nesac"));
            assert_eq!(list_with(&path).unwrap(), vec![owned.to_string()]);
        }
        #[test]
        fn task_scope_version_capabilities() {
            assert_eq!(parse_version("systemd 253 (253.1)\n"), Some(253));
            assert_eq!(parse_version("unknown 262"), None);
            assert_eq!(parse_version("systemd unknown"), None);
        }

        #[test]
        fn task_scope_adopt_rejects_other_scope() {
            let id = "bash-0123456789abcdef";
            let events = std::sync::Arc::new(Mutex::new(Vec::new()));
            let result = with_kill_events_for_test(events.clone(), || {
                adopt_with(
                    id,
                    42,
                    99,
                    |_| Ok(Some(handle_for_test(id, "/fake".into()))),
                    |_| Ok("0::/aft.slice/aft-task-bash-0000000000000000.scope\n".into()),
                    |_| Ok(99),
                )
            });
            assert!(result.unwrap_err().contains("identity mismatch"));
            assert!(events.lock().unwrap().is_empty());
        }

        #[test]
        fn task_scope_adopt_rejects_changed_start_time() {
            let id = "bash-0123456789abcdef";
            for changed_at in [0, 1] {
                let mut reads = 0;
                let events = std::sync::Arc::new(Mutex::new(Vec::new()));
                let result = with_kill_events_for_test(events.clone(), || {
                    adopt_with(
                        id,
                        42,
                        99,
                        |_| {
                            Ok(Some(handle_for_test(
                                id,
                                format!("/sys/fs/cgroup/aft.slice/aft-task-{id}.scope").into(),
                            )))
                        },
                        |_| Ok(format!("0::/aft.slice/aft-task-{id}.scope\n")),
                        |_| {
                            let value = if reads >= changed_at { 100 } else { 99 };
                            reads += 1;
                            Ok(value)
                        },
                    )
                });
                assert!(result.is_err(), "changed identity was adopted");
                assert!(events.lock().unwrap().is_empty());
            }
        }
        #[test]
        fn task_scope_runtime_properties_use_the_launch_policy() {
            let launcher = || SystemdScopeLauncher {
                path: "/usr/bin/systemd-run".into(),
                supports_expand_environment: true,
                supports_scope_oom_policy: true,
            };
            for (limit, expected) in [
                (
                    HardKill::After(Duration::from_millis(1500)),
                    "--property=RuntimeMaxSec=61500ms",
                ),
                (HardKill::Default, "--property=RuntimeMaxSec=7200000ms"),
            ] {
                let launch = ScopeLaunch::new(
                    launcher(),
                    ScopePolicy {
                        max_runtime: Duration::from_secs(7200),
                    },
                    "bash-0123456789abcdef",
                    limit,
                    None,
                )
                .unwrap();
                assert!(launch
                    .argv(Path::new("/bin/true"), &[])
                    .1
                    .contains(&OsString::from(expected)));
            }
        }
        #[test]
        fn task_scope_unified_visibility_decision() {
            assert!(unified_cgroup_path("0::/user.slice/test\n").is_some());
            for text in [
                "1:memory:/user.slice/test\n",
                "",
                "0::/../other\n",
                "0::relative\n",
            ] {
                assert!(unified_cgroup_path(text).is_none(), "{text}");
            }
        }
        #[test]
        fn lifecycle_scope_probe_deadline_falls_back() {
            let (_dir, path) = fake("sleep .1; exit 0");
            assert!(probe_systemd_scope(&path, Duration::ZERO).is_none());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod unsupported {
    #[derive(Default)]
    pub(crate) struct TaskScopes;
    pub(crate) enum ScopeLaunch {}
    #[cfg(target_os = "macos")]
    pub(crate) enum ScopePolicy {}
    #[cfg(target_os = "macos")]
    pub(crate) enum ScopeHandle {}
    impl TaskScopes {
        pub(crate) fn configure_runtime(&self, _daemon_mode: bool, _max: std::time::Duration) {}
        #[cfg(target_os = "macos")]
        pub(crate) fn max_runtime(&self) -> std::time::Duration {
            std::time::Duration::from_secs(crate::config::DEFAULT_LINUX_SCOPE_MAX_RUNTIME_SECS)
        }
        #[cfg(target_os = "macos")]
        pub(crate) fn policy(
            &self,
            _enabled: bool,
            _saved: Option<std::time::Duration>,
            _native: bool,
        ) -> Option<ScopePolicy> {
            None
        }
        #[cfg(target_os = "macos")]
        pub(crate) fn register(&self, _handle: ScopeHandle) {}
        #[cfg(target_os = "macos")]
        pub(crate) fn prepare(
            &self,
            _policy: ScopePolicy,
            _task_id: &str,
            _hard_kill: super::super::HardKill,
            _memory_mib: Option<u64>,
        ) -> Result<Option<ScopeLaunch>, String> {
            Ok(None)
        }
    }

    #[cfg(target_os = "macos")]
    impl ScopeLaunch {
        pub(crate) fn argv(
            &self,
            executable: &std::path::Path,
            args: &[std::ffi::OsString],
        ) -> (std::path::PathBuf, Vec<std::ffi::OsString>) {
            (executable.into(), args.into())
        }
        pub(crate) fn await_placement(
            &self,
            _pid: u32,
            _limit: std::time::Duration,
        ) -> Result<ScopeHandle, String> {
            Err("task scopes are unavailable on this platform".into())
        }
        pub(crate) fn stop(&self) -> Result<(), String> {
            Ok(())
        }
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
    fn task_scope_cleanup_never_claims_unknown_units() {
        let terminal = "aft-task-bash-0000000000000001.scope".to_string();
        let running = "aft-task-bash-0000000000000002.scope".to_string();
        let foreign = "aft-task-bash-0000000000000003.scope".to_string();
        let records = vec![
            ScopeRecord {
                task_id: "bash-0000000000000001".into(),
                terminal: true,
            },
            ScopeRecord {
                task_id: "bash-0000000000000002".into(),
                terminal: false,
            },
        ];
        let units = vec![terminal.clone(), running.clone(), foreign];
        assert_eq!(
            select_owned_units(&units, &records, true),
            vec![terminal.clone()]
        );
        assert_eq!(
            select_owned_units(&units, &records, false),
            vec![terminal, running]
        );
    }

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

    #[test]
    fn task_scope_daemon_gate() {
        let scopes = TaskScopes::default();
        assert_eq!(scopes.max_runtime(), Duration::from_secs(86400));
        scopes.configure_runtime(false, Duration::from_secs(86400));
        assert!(
            scopes.policy(true, None, false).is_none(),
            "standalone acquired a task scope"
        );
        assert!(scopes
            .policy(true, Some(Duration::from_secs(7200)), false)
            .is_none());
        scopes.configure_runtime(true, Duration::from_secs(86400));
        assert!(scopes.policy(true, None, false).is_some());
        assert_eq!(
            scopes
                .policy(true, Some(Duration::from_secs(7200)), false)
                .unwrap()
                .max_runtime,
            Duration::from_secs(7200)
        );
        assert!(
            scopes.policy(true, None, true).is_none(),
            "native launcher must stay unscoped"
        );
        assert!(scopes.policy(false, None, false).is_none());
    }
}
