//! Cached lifecycle census for health and status surfaces.
//!
//! Collection runs only from the off-path health rollup worker. Reply paths read
//! [`LifecycleCensusCache`] so a control request never spawns a helper process or
//! waits behind filesystem/process enumeration.

use std::collections::BTreeMap;
#[cfg(target_os = "macos")]
use std::ffi::CStr;
use std::sync::RwLock;

use serde::Serialize;

use crate::context::{App, AppContext};

// Thread enumeration is only wired on Linux and macOS; other targets report the
// count unclassified.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const THREAD_CLASSIFICATION_CAP: usize = 4_096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct LspChildRootSnapshot {
    pub root: String,
    pub kind: String,
    pub count: u64,
    pub rss_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LspCensus {
    pub children_total: u64,
    pub children_by_root: Vec<LspChildRootSnapshot>,
    pub children_roots_total: u64,
    pub children_roots_omitted: u64,
    pub children_omitted_total: u64,
    pub children_omitted_rss_bytes: u64,
    pub children_without_client: u64,
    pub children_with_deleted_cwd: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ThreadCensus {
    pub total: u64,
    pub classified: bool,
    pub by_class: BTreeMap<String, u64>,
}

impl Default for ThreadCensus {
    fn default() -> Self {
        Self {
            total: 0,
            classified: true,
            by_class: empty_thread_classes(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ChildrenCensus {
    pub detached_total: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct FdCensus {
    pub open: u64,
    pub soft_limit: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct LifecycleCensusSnapshot {
    pub lsp: LspCensus,
    pub threads: ThreadCensus,
    pub sqlite: crate::db::SqliteConnectionSnapshot,
    pub children: ChildrenCensus,
    pub fds: FdCensus,
}

/// A published snapshot is cheap to clone and avoids control-path OS probes.
#[derive(Default)]
pub(crate) struct LifecycleCensusCache {
    snapshot: RwLock<LifecycleCensusSnapshot>,
}

impl LifecycleCensusCache {
    pub(crate) fn publish(&self, snapshot: LifecycleCensusSnapshot) {
        match self.snapshot.write() {
            Ok(mut target) => *target = snapshot,
            Err(error) => *error.into_inner() = snapshot,
        }
    }

    pub(crate) fn snapshot(&self) -> LifecycleCensusSnapshot {
        match self.snapshot.try_read() {
            Ok(snapshot) => snapshot.clone(),
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner().clone(),
            // Preserve every field with honest zero values instead of making a
            // reply block behind a concurrent rollup publication.
            Err(std::sync::TryLockError::WouldBlock) => LifecycleCensusSnapshot::default(),
        }
    }
}

/// Collect the process census after root contexts have been snapshotted by the
/// health worker. Registry locks are held only long enough to copy PIDs; all OS
/// probes happen after those locks are released.
pub(crate) fn collect(
    app: &App,
    contexts: &[std::sync::Arc<AppContext>],
) -> LifecycleCensusSnapshot {
    let lsp_health = app.lsp_child_registry().health_snapshot();
    let lsp = LspCensus {
        children_total: lsp_health.children_total as u64,
        children_by_root: lsp_health
            .children_by_root
            .into_iter()
            .map(|row| LspChildRootSnapshot {
                root: row.root,
                kind: row.kind,
                count: row.count as u64,
                rss_bytes: row.rss_bytes,
            })
            .collect(),
        children_roots_total: lsp_health.children_roots_total as u64,
        children_roots_omitted: lsp_health.children_roots_omitted as u64,
        children_omitted_total: lsp_health.children_omitted_total as u64,
        children_omitted_rss_bytes: lsp_health.children_omitted_rss_bytes,
        children_without_client: lsp_health.children_without_client as u64,
        children_with_deleted_cwd: lsp_health.children_with_deleted_cwd as u64,
    };
    let detached_total = contexts
        .iter()
        .map(|context| context.bash_background().detached_live_process_count())
        .sum::<usize>() as u64;

    LifecycleCensusSnapshot {
        lsp,
        threads: thread_census(),
        sqlite: crate::db::connection_snapshot(),
        children: ChildrenCensus { detached_total },
        fds: fd_census(),
    }
}

const THREAD_CLASSES: &[&str] = &[
    "aft-inspect",
    "aft-fs-lock-heartbeat",
    "aft-lsp",
    "notify-rs fsevents loop",
    "reqwest-internal-sync-runtime",
    "tokio-rt-worker",
    "aft-callgraph",
    "aft-mem",
    "aft-health-rollup",
    "unnamed",
];

fn empty_thread_classes() -> BTreeMap<String, u64> {
    THREAD_CLASSES
        .iter()
        .map(|class| ((*class).to_string(), 0))
        .collect()
}

/// Classify a kernel thread name into the bounded health vocabulary. Unknown
/// names intentionally join `unnamed`: the report tracks leak-prone anonymous
/// workers without making every dependency thread a schema addition.
pub(crate) fn thread_class(name: Option<&str>) -> &'static str {
    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return "unnamed";
    };
    if name.starts_with("aft-inspect-") {
        "aft-inspect"
    } else if name.starts_with("aft-fs-lock-heartbeat") {
        "aft-fs-lock-heartbeat"
    } else if name.starts_with("aft-lsp-") {
        "aft-lsp"
    } else if name.starts_with("notify-rs fsevents loop") {
        "notify-rs fsevents loop"
    } else if name.starts_with("reqwest-internal-sync-runtime") {
        "reqwest-internal-sync-runtime"
    } else if name.starts_with("tokio-rt-worker") {
        "tokio-rt-worker"
    } else if name.starts_with("aft-callgraph-") {
        "aft-callgraph"
    } else if name.starts_with("aft-mem-") {
        "aft-mem"
    } else if name.starts_with("aft-health-rollup") {
        "aft-health-rollup"
    } else {
        "unnamed"
    }
}

fn thread_census() -> ThreadCensus {
    let (count, names) = os_thread_names();
    let total = count as u64;
    let Some(names) = names else {
        return ThreadCensus {
            total,
            classified: false,
            by_class: empty_thread_classes(),
        };
    };

    let mut by_class = empty_thread_classes();
    for name in names {
        *by_class
            .entry(thread_class(name.as_deref()).to_string())
            .or_default() += 1;
    }
    ThreadCensus {
        total,
        classified: true,
        by_class,
    }
}

#[cfg(target_os = "linux")]
fn os_thread_names() -> (usize, Option<Vec<Option<String>>>) {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return (0, Some(Vec::new()));
    };
    let entries = entries.flatten().collect::<Vec<_>>();
    let count = entries.len();
    if count > THREAD_CLASSIFICATION_CAP {
        // Count directory entries without opening every `comm` file. A runaway
        // thread count must not turn the health rollup into thousands of reads.
        return (count, None);
    }
    let names = entries
        .into_iter()
        .map(|entry| {
            std::fs::read_to_string(entry.path().join("comm"))
                .ok()
                .map(|name| name.trim_end().to_string())
                .filter(|name| !name.is_empty())
        })
        .collect();
    (count, Some(names))
}

#[cfg(target_os = "macos")]
fn os_thread_names() -> (usize, Option<Vec<Option<String>>>) {
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    const MAX_THREADS: usize = 16_384;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcThreadInfo {
        _user_time: u64,
        _system_time: u64,
        _cpu_usage: i32,
        _policy: i32,
        _run_state: i32,
        _flags: i32,
        _sleep_time: i32,
        _current_priority: i32,
        _priority: i32,
        _max_priority: i32,
        name: [libc::c_char; 64],
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffer_size: libc::c_int,
        ) -> libc::c_int;
    }

    let mut buffer = vec![
        // SAFETY: proc_pidinfo fills this plain C structure before it is read.
        unsafe { std::mem::zeroed::<ProcThreadInfo>() };
        MAX_THREADS
    ];
    let buffer_size = match libc::c_int::try_from(std::mem::size_of_val(buffer.as_slice())) {
        Ok(size) => size,
        Err(_) => return (0, Some(Vec::new())),
    };
    // SAFETY: `buffer` is valid writable storage for the exact byte count passed.
    let bytes = unsafe {
        proc_pidinfo(
            std::process::id() as libc::c_int,
            PROC_PIDLISTTHREADS,
            0,
            buffer.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if bytes <= 0 {
        return (0, Some(Vec::new()));
    }
    let count = (bytes as usize / std::mem::size_of::<ProcThreadInfo>()).min(buffer.len());
    if count > THREAD_CLASSIFICATION_CAP {
        return (count, None);
    }
    let names = buffer
        .into_iter()
        .take(count)
        .map(|info| {
            // SAFETY: the kernel writes a NUL-terminated thread name into this field.
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }.to_string_lossy();
            (!name.is_empty()).then(|| name.into_owned())
        })
        .collect();
    (count, Some(names))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn os_thread_names() -> (usize, Option<Vec<Option<String>>>) {
    (0, Some(Vec::new()))
}

fn fd_census() -> FdCensus {
    FdCensus {
        open: open_fd_count(),
        soft_limit: soft_fd_limit(),
    }
}

#[cfg(target_os = "linux")]
fn open_fd_count() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as u64)
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn open_fd_count() -> u64 {
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const MAX_FDS: usize = 65_536;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFdInfo {
        _fd: libc::c_int,
        _fd_type: u32,
    }

    #[link(name = "proc")]
    extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffer_size: libc::c_int,
        ) -> libc::c_int;
    }

    let mut buffer = vec![
        // SAFETY: proc_pidinfo fills this plain C structure before it is read.
        unsafe { std::mem::zeroed::<ProcFdInfo>() };
        MAX_FDS
    ];
    let Ok(buffer_size) = libc::c_int::try_from(std::mem::size_of_val(buffer.as_slice())) else {
        return 0;
    };
    // SAFETY: `buffer` is valid writable storage for the exact byte count passed.
    let bytes = unsafe {
        proc_pidinfo(
            std::process::id() as libc::c_int,
            PROC_PIDLISTFDS,
            0,
            buffer.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    (bytes.max(0) as usize / std::mem::size_of::<ProcFdInfo>()) as u64
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_fd_count() -> u64 {
    0
}

#[cfg(unix)]
fn soft_fd_limit() -> u64 {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `limit` points to valid writable storage for getrlimit(2).
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return 0;
    }
    // SAFETY: a successful getrlimit initialized `limit`.
    let limit = unsafe { limit.assume_init() };
    if limit.rlim_cur == libc::RLIM_INFINITY {
        0
    } else {
        limit.rlim_cur as u64
    }
}

#[cfg(not(unix))]
fn soft_fd_limit() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::thread_class;

    #[test]
    fn thread_class_parser_buckets_known_prefixes_and_unnamed_threads() {
        for (name, expected) in [
            ("aft-inspect-scan", "aft-inspect"),
            ("aft-fs-lock-heartbeat", "aft-fs-lock-heartbeat"),
            ("aft-lsp-rust", "aft-lsp"),
            ("notify-rs fsevents loop", "notify-rs fsevents loop"),
            (
                "reqwest-internal-sync-runtime",
                "reqwest-internal-sync-runtime",
            ),
            ("tokio-rt-worker", "tokio-rt-worker"),
            ("aft-callgraph-refresh", "aft-callgraph"),
            ("aft-mem-sampler", "aft-mem"),
            ("aft-health-rollup", "aft-health-rollup"),
            ("dependency-worker", "unnamed"),
        ] {
            assert_eq!(thread_class(Some(name)), expected, "name={name}");
        }
        assert_eq!(thread_class(None), "unnamed");
        assert_eq!(thread_class(Some("   ")), "unnamed");
    }
}
