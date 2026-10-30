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
    pub applicability_by_root: BTreeMap<String, crate::lsp::manager::ApplicabilityMetrics>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ThreadCensus {
    pub total: u64,
    pub classified: bool,
    pub by_class: BTreeMap<String, u64>,
    /// Age of the `by_class` figures. The health rollup counts threads on
    /// every pass but reads every thread's name only every
    /// [`THREAD_CLASSIFICATION_REUSE`], so between those reads `by_class`
    /// is an earlier pass's figures and need not sum to `total`.
    pub classified_age_ms: u64,
}

impl Default for ThreadCensus {
    fn default() -> Self {
        Self {
            total: 0,
            classified: true,
            by_class: empty_thread_classes(),
            classified_age_ms: 0,
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

    pub(crate) fn lsp_applicability_for_root(
        &self,
        root: &str,
    ) -> Option<crate::lsp::manager::ApplicabilityMetrics> {
        let snapshot = match self.snapshot.try_read() {
            Ok(snapshot) => snapshot,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        snapshot.lsp.applicability_by_root.get(root).cloned()
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
        applicability_by_root: contexts
            .iter()
            .filter_map(|context| {
                context.config().project_root.as_ref().map(|root| {
                    let root = crate::inspect::job::canonicalize_normalized(root);
                    (
                        root.display().to_string(),
                        crate::lsp::manager::applicability_metrics(&root),
                    )
                })
            })
            .collect(),
    };
    let detached_total = contexts
        .iter()
        .map(|context| context.bash_background().detached_live_process_count())
        .sum::<usize>() as u64;

    LifecycleCensusSnapshot {
        lsp,
        threads: sampled_thread_census(),
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
    // Linux exposes at most 15 bytes in /proc task comm, even when the
    // thread builder was given a longer name. Match that exact truncation,
    // not arbitrary short prefixes that could belong to unrelated workers.
    let matches = |prefix: &str| {
        name.starts_with(prefix)
            || (cfg!(target_os = "linux") && prefix.len() > 15 && name == &prefix[..15])
    };
    if name.starts_with("aft-inspect-") || matches("aft-rayon-inspect-") {
        "aft-inspect"
    } else if matches("aft-fs-lock-heartbeat") {
        "aft-fs-lock-heartbeat"
    } else if name.starts_with("aft-lsp-") {
        "aft-lsp"
    } else if matches("notify-rs fsevents loop") {
        "notify-rs fsevents loop"
    } else if matches("reqwest-internal-sync-runtime") {
        "reqwest-internal-sync-runtime"
    } else if name.starts_with("tokio-rt-worker") {
        "tokio-rt-worker"
    } else if name.starts_with("aft-callgraph-") || matches("aft-rayon-callgraph-") {
        "aft-callgraph"
    } else if name.starts_with("aft-mem-") {
        "aft-mem"
    } else if matches("aft-health-rollup") {
        "aft-health-rollup"
    } else {
        "unnamed"
    }
}

pub(crate) fn thread_census() -> ThreadCensus {
    #[cfg(test)]
    THREAD_CLASSIFICATIONS.with(|count| count.set(count.get() + 1));
    let (count, names) = os_thread_names();
    let total = count as u64;
    let Some(names) = names else {
        return ThreadCensus {
            total,
            classified: false,
            by_class: empty_thread_classes(),
            classified_age_ms: 0,
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
        classified_age_ms: 0,
    }
}

/// How long the health rollup reuses a thread classification. Classifying
/// reads every thread's name with one system call or `/proc` file read per
/// thread; a daemon serving many roots runs hundreds of threads, and the
/// rollup runs every few seconds. Counting them stays one call per pass.
const THREAD_CLASSIFICATION_REUSE: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
thread_local! {
    /// Full thread classifications (one name read per thread) on this thread.
    static THREAD_CLASSIFICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Only the census test reads it, and that test runs where threads can be named.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) fn thread_classifications_for_test() -> usize {
    THREAD_CLASSIFICATIONS.with(std::cell::Cell::get)
}

/// The thread census for the health rollup: a fresh total on every call, and
/// a classification reused for up to [`THREAD_CLASSIFICATION_REUSE`], with
/// its age in `classified_age_ms`. Callers that need an exact classification
/// use [`thread_census`].
pub(crate) fn sampled_thread_census() -> ThreadCensus {
    static LAST: std::sync::Mutex<Option<(std::time::Instant, ThreadCensus)>> =
        std::sync::Mutex::new(None);
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((classified_at, census)) = last.as_ref() {
        let age = classified_at.elapsed();
        if age < THREAD_CLASSIFICATION_REUSE {
            return ThreadCensus {
                total: os_thread_count() as u64,
                classified_age_ms: u64::try_from(age.as_millis()).unwrap_or(u64::MAX),
                ..census.clone()
            };
        }
    }
    let census = thread_census();
    // An unclassified census (too many threads to name) has nothing to
    // reuse; the next pass counts again.
    *last = census
        .classified
        .then(|| (std::time::Instant::now(), census.clone()));
    census
}

#[cfg(target_os = "linux")]
fn os_thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |entries| entries.count())
}

#[cfg(not(target_os = "linux"))]
fn os_thread_count() -> usize {
    // macOS lists thread IDs with one call; the names are what cost a call
    // per thread. Other targets report zero, as `os_thread_names` does.
    os_thread_ids().map_or(0, |ids| ids.len())
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

/// This process's thread IDs, or `None` when the kernel refused the listing.
#[cfg(target_os = "macos")]
fn os_thread_ids() -> Option<Vec<u64>> {
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    const MAX_THREADS: usize = 16_384;

    let mut ids = vec![0u64; MAX_THREADS];
    let buffer_size = std::mem::size_of_val(ids.as_slice()) as libc::c_int;
    // SAFETY: ids is writable storage of the passed byte length.
    let bytes = unsafe {
        proc_pidinfo(
            std::process::id() as libc::c_int,
            PROC_PIDLISTTHREADS,
            0,
            ids.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if bytes <= 0 {
        return None;
    }
    let count = (bytes as usize / std::mem::size_of::<u64>()).min(ids.len());
    ids.truncate(count);
    Some(ids)
}

/// This process's thread IDs from a Toolhelp snapshot, or `None` when the
/// snapshot could not be taken. The snapshot lists every thread on the
/// system, so it is filtered to this process.
#[cfg(windows)]
fn os_thread_ids() -> Option<Vec<u64>> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };

    let pid = std::process::id();
    // SAFETY: plain FFI call; the returned handle is checked and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return None;
    }
    // SAFETY: THREADENTRY32 is a plain C record; dwSize is set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut ids = Vec::new();
    // SAFETY: snapshot is a valid Toolhelp handle and entry is writable with
    // its size recorded in dwSize.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            ids.push(u64::from(entry.th32ThreadID));
        }
        // SAFETY: as for Thread32First.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: snapshot is a handle this function owns.
    unsafe {
        CloseHandle(snapshot);
    }
    (!ids.is_empty()).then_some(ids)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn os_thread_ids() -> Option<Vec<u64>> {
    None
}

/// This process's thread IDs, for tests that compare which threads exist
/// before and after some work. Linux and macOS tests read richer per-thread
/// details themselves.
#[cfg(all(test, windows))]
pub(crate) fn thread_ids_for_test() -> Option<Vec<u64>> {
    os_thread_ids()
}

#[cfg(target_os = "macos")]
fn os_thread_names() -> (usize, Option<Vec<Option<String>>>) {
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

    // LISTTHREADS returns thread IDs, not proc_threadinfo records. Query each
    // ID separately so the census cannot silently divide an ID buffer by the
    // larger record size and report zero runtime threads.
    let Some(ids) = os_thread_ids() else {
        return (0, None);
    };
    let count = ids.len();
    if count > THREAD_CLASSIFICATION_CAP {
        return (count, None);
    }
    let names = ids
        .into_iter()
        .map(|id| {
            // SAFETY: this plain C record is initialized before the kernel fills it.
            let mut info: ProcThreadInfo = unsafe { std::mem::zeroed() };
            // SAFETY: info is writable storage of the exact record size.
            let received = unsafe {
                proc_pidinfo(
                    std::process::id() as libc::c_int,
                    5,
                    id,
                    (&mut info as *mut ProcThreadInfo).cast(),
                    std::mem::size_of::<ProcThreadInfo>() as libc::c_int,
                )
            };
            if received != std::mem::size_of::<ProcThreadInfo>() as libc::c_int {
                return None;
            }
            info.name[63] = 0;
            // SAFETY: the final byte is explicitly terminated even if the kernel truncated it.
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }.to_string_lossy();
            (!name.is_empty()).then(|| name.into_owned())
        })
        .collect();
    (count, Some(names))
}

/// Windows counts threads from a Toolhelp snapshot but does not read their
/// names (that takes a handle per thread), so the census is unclassified.
#[cfg(windows)]
fn os_thread_names() -> (usize, Option<Vec<Option<String>>>) {
    (os_thread_ids().map_or(0, |ids| ids.len()), None)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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

    /// The health rollup counts threads on every pass but must not read every
    /// thread's name each time: a reused classification is served with its
    /// age, while the exact census used elsewhere still names every thread.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn rollup_thread_census_reuses_its_classification_and_reports_its_age() {
        let first = super::sampled_thread_census();
        assert!(first.total > 0, "the census found no threads: {first:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let before = super::thread_classifications_for_test();
        let reused = super::sampled_thread_census();
        assert_eq!(
            super::thread_classifications_for_test() - before,
            0,
            "a rollup pass read every thread's name again"
        );
        assert!(reused.total > 0);
        assert!(
            reused.classified_age_ms >= 20,
            "a reused classification must report its age: {reused:?}"
        );
        assert_eq!(reused.by_class, first.by_class);

        let before = super::thread_classifications_for_test();
        let exact = super::thread_census();
        assert_eq!(super::thread_classifications_for_test() - before, 1);
        assert_eq!(exact.classified_age_ms, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn thread_class_recognizes_linux_comm_truncation() {
        for (full, expected) in [
            ("aft-rayon-inspect-0", "aft-inspect"),
            ("aft-rayon-callgraph-build-0", "aft-callgraph"),
            ("aft-fs-lock-heartbeat", "aft-fs-lock-heartbeat"),
            ("aft-health-rollup", "aft-health-rollup"),
            (
                "reqwest-internal-sync-runtime",
                "reqwest-internal-sync-runtime",
            ),
        ] {
            let comm = std::thread::Builder::new()
                .name(full.to_string())
                .spawn(|| std::fs::read_to_string("/proc/thread-self/comm").unwrap())
                .unwrap()
                .join()
                .unwrap();
            assert_eq!(thread_class(Some(&comm)), expected, "name={full}");
        }
        assert_eq!(thread_class(Some("aft-fs-lock")), "unnamed");
    }

    #[test]
    fn thread_class_parser_buckets_known_prefixes_and_unnamed_threads() {
        for (name, expected) in [
            ("aft-inspect-scan", "aft-inspect"),
            ("aft-rayon-inspect-0", "aft-inspect"),
            ("aft-fs-lock-heartbeat", "aft-fs-lock-heartbeat"),
            ("aft-lsp-rust", "aft-lsp"),
            ("notify-rs fsevents loop", "notify-rs fsevents loop"),
            (
                "reqwest-internal-sync-runtime",
                "reqwest-internal-sync-runtime",
            ),
            ("tokio-rt-worker", "tokio-rt-worker"),
            ("aft-callgraph-refresh", "aft-callgraph"),
            ("aft-rayon-callgraph-build-0", "aft-callgraph"),
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
