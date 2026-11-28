//! Per-command aggregate memory accounting. Launchers keep shell commands
//! behind a gate until their memory-enforcement setup is complete.
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Matches the existing watchdog tick: two samples per second, only for capped tasks.
pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const LINUX_WATCHER_KIND: &str =
    "Linux process-group RSS watcher; kernel memory controller unavailable";
const LINUX_CGROUP_KIND: &str = "Linux cgroup v2 memory.max";
#[cfg(unix)]
const PID_BUDGET: usize = 4096;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const UNKNOWN_SAMPLE_LIMIT: u8 = 3;

#[cfg(all(test, target_os = "linux"))]
#[derive(Clone)]
struct AttachFault {
    path: std::path::PathBuf,
    errno: i32,
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static THREAD_ATTACH_FAULT: std::cell::RefCell<Option<AttachFault>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, target_os = "linux"))]
fn task_attach_faults() -> &'static std::sync::Mutex<std::collections::HashMap<String, AttachFault>>
{
    static FAULTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, AttachFault>>,
    > = std::sync::OnceLock::new();
    FAULTS.get_or_init(Default::default)
}

#[cfg(all(test, target_os = "linux"))]
pub(super) struct AttachFaultGuard {
    task: Option<String>,
    previous: Option<AttachFault>,
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(all(test, target_os = "linux"))]
impl AttachFaultGuard {
    pub(super) fn for_thread(path: std::path::PathBuf, errno: i32) -> Self {
        std::fs::create_dir(&path).unwrap();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fault = AttachFault {
            path,
            errno,
            attempts: attempts.clone(),
        };
        let previous = THREAD_ATTACH_FAULT.with(|slot| slot.replace(Some(fault)));
        Self {
            task: None,
            previous,
            attempts,
            _thread_bound: std::marker::PhantomData,
        }
    }

    pub(super) fn for_task(task: String, path: std::path::PathBuf, errno: i32) -> Self {
        std::fs::create_dir(&path).unwrap();
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fault = AttachFault {
            path,
            errno,
            attempts: attempts.clone(),
        };
        assert!(task_attach_faults()
            .lock()
            .unwrap()
            .insert(task.clone(), fault)
            .is_none());
        Self {
            task: Some(task),
            previous: None,
            attempts,
            _thread_bound: std::marker::PhantomData,
        }
    }

    pub(super) fn attempts(&self) -> usize {
        self.attempts.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for AttachFaultGuard {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task_attach_faults().lock().unwrap().remove(task);
        } else {
            THREAD_ATTACH_FAULT.with(|slot| *slot.borrow_mut() = self.previous.take());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
fn attach_fault_candidate(task: &str) -> Option<std::path::PathBuf> {
    task_attach_faults()
        .lock()
        .unwrap()
        .get(task)
        .cloned()
        .or_else(|| THREAD_ATTACH_FAULT.with(|slot| slot.borrow().clone()))
        .map(|fault| fault.path)
}

#[cfg(all(test, target_os = "linux"))]
fn attach_fault_error(path: &std::path::Path) -> Option<std::io::Error> {
    task_attach_faults()
        .lock()
        .unwrap()
        .values()
        .find(|fault| fault.path == path)
        .cloned()
        .or_else(|| {
            THREAD_ATTACH_FAULT.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .filter(|fault| fault.path == path)
                    .cloned()
            })
        })
        .map(|fault| {
            fault
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::io::Error::from_raw_os_error(fault.errno)
        })
}

#[cfg(all(test, target_os = "linux"))]
pub(super) fn sampling_available_for_test() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let Ok(mut child) = Command::new("/bin/sh")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-c", "read -r ready"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let available = loop {
            if group_bytes(child.id() as i32).is_some_and(|bytes| bytes > 0) {
                break true;
            }
            if Instant::now() >= deadline || child.try_wait().ok().flatten().is_some() {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        linux::stop_bootstrap(child);
        available
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPolicy {
    pub mib: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<std::path::PathBuf>,
}

impl MemoryPolicy {
    #[cfg(target_os = "linux")]
    pub(crate) const TASK_SCOPE_MEMORY_KIND: &str = "Linux task scope memory.max";

    #[cfg(target_os = "linux")]
    pub(super) fn systemd_cgroup(&self) -> Option<&std::path::Path> {
        self.cgroup.as_deref().filter(|path| {
            // New memory policies save the payload cgroup. Policies saved by
            // older AFT versions use the systemd scope for accounting instead.
            self.kind == LINUX_CGROUP_KIND
                && (path.ends_with("payload") || path.extension().is_some_and(|ext| ext == "scope"))
        })
    }

    pub fn note(&self) -> String {
        format!("Memory cap: {} MiB ({}).", self.mib, self.kind)
    }

    pub fn exceeded_reason(&self) -> String {
        format!(
            "Command stopped: exceeded the {} MiB memory cap ({}).",
            self.mib, self.kind
        )
    }

    fn accounting_failure_reason(&self) -> String {
        format!(
            "Command stopped: memory-cap accounting could not complete its bounded process scan ({}; cap {} MiB).",
            self.kind, self.mib
        )
    }
}

#[cfg(target_os = "linux")]
pub(super) struct ScopeHolder(Option<std::process::Child>);

#[cfg(target_os = "linux")]
impl Drop for ScopeHolder {
    fn drop(&mut self) {
        if let Some(helper) = self.0.take() {
            linux::stop_bootstrap(helper);
        }
    }
}

pub(crate) struct Monitor {
    pub policy: MemoryPolicy,
    last_sample: Option<Instant>,
    exceeded: bool,
    accounting_failed: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    unknown_samples: u8,
    #[cfg(target_os = "linux")]
    holder: Option<ScopeHolder>,
    #[cfg(target_os = "linux")]
    exit_oom_sampled: bool,
    #[cfg(target_os = "linux")]
    retired_cgroup: Option<std::path::PathBuf>,
    #[cfg(target_os = "linux")]
    borrowed_scope: Option<super::task_scope::ScopeHandle>,
    #[cfg(windows)]
    job: windows::Job,
}

impl Monitor {
    pub fn prepare(mib: u64, task_id: &str) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let (cgroup, holder) = linux::create(mib, task_id);
        #[cfg(not(target_os = "linux"))]
        let cgroup = None;
        #[cfg(windows)]
        let job = windows::Job::new(mib, task_id)?;
        let kind = if cgroup.is_some() {
            LINUX_CGROUP_KIND
        } else if cfg!(target_os = "linux") {
            LINUX_WATCHER_KIND
        } else if cfg!(target_os = "macos") {
            "macOS process-group physical-footprint watcher"
        } else {
            "Windows job-object memory limit"
        };
        let _ = task_id;
        Ok(Self {
            policy: MemoryPolicy {
                mib,
                kind: kind.into(),
                cgroup,
            },
            last_sample: None,
            exceeded: false,
            accounting_failed: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            unknown_samples: 0,
            #[cfg(target_os = "linux")]
            holder,
            #[cfg(target_os = "linux")]
            exit_oom_sampled: false,
            #[cfg(target_os = "linux")]
            retired_cgroup: None,
            #[cfg(target_os = "linux")]
            borrowed_scope: None,
            #[cfg(windows)]
            job,
        })
    }

    pub fn attach(&mut self, pid: u32) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if let Some(path) = self
            .policy
            .cgroup
            .as_ref()
            .filter(|_| self.borrowed_scope.is_none())
        {
            if linux::attach(path, pid).is_err() {
                self.use_watcher_fallback();
            }
        }
        #[cfg(windows)]
        self.job.attach(pid)?;
        let _ = pid;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn use_watcher_fallback(&mut self) {
        let failed = self.policy.cgroup.take();
        self.policy.kind = LINUX_WATCHER_KIND.into();
        drop(self.take_holder());
        if let Some(path) = failed {
            if std::fs::remove_dir(&path).is_err() {
                // If immediate removal fails, retry it when this monitor is dropped.
                self.retired_cgroup = Some(path);
            }
        }
    }

    pub fn recover(policy: MemoryPolicy, task_id: &str) -> Result<Option<Self>, String> {
        #[cfg(target_os = "linux")]
        if policy.kind == MemoryPolicy::TASK_SCOPE_MEMORY_KIND {
            return match super::task_scope::lookup_local(task_id, policy.cgroup.as_deref())? {
                super::task_scope::LocalScopeLookup::Present(scope) => {
                    Ok(Some(Self::recover_task_scope(policy.mib, &scope)))
                }
                super::task_scope::LocalScopeLookup::Absent => Ok(None),
                super::task_scope::LocalScopeLookup::Unknown => Err(format!(
                    "task scope aft-task-{task_id}.scope location is unknown"
                )),
            };
        }
        #[cfg(windows)]
        let job = windows::Job::new(policy.mib, task_id)?;
        let _ = task_id;
        Ok(Some(Self {
            policy,
            last_sample: None,
            exceeded: false,
            accounting_failed: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            unknown_samples: 0,
            #[cfg(target_os = "linux")]
            holder: None,
            #[cfg(target_os = "linux")]
            exit_oom_sampled: false,
            #[cfg(target_os = "linux")]
            retired_cgroup: None,
            #[cfg(target_os = "linux")]
            borrowed_scope: None,
            #[cfg(windows)]
            job,
        }))
    }

    pub fn exceeded(&mut self, pgid: i32) -> bool {
        if self.exceeded {
            return true;
        }
        if self.policy.cgroup.is_none()
            && !cfg!(windows)
            && self
                .last_sample
                .is_some_and(|last| last.elapsed() < SAMPLE_INTERVAL)
        {
            return false;
        }
        self.last_sample = Some(Instant::now());
        #[cfg(target_os = "linux")]
        let over = match &self.policy.cgroup {
            Some(path) => linux::oom(path).unwrap_or(false),
            None => self.fallback_sample_exceeded(group_bytes(pgid)),
        };
        #[cfg(target_os = "macos")]
        let over = self.fallback_sample_exceeded(group_bytes(pgid));
        #[cfg(windows)]
        let over = self.job.exceeded();
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let over = false;
        let _ = pgid;
        self.exceeded = over;
        over
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn fallback_sample_exceeded(&mut self, bytes: Option<u64>) -> bool {
        if let Some(bytes) = bytes {
            self.unknown_samples = 0;
            bytes > self.policy.mib * 1024 * 1024
        } else {
            // A transient scan failure is not evidence that the command exceeded the cap.
            self.unknown_samples = self.unknown_samples.saturating_add(1);
            self.accounting_failed = self.unknown_samples >= UNKNOWN_SAMPLE_LIMIT;
            self.accounting_failed
        }
    }

    pub(super) fn exceeded_at_exit(&mut self, pgid: i32, task_id: &str, sigkill: bool) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(path) = self.policy.systemd_cgroup() {
            if !self.exceeded && !self.exit_oom_sampled {
                match linux::oom(path) {
                    Ok(over) => self.exceeded = over,
                    Err(error) if sigkill => {
                        crate::slog_warn!(
                            "could not read memory-cap accounting for {task_id} after exit 137 at {}: {error}",
                            path.join("memory.events").display()
                        );
                    }
                    Err(_) => {}
                }
            }
            self.exit_oom_sampled = true;
            return self.exceeded;
        }
        let _ = (task_id, sigkill);
        self.exceeded(pgid)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn take_holder(&mut self) -> Option<ScopeHolder> {
        self.holder.take()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn holder_release_pending(&self, persisted: bool) -> bool {
        self.holder.is_some() && (!self.exit_oom_sampled || !persisted)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn set_holder_for_test(&mut self, helper: std::process::Child) {
        self.holder = Some(ScopeHolder(Some(helper)));
    }

    pub fn terminate(&self) {
        #[cfg(target_os = "linux")]
        if let Some(scope) = &self.borrowed_scope {
            let _ = scope.kill();
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.policy.cgroup {
            let _ = linux::write_control(&path.join("cgroup.kill"), "1");
        }
        #[cfg(windows)]
        self.job.terminate();
    }

    pub fn reason(&self) -> String {
        if self.accounting_failed {
            self.policy.accounting_failure_reason()
        } else {
            self.policy.exceeded_reason()
        }
    }

    pub(crate) fn is_task_scope(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.borrowed_scope.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn task_scope(&self) -> Option<&super::task_scope::ScopeHandle> {
        self.borrowed_scope.as_ref()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn from_task_scope(
        mib: u64,
        scope: &super::task_scope::ScopeHandle,
    ) -> Result<Self, String> {
        scope.ensure_oom_group()?;
        Ok(Self::recover_task_scope(mib, scope))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn recover_task_scope(mib: u64, scope: &super::task_scope::ScopeHandle) -> Self {
        Self {
            policy: MemoryPolicy {
                mib,
                kind: MemoryPolicy::TASK_SCOPE_MEMORY_KIND.into(),
                cgroup: Some(scope.cgroup().to_owned()),
            },
            last_sample: None,
            exceeded: false,
            accounting_failed: false,
            unknown_samples: 0,
            holder: None,
            exit_oom_sampled: false,
            retired_cgroup: None,
            borrowed_scope: Some(scope.clone()),
        }
    }

    #[cfg(all(test, unix))]
    pub(super) fn force_exceeded_for_test(&mut self) {
        self.exceeded = true;
    }

    #[cfg(windows)]
    pub fn outcome_note(&self) -> Option<&'static str> {
        if self.exceeded {
            None
        } else {
            self.job.outcome_note()
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        drop(self.take_holder());
        #[cfg(target_os = "linux")]
        // Let systemd remove its cgroups. During restart recovery, the
        // monitor can be dropped before AFT saves the memory-cap reason.
        if let Some(path) = self
            .policy
            .cgroup
            .as_ref()
            .filter(|_| self.borrowed_scope.is_none() && self.policy.systemd_cgroup().is_none())
        {
            let _ = std::fs::remove_dir(path);
        }
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.retired_cgroup {
            let _ = std::fs::remove_dir(path);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};

    pub(super) fn write_control(path: &Path, value: &str) -> std::io::Result<()> {
        use std::io::Write;
        // Controller files must already exist; an ordinary directory is not a cgroup.
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)?
            .write_all(value.as_bytes())
    }

    fn helper_command(executable: &Path) -> std::process::Command {
        super::super::task_scope::user_manager_command(executable)
    }

    fn systemd_run_path_with(exists: impl FnMut(&Path) -> bool) -> Option<&'static Path> {
        super::super::task_scope::systemd_run_path_with(exists)
    }

    fn current(pid: u32) -> Option<PathBuf> {
        super::super::task_scope::current_cgroup(pid)
    }

    fn delegated(parent: &Path, mib: u64, task_id: &str) -> Option<PathBuf> {
        let path = parent.join(format!("aft-memory-{task_id}"));
        std::fs::create_dir(&path).ok()?;
        let result = (|| {
            write_control(&path.join("memory.max"), &(mib * 1024 * 1024).to_string()).ok()?;
            write_control(&path.join("memory.oom.group"), "1").ok()?;
            // Swap must not turn the physical-memory ceiling into an unbounded allocation budget.
            write_control(&path.join("memory.swap.max"), "0").ok()?;
            Some(path.clone())
        })();
        if result.is_none() {
            let _ = std::fs::remove_dir(&path);
        }
        result
    }

    pub(super) fn create(mib: u64, task_id: &str) -> (Option<PathBuf>, Option<super::ScopeHolder>) {
        #[cfg(test)]
        if let Some(path) = super::attach_fault_candidate(task_id) {
            return (Some(path), None);
        }
        let Some(parent) = current(std::process::id()) else {
            return (None, None);
        };
        if let Some(path) = delegated(&parent, mib, task_id) {
            return (Some(path), None);
        }
        // Ask the user manager for delegation when the current cgroup is not writable.
        // The bootstrap waits on stdin, so no uncontained payload can start.
        use std::process::Stdio;
        let Some(executable) = systemd_run_path_with(|path| path.is_file()) else {
            return (None, None);
        };
        let Ok(mut helper) = helper_command(executable)
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--property=Delegate=yes",
            ])
            .args(["/bin/sh", "-c", "read -r ready"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return (None, None);
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            let child_pid =
                std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", helper.id()))
                    .ok()
                    .and_then(|children| children.split_whitespace().next()?.parse::<u32>().ok());
            let placement = [Some(helper.id()), child_pid]
                .into_iter()
                .flatten()
                .find_map(|pid| {
                    current(pid)
                        .filter(|scope| scope != &parent)
                        .map(|scope| (pid, scope))
                });
            if let Some((pid, scope)) = placement {
                if let Ok(payload) = setup_systemd_payload(
                    &scope,
                    pid,
                    mib,
                    |path| std::fs::create_dir(path),
                    write_control,
                ) {
                    return (Some(payload), Some(super::ScopeHolder(Some(helper))));
                }
                break;
            }
            if helper.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        stop_bootstrap(helper);
        (None, None)
    }

    pub(in crate::bash_background) fn setup_systemd_payload(
        scope: &Path,
        bootstrap_pid: u32,
        mib: u64,
        mkdir: impl FnMut(&Path) -> std::io::Result<()>,
        write: impl FnMut(&Path, &str) -> std::io::Result<()>,
    ) -> std::io::Result<PathBuf> {
        let (mut mkdir, mut write) = (mkdir, write);
        let holder = scope.join("holder");
        mkdir(&holder)?;
        // Cgroup v2 only lets the scope delegate memory once its last process
        // has moved out. The holder keeps the scope populated without sharing the cap.
        write(&holder.join("cgroup.procs"), &bootstrap_pid.to_string())?;
        write(&scope.join("cgroup.subtree_control"), "+memory")?;
        let payload = scope.join("payload");
        mkdir(&payload)?;
        write(
            &payload.join("memory.max"),
            &(mib * 1024 * 1024).to_string(),
        )?;
        write(&payload.join("memory.oom.group"), "1")?;
        // Keep swap from bypassing the physical-memory ceiling.
        write(&payload.join("memory.swap.max"), "0")?;
        Ok(payload)
    }

    pub(super) fn stop_bootstrap(mut helper: std::process::Child) {
        // Close stdin before waiting, so the shell blocked on read can exit normally.
        drop(helper.stdin.take());
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            match helper.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(None) => break,
            }
        }
        let _ = helper.kill();
        let _ = helper.wait();
    }

    pub(super) fn oom(path: &Path) -> std::io::Result<bool> {
        std::fs::read_to_string(path.join("memory.events")).map(|text| {
            text.lines().any(|line| {
                line.split_once(' ').is_some_and(|(key, value)| {
                    matches!(key, "oom" | "oom_kill" | "oom_group_kill")
                        && value.parse::<u64>().unwrap_or(0) > 0
                })
            })
        })
    }

    pub(super) fn attach(path: &Path, pid: u32) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(error) = super::attach_fault_error(path) {
            return Err(error);
        }
        write_control(&path.join("cgroup.procs"), &pid.to_string())
    }

    #[cfg(test)]
    mod helper_env_tests {
        use super::*;

        const DRIVER: &str = "bash_background::memory_limit::linux::helper_env_tests::memory_limit_helper_environment_probe";

        #[test]
        fn memory_limit_delegation_setup_requires_existing_controller_files() {
            let dir = tempfile::tempdir().unwrap();
            assert!(delegated(dir.path(), 64, "missing-controller").is_none());
            assert!(!dir.path().join("aft-memory-missing-controller").exists());
        }

        #[test]
        fn memory_limit_attach_failure_closes_and_reaps_bootstrap() {
            use std::process::{Command, Stdio};
            let dir = tempfile::tempdir().unwrap();
            let failed = dir.path().join("denied-cgroup");
            let fault = super::super::AttachFaultGuard::for_thread(failed.clone(), libc::EPERM);
            let mut monitor = super::super::Monitor::prepare(64, "cleanup-probe").unwrap();
            assert_eq!(monitor.policy.cgroup.as_ref(), Some(&failed));
            let marker = dir.path().join("bootstrap-released");
            let helper = Command::new("/bin/sh")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .args([
                    "-c",
                    &format!("read -r ready; printf released > '{}'", marker.display()),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let helper_pid = helper.id();
            monitor.holder = Some(super::super::ScopeHolder(Some(helper)));
            monitor.attach(12345).unwrap();
            assert_eq!(fault.attempts(), 1);
            assert!(monitor.holder.is_none());
            assert!(monitor.policy.cgroup.is_none());
            assert_eq!(monitor.policy.mib, 64);
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "released");
            assert!(!failed.exists());
            assert_eq!(unsafe { libc::kill(helper_pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }

        #[test]
        fn task_scope_memory_monitor_borrows_and_detects_oom() {
            for initial in ["1", "0"] {
                let dir = tempfile::tempdir().unwrap();
                let group = dir.path().join("memory.oom.group");
                std::fs::write(&group, initial).unwrap();
                std::fs::write(dir.path().join("memory.events"), "oom_kill 1\n").unwrap();
                let scope = crate::bash_background::task_scope::handle_for_test(
                    "bash-0123456789abcdef",
                    dir.path().into(),
                );
                let mut monitor = super::super::Monitor::from_task_scope(64, &scope).unwrap();
                monitor.attach(12345).unwrap();
                assert!(
                    !dir.path().join("cgroup.procs").exists(),
                    "borrowed scope received a PID write"
                );
                assert_eq!(std::fs::read_to_string(&group).unwrap(), "1");
                assert!(monitor.exceeded(0));
                drop(monitor);
                assert!(dir.path().exists(), "borrowed cgroup was removed");
            }
        }

        #[test]
        fn task_scope_memory_monitor_refuses_denied_group_write() {
            let dir = tempfile::tempdir().unwrap();
            let group = dir.path().join("memory.oom.group");
            std::fs::create_dir(&group).unwrap();
            let scope = crate::bash_background::task_scope::handle_for_test(
                "bash-0123456789abcdef",
                dir.path().into(),
            );
            assert!(super::super::Monitor::from_task_scope(64, &scope).is_err());
            assert!(group.is_dir());
        }

        #[test]
        fn memory_limit_helper_binary_uses_fixed_system_paths() {
            let mut visited = Vec::new();
            let path = systemd_run_path_with(|path| {
                visited.push(path.to_path_buf());
                path == Path::new("/bin/systemd-run")
            });
            assert_eq!(path, Some(Path::new("/bin/systemd-run")));
            assert_eq!(
                visited,
                [
                    PathBuf::from("/usr/bin/systemd-run"),
                    PathBuf::from("/bin/systemd-run")
                ]
            );
            assert!(systemd_run_path_with(|_| false).is_none());
        }

        #[test]
        fn memory_limit_helper_environment_probe() {
            if std::env::var("AFT_TEST_MEMORY_HELPER_PARENT").as_deref() != Ok("fixture") {
                return;
            }
            let output = helper_command(Path::new("/usr/bin/env")).output().unwrap();
            assert!(output.status.success());
            let vars: std::collections::BTreeMap<String, String> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| {
                    let (key, value) = line.split_once('=').unwrap();
                    (key.into(), value.into())
                })
                .collect();
            println!(
                "HELPER_ENV_SNAPSHOT: {}",
                serde_json::to_string(&vars).unwrap()
            );
        }

        #[test]
        fn memory_limit_helper_environment_excludes_daemon_launch_material() {
            // A separate fixture parent avoids changing the test runner's environment.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .env_clear()
                .envs([
                    ("PATH", "/fixture/unsafe-path"),
                    ("HOME", "/fixture/unsafe-home"),
                    ("XDG_RUNTIME_DIR", "/fixture/runtime"),
                    ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/fixture/bus"),
                    ("AFT_TEST_MEMORY_HELPER_PARENT", "fixture"),
                    ("AFT_SUBC_LAUNCH_NONCE", "fixture-nonce"),
                    ("SUBC_MODULE_ID", "fixture-module-id"),
                    ("SUBC_TOKEN", "fixture-subc-token"),
                    ("CK_TEST_SECRET", "fixture-ck-secret"),
                    ("AWS_SESSION_TOKEN", "fixture-unrelated-secret"),
                ])
                .args(["--exact", DRIVER, "--nocapture"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            let json = stdout
                .lines()
                .find_map(|line| {
                    line.split_once("HELPER_ENV_SNAPSHOT: ")
                        .map(|(_, json)| json)
                })
                .expect("fixture helper environment");
            let vars: std::collections::BTreeMap<String, String> =
                serde_json::from_str(json).unwrap();
            let forbidden: Vec<_> = vars
                .keys()
                .filter(|key| {
                    key.starts_with("AFT_") || key.starts_with("SUBC_") || key.starts_with("CK_")
                })
                .collect();
            assert!(
                forbidden.is_empty(),
                "helper inherited daemon launch names: {forbidden:?}"
            );
            assert_eq!(
                vars,
                std::collections::BTreeMap::from([
                    ("PATH".into(), "/usr/bin:/bin".into()),
                    ("XDG_RUNTIME_DIR".into(), "/fixture/runtime".into()),
                    (
                        "DBUS_SESSION_BUS_ADDRESS".into(),
                        "unix:path=/fixture/bus".into()
                    ),
                ])
            );
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(super) use linux::setup_systemd_payload;

#[cfg(target_os = "linux")]
fn group_bytes(pgid: i32) -> Option<u64> {
    if pgid == 0
        || (pgid > 0
            && unsafe { libc::kill(-pgid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
    {
        return Some(0);
    }
    linux_group_bytes(std::path::Path::new("/proc"), pgid)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_gone_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT))
}

#[cfg(target_os = "linux")]
fn linux_group_bytes(proc_dir: &std::path::Path, pgid: i32) -> Option<u64> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    let mut sum = 0_u64;
    // Bound all work, not just matching members. Reaching the budget fails closed.
    for (index, entry) in std::fs::read_dir(proc_dir).ok()?.enumerate() {
        if index >= PID_BUDGET {
            return None;
        }
        let entry = entry.ok()?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let stat = match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if process_gone_error(&error) => continue,
            Err(_) => return None,
        };
        // The command name may contain non-UTF-8 bytes or parentheses; parse only the fields after it.
        let end_of_comm = stat.windows(2).rposition(|bytes| bytes == b") ")?;
        let mut fields = std::str::from_utf8(&stat[end_of_comm + 2..])
            .ok()?
            .split_ascii_whitespace();
        if fields.nth(2)?.parse::<i32>().ok()? != pgid {
            continue;
        }
        let rss = fields.nth(18)?.parse::<u64>().ok()?;
        sum = sum.saturating_add(rss.saturating_mul(page_size as u64));
    }
    Some(sum)
}

#[cfg(target_os = "macos")]
fn group_pids(pgid: i32) -> Option<Vec<u32>> {
    use std::ffi::{c_int, c_void};
    #[link(name = "proc")]
    extern "C" {
        fn proc_listpgrppids(group: u32, buffer: *mut c_void, size: c_int) -> c_int;
    }
    if pgid == 0 {
        return Some(Vec::new());
    }
    group_pids_with(|buffer| {
        let (pointer, size) = match buffer {
            Some(buffer) => (
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(buffer) as c_int,
            ),
            None => (std::ptr::null_mut(), 0),
        };
        macos_group_pid_count(|| unsafe { proc_listpgrppids(pgid as u32, pointer, size) })
    })
}

#[cfg(target_os = "macos")]
fn macos_group_pid_count(read: impl FnOnce() -> i32) -> i32 {
    // libproc can return zero for both an empty group and a syscall error.
    let errno = unsafe { libc::__error() };
    unsafe { *errno = 0 };
    let count = read();
    if count == 0 && unsafe { *errno } != 0 {
        -1
    } else {
        count
    }
}

#[cfg(target_os = "macos")]
fn group_pids_with(mut read: impl FnMut(Option<&mut [u32]>) -> i32) -> Option<Vec<u32>> {
    let estimate = read(None);
    if estimate == 0 {
        return Some(Vec::new());
    }
    if estimate < 0 {
        return missing_group_after_error();
    }
    let mut capacity = (estimate as usize).clamp(16, PID_BUDGET);
    for _ in 0..3 {
        let mut pids = vec![0_u32; capacity];
        let count = read(Some(&mut pids));
        if count < 0 {
            return missing_group_after_error();
        }
        if (count as usize) < capacity {
            pids.resize(count as usize, 0);
            pids.retain(|pid| *pid != 0);
            return Some(pids);
        }
        if capacity == PID_BUDGET {
            return None;
        }
        capacity = capacity.saturating_mul(2).min(PID_BUDGET);
    }
    None
}

#[cfg(target_os = "macos")]
fn missing_group_after_error() -> Option<Vec<u32>> {
    process_gone_error(&std::io::Error::last_os_error()).then(Vec::new)
}

#[cfg(target_os = "macos")]
fn group_bytes(pgid: i32) -> Option<u64> {
    let pids = group_pids(pgid)?;
    let mut sum = 0_u64;
    for pid in pids {
        if pid == 0 {
            continue;
        }
        let mut usage: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::proc_pid_rusage(
                pid as i32,
                libc::RUSAGE_INFO_V2,
                (&mut usage as *mut libc::rusage_info_v2).cast(),
            )
        } == 0
        {
            sum = sum.saturating_add(usage.ri_phys_footprint);
        } else if !process_gone_error(&std::io::Error::last_os_error())
            && !(unsafe { libc::kill(pid as i32, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
        {
            return None;
        }
    }
    Some(sum)
}

#[cfg(windows)]
#[path = "memory_limit_windows.rs"]
mod windows;

#[cfg(all(test, windows))]
pub(super) use windows::test_trace;

#[cfg(test)]
mod agent_text_tests {
    use super::MemoryPolicy;

    #[test]
    fn memory_limit_note_matches_approved_text() {
        let policy = MemoryPolicy {
            mib: 64,
            kind: "test enforcement".into(),
            cgroup: None,
        };
        assert_eq!(policy.note(), "Memory cap: 64 MiB (test enforcement).");
    }

    #[test]
    fn memory_limit_accounting_failure_matches_approved_text() {
        let policy = MemoryPolicy {
            mib: 64,
            kind: "test enforcement".into(),
            cgroup: None,
        };
        assert_eq!(policy.accounting_failure_reason(), "Command stopped: memory-cap accounting could not complete its bounded process scan (test enforcement; cap 64 MiB).");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod fallback_tests {
    use super::*;

    #[test]
    fn memory_limit_fallback_rejects_incomplete_rss_but_skips_gone_and_outside_processes() {
        let proc = tempfile::tempdir().unwrap();
        let stat = |pid: u32, contents: &[u8]| {
            let path = proc.path().join(pid.to_string());
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("stat"), contents).unwrap();
        };
        let mut member = b"1 (nested ) name \xff)".to_vec();
        member.extend(format!(" S 1 42{} 5", " 0".repeat(18)).bytes());
        stat(1, &member);
        std::fs::create_dir(proc.path().join("2")).unwrap(); // Gone before stat was read.
        stat(3, b"3 (other group) S 1 41");
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        assert_eq!(linux_group_bytes(proc.path(), 42), Some(5 * page_size));

        stat(4, b"4 (missing RSS) S 1 42");
        assert_eq!(linux_group_bytes(proc.path(), 42), None);
        stat(4, b"4 (unknown group) S 1 invalid");
        assert_eq!(linux_group_bytes(proc.path(), 42), None);
        let broken_stat = proc.path().join("4/stat");
        std::fs::remove_file(&broken_stat).unwrap();
        std::fs::create_dir(&broken_stat).unwrap();
        assert_eq!(linux_group_bytes(proc.path(), 42), None);
    }

    #[test]
    fn memory_limit_fallback_requires_three_consecutive_unknown_samples() {
        let policy = MemoryPolicy {
            mib: 64,
            kind: "test enforcement".into(),
            cgroup: None,
        };
        let mut monitor = Monitor::recover(policy, "test").unwrap().unwrap();
        assert!(!monitor.fallback_sample_exceeded(None));
        assert!(!monitor.fallback_sample_exceeded(Some(0)));
        assert!(!monitor.fallback_sample_exceeded(None));
        assert!(!monitor.fallback_sample_exceeded(None));
        assert!(monitor.fallback_sample_exceeded(None));
        assert_eq!(monitor.reason(), monitor.policy.accounting_failure_reason());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn memory_limit_empty_group_has_zero_footprint() {
        assert_eq!(group_pids(0), Some(Vec::new()));
        assert_eq!(group_bytes(0), Some(0));
    }

    #[test]
    fn memory_limit_zero_sizing_query_is_an_empty_group() {
        assert_eq!(
            group_pids_with(|_| macos_group_pid_count(|| 0)),
            Some(Vec::new())
        );
    }

    #[test]
    fn memory_limit_zero_with_errno_is_not_an_empty_group() {
        let group = group_pids_with(|_| {
            macos_group_pid_count(|| {
                unsafe { *libc::__error() = libc::EACCES };
                0
            })
        });
        assert_eq!(group, None);
    }

    #[test]
    fn memory_limit_disappeared_group_is_empty_but_scan_errors_are_not() {
        let missing = group_pids_with(|_| {
            unsafe {
                *libc::__error() = libc::ESRCH;
            }
            -1
        });
        assert_eq!(missing, Some(Vec::new()));
        let denied = group_pids_with(|_| {
            unsafe {
                *libc::__error() = libc::EACCES;
            }
            -1
        });
        assert_eq!(denied, None);
    }

    struct Group(Vec<std::process::Child>);
    impl Drop for Group {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn memory_limit_macos_decodes_exact_three_process_group_and_measures_cost() {
        let first = std::process::Command::new("/bin/sleep")
            .env_clear()
            .arg("20")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = first.id() as i32;
        let mut group = Group(vec![first]);
        for _ in 0..2 {
            group.0.push(
                std::process::Command::new("/bin/sleep")
                    .env_clear()
                    .arg("20")
                    .process_group(pgid)
                    .spawn()
                    .unwrap(),
            );
        }
        let mut expected: Vec<_> = group.0.iter().map(|child| child.id()).collect();
        expected.sort_unstable();
        let mut actual = group_pids(pgid).unwrap();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "libproc returns PID counts, not byte counts"
        );
        let start = Instant::now();
        for _ in 0..1000 {
            assert!(group_bytes(pgid).unwrap() > 0);
        }
        println!(
            "memory watcher: 1000 samples, 3 processes; {:.2} us/sample; interval {} ms",
            start.elapsed().as_secs_f64() * 1000.0,
            SAMPLE_INTERVAL.as_millis()
        );
    }
}
