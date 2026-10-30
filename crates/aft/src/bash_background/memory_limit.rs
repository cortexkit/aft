//! Per-command aggregate memory accounting. Launchers keep shell commands
//! behind a gate until their memory-enforcement setup is complete.
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Matches the existing watchdog tick: two samples per second, only for capped tasks.
pub(super) const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const LINUX_WATCHER_KIND: &str =
    "Linux process-group RSS watcher; kernel memory controller unavailable";
#[cfg(unix)]
const PID_BUDGET: usize = 4096;

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

pub(crate) struct Monitor {
    pub policy: MemoryPolicy,
    last_sample: Option<Instant>,
    exceeded: bool,
    accounting_failed: bool,
    #[cfg(target_os = "linux")]
    bootstrap: Option<std::process::Child>,
    #[cfg(target_os = "linux")]
    retired_cgroup: Option<std::path::PathBuf>,
    #[cfg(windows)]
    job: windows::Job,
}

impl Monitor {
    pub fn prepare(mib: u64, task_id: &str) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        let (cgroup, bootstrap) = linux::create(mib, task_id);
        #[cfg(not(target_os = "linux"))]
        let cgroup = None;
        #[cfg(windows)]
        let job = windows::Job::new(mib, task_id)?;
        let kind = if cgroup.is_some() {
            "Linux cgroup v2 memory.max"
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
            #[cfg(target_os = "linux")]
            bootstrap,
            #[cfg(target_os = "linux")]
            retired_cgroup: None,
            #[cfg(windows)]
            job,
        })
    }

    pub fn attach(&mut self, pid: u32) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.policy.cgroup {
            if linux::attach(path, pid).is_err() {
                self.use_watcher_fallback();
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(helper) = self.bootstrap.take() {
            linux::stop_bootstrap(helper);
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
        if let Some(helper) = self.bootstrap.take() {
            linux::stop_bootstrap(helper);
        }
        if let Some(path) = failed {
            if std::fs::remove_dir(&path).is_err() {
                // If immediate removal fails, retry it when this monitor is dropped.
                self.retired_cgroup = Some(path);
            }
        }
    }

    pub fn recover(policy: MemoryPolicy, task_id: &str) -> Result<Self, String> {
        #[cfg(windows)]
        let job = windows::Job::new(policy.mib, task_id)?;
        let _ = task_id;
        Ok(Self {
            policy,
            last_sample: None,
            exceeded: false,
            accounting_failed: false,
            #[cfg(target_os = "linux")]
            bootstrap: None,
            #[cfg(target_os = "linux")]
            retired_cgroup: None,
            #[cfg(windows)]
            job,
        })
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
            Some(path) => linux::oom(path),
            None => match group_bytes(pgid) {
                Some(bytes) => bytes > self.policy.mib * 1024 * 1024,
                None => {
                    self.accounting_failed = true;
                    true
                }
            },
        };
        #[cfg(target_os = "macos")]
        let over = match group_bytes(pgid) {
            Some(bytes) => bytes > self.policy.mib * 1024 * 1024,
            None => {
                self.accounting_failed = true;
                true
            }
        };
        #[cfg(windows)]
        let over = self.job.exceeded();
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let over = false;
        let _ = pgid;
        self.exceeded = over;
        over
    }

    pub fn terminate(&self) {
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
        if let Some(helper) = self.bootstrap.take() {
            linux::stop_bootstrap(helper);
        }
        #[cfg(target_os = "linux")]
        if let Some(path) = &self.policy.cgroup {
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
        let mut command = std::process::Command::new(executable);
        // The user manager needs its session endpoint, not the daemon's credentials.
        command.env_clear().env("PATH", "/usr/bin:/bin");
        for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
    }

    fn systemd_run_path_with(mut exists: impl FnMut(&Path) -> bool) -> Option<&'static Path> {
        for path in [
            Path::new("/usr/bin/systemd-run"),
            Path::new("/bin/systemd-run"),
        ] {
            if exists(path) {
                return Some(path);
            }
        }
        None
    }

    fn current(pid: u32) -> Option<PathBuf> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        let relative = text.lines().find_map(|line| line.strip_prefix("0::"))?;
        // Paths containing '..' are outside this process's cgroup namespace.
        if Path::new(relative)
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')))
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

    pub(super) fn create(
        mib: u64,
        task_id: &str,
    ) -> (Option<PathBuf>, Option<std::process::Child>) {
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
            .arg(format!("--property=MemoryMax={}", mib * 1024 * 1024))
            .args(["/bin/sh", "-c", "read -r ready"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return (None, None);
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut result = None;
        while std::time::Instant::now() < deadline {
            let child_pid =
                std::fs::read_to_string(format!("/proc/{0}/task/{0}/children", helper.id()))
                    .ok()
                    .and_then(|children| children.split_whitespace().next()?.parse::<u32>().ok());
            let scope = current(helper.id())
                .filter(|scope| scope != &parent)
                .or_else(|| child_pid.and_then(current).filter(|scope| scope != &parent));
            if let Some(scope) = scope {
                if write_control(&scope.join("memory.oom.group"), "1").is_ok()
                    && write_control(&scope.join("memory.swap.max"), "0").is_ok()
                {
                    result = Some(scope);
                }
                break;
            }
            if helper.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Keep the delegated scope alive until the real task has joined its child cgroup.
        // A populated descendant keeps the scope's cgroup from being collected.
        if result.is_some() {
            return (result, Some(helper));
        }
        stop_bootstrap(helper);
        (None, None)
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

    pub(super) fn oom(path: &Path) -> bool {
        std::fs::read_to_string(path.join("memory.events")).is_ok_and(|text| {
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
            monitor.bootstrap = Some(helper);
            monitor.attach(12345).unwrap();
            assert_eq!(fault.attempts(), 1);
            assert!(monitor.bootstrap.is_none());
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

#[cfg(target_os = "linux")]
fn group_bytes(pgid: i32) -> Option<u64> {
    if pgid == 0
        || (pgid > 0
            && unsafe { libc::kill(-pgid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
    {
        return Some(0);
    }
    let mut sum = 0_u64;
    // Bound all work, not just matching members. Reaching the budget fails closed.
    for (index, entry) in std::fs::read_dir("/proc").ok()?.enumerate() {
        if index >= PID_BUDGET {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields: Vec<_> = rest.split_whitespace().collect();
        if fields.get(2).and_then(|value| value.parse::<i32>().ok()) != Some(pgid) {
            continue;
        }
        let rss = fields
            .get(21)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        sum = sum.saturating_add(
            rss.saturating_mul(unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64),
        );
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
        unsafe { proc_listpgrppids(pgid as u32, pointer, size) }
    })
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
    (std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)).then(Vec::new)
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
        assert_eq!(group_pids_with(|_| 0), Some(Vec::new()));
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
