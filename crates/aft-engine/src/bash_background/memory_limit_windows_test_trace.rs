//! Failure-only diagnostics for native memory-cap tests. Child launch and cap
//! decisions remain unchanged; separate files avoid cross-process append races.
use std::{
    cell::RefCell,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE},
    System::{
        JobObjects::{IsProcessInJob, OpenJobObjectW},
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

thread_local! {
    static TRACE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) struct Guard {
    base: PathBuf,
    previous: Option<PathBuf>,
    pty: bool,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Guard {
    pub(crate) fn new(base: PathBuf, pty: bool) -> Self {
        let previous = TRACE.with(|slot| slot.replace(Some(base.clone())));
        let guard = Self {
            base,
            previous,
            pty,
            _thread_bound: std::marker::PhantomData,
        };
        guard.observe("test_start");
        guard
    }

    pub(crate) fn observe(&self, status: &str) {
        let _ = fs::write(
            self.base.with_extension("status"),
            format!("at_ms={} {status}", now()),
        );
    }

    pub(crate) fn summary(&self) -> String {
        let read = |extension| {
            fs::read_to_string(self.base.with_extension(extension))
                .unwrap_or_else(|error| format!("not_observed({error})"))
        };
        let mut inherited = 0;
        let inherited_ok =
            unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut inherited) };
        format!("MEMORY_FAILURE exe={:?} launch={} breakaway={} parent_in_any_job={inherited} parent_job_query_ok={inherited_ok} status=[{}] waiter_attach_cap_kill=[{}] pwsh_start=[{}] hog_start_membership=[{}] first_sample_over_cap=[{}] job_kill=[{}]", std::env::current_exe(), if self.pty { "pty" } else { "piped" }, if self.pty { "not_requested_by_aft_pty" } else { "see_spawn_output" }, read("status"), read("job"), read("pwsh"), read("hog"), read("over"), read("kill"))
            .replace('\r', "").replace('\n', " | ")
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        TRACE.with(|slot| {
            slot.replace(self.previous.take());
        });
        if std::thread::panicking() {
            eprintln!("{}", self.summary());
        }
    }
}

pub(crate) struct Writer {
    base: PathBuf,
    job_name: String,
}

impl Writer {
    pub(crate) fn current(task: &str) -> Option<Self> {
        TRACE.with(|slot| slot.borrow().clone()).map(|base| {
            let trace = Self {
                base,
                job_name: format!("Local\\AFT-memory-{task}"),
            };
            trace.event("job_create_requested");
            trace
        })
    }

    pub(crate) fn event(&self, event: &str) {
        if event.starts_with("job_kill") {
            let _ = fs::write(
                self.base.with_extension("kill"),
                format!("at_ms={} {event}", now()),
            );
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.base.with_extension("job"))
        {
            let _ = writeln!(file, "at_ms={} {event}", now());
        }
    }

    pub(crate) fn attach_begin(&self, pid: u32) {
        let _ = fs::write(
            self.base.with_extension("identity"),
            format!("{}\n{pid}", self.job_name),
        );
        self.event(&format!("attach_begin pid={pid}"));
    }

    pub(crate) fn sample(&self, peak: Option<usize>, limit: usize) {
        if peak.is_some_and(|peak| peak >= limit) && !self.base.with_extension("over").exists() {
            let _ = fs::write(
                self.base.with_extension("over"),
                format!("at_ms={} peak_bytes={peak:?} limit_bytes={limit}", now()),
            );
            self.event(&format!(
                "first_sample_over_cap peak_bytes={peak:?} limit_bytes={limit}"
            ));
        }
    }
}

pub(crate) fn process(role: &str, pid: u32, job: HANDLE) -> String {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return format!(
                "{role} pid={pid} open_error={}",
                std::io::Error::last_os_error()
            );
        }
        let mut inside = 0;
        let ok = IsProcessInJob(handle, job, &mut inside);
        let error = if ok == 0 {
            std::io::Error::last_os_error().to_string()
        } else {
            "none".into()
        };
        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        // Process creation times and phase observations share Unix milliseconds,
        // so slow image startup can be separated from late allocator execution.
        let times_ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(handle);
        let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        let start_ms = (ticks / 10_000).saturating_sub(11_644_473_600_000);
        format!("{role} pid={pid} in_our_job={inside} query_ok={ok} query_error={error} start_ms={start_ms} times_ok={times_ok}")
    }
}

pub(crate) fn hog_start(base: &Path) {
    let membership = (|| -> Result<String, String> {
        let identity = fs::read_to_string(base.with_extension("identity"))
            .map_err(|error| error.to_string())?;
        let mut lines = identity.lines();
        let name: Vec<u16> = format!("{}\0", lines.next().ok_or("missing job name")?)
            .encode_utf16()
            .collect();
        let root = lines
            .next()
            .ok_or("missing waiter pid")?
            .parse()
            .map_err(|error: std::num::ParseIntError| error.to_string())?;
        let pwsh =
            fs::read_to_string(base.with_extension("pwsh")).map_err(|error| error.to_string())?;
        let pwsh = pwsh
            .split_whitespace()
            .next()
            .ok_or("missing pwsh pid")?
            .parse()
            .map_err(|error: std::num::ParseIntError| error.to_string())?;
        let job = unsafe { OpenJobObjectW(0x0004, 0, name.as_ptr()) };
        if job.is_null() {
            return Err(format!(
                "job_open_error={}",
                std::io::Error::last_os_error()
            ));
        }
        let membership = format!(
            "{}; {}; {}",
            process("waiter", root, job),
            process("pwsh", pwsh, job),
            process("hog", unsafe { GetCurrentProcessId() }, job)
        );
        unsafe {
            CloseHandle(job);
        }
        Ok(membership)
    })();
    let _ = fs::write(
        base.with_extension("hog"),
        format!("hog_start at_ms={} membership={membership:?}", now()),
    );
}

pub(crate) fn hog_allocated(base: &Path) {
    if let Ok(mut file) = fs::OpenOptions::new()
        .append(true)
        .open(base.with_extension("hog"))
    {
        let _ = writeln!(file, " hog_allocated at_ms={}", now());
    }
}

#[test]
fn memory_trace_failure_summary_keeps_phases_on_one_line() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("trace");
    let guard = Guard::new(base.clone(), true);
    fs::write(
        base.with_extension("job"),
        "attach_result ok=false error=5\nwaiter start_ms=123",
    )
    .unwrap();
    fs::write(base.with_extension("pwsh"), "42 124").unwrap();
    fs::write(
        base.with_extension("hog"),
        "hog_start at_ms=125 pwsh in_our_job=0 hog in_our_job=0",
    )
    .unwrap();
    fs::write(base.with_extension("over"), "at_ms=126 peak_bytes=100").unwrap();
    fs::write(
        base.with_extension("kill"),
        "at_ms=127 job_kill_result ok=false error=5",
    )
    .unwrap();
    let summary = guard.summary();
    assert!(!summary.contains(['\r', '\n']), "{summary}");
    for required in [
        "breakaway=not_requested",
        "attach_result ok=false error=5",
        "waiter start_ms=123",
        "pwsh_start=[42 124]",
        "hog_start at_ms=125",
        "pwsh in_our_job=0",
        "hog in_our_job=0",
        "first_sample_over_cap=[at_ms=126",
        "job_kill_result ok=false error=5",
    ] {
        assert!(summary.contains(required), "missing {required}: {summary}");
    }
}
