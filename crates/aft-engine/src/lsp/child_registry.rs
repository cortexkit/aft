//! Process-wide registry of LSP child PIDs spawned by `LspClient::spawn`.
//!
//! Mirrors the `BgTaskRegistry` pattern: `Arc`-cloneable handle that the
//! signal handler thread can use to SIGKILL all child language servers
//! before the aft process exits. Without this registry, LSP children get
//! orphaned to PID 1 when aft is SIGTERM'd by its parent (e.g., during
//! plugin bridge.shutdown() or e2e test cleanup), accumulating across runs.
//!
//! The registry intentionally does NOT do graceful shutdown — that takes
//! up to 5 seconds per server (shutdown request + exit notification +
//! poll). Signal handlers must finish quickly. Graceful shutdown still
//! happens on the natural stdin-closed exit path via `LspManager::shutdown_all`.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::lsp::registry::ServerKind;

mod process_group;
pub(super) use process_group::TrackedProcess;
use process_group::{GroupOutcome, GroupSignal};

/// How long a freshly spawned child may wait for its `LspClient` to claim it
/// before the orphan sweep treats it as abandoned. Claiming normally takes
/// microseconds; the window only has to outlast a slow fork under load while
/// still letting the sweep collect a child whose client construction failed.
const SPAWN_CLAIM_WINDOW: Duration = Duration::from_secs(10);

/// Who owns a tracked child right now.
#[derive(Clone, Copy, Debug, Default)]
enum ClientOwnership {
    /// Just spawned; its `LspClient` is still being built and will claim it.
    Spawning { since: Instant },
    /// A live `LspClient` owns the child.
    Live,
    /// No client owns the child: it was dropped without a kill, or the PID
    /// was registered directly. This is the orphan the sweep reaps.
    #[default]
    Gone,
}

impl ClientOwnership {
    /// Whether the orphan sweep may kill this child now.
    fn is_orphan(self, now: Instant) -> bool {
        match self {
            Self::Spawning { since } => now.saturating_duration_since(since) >= SPAWN_CLAIM_WINDOW,
            Self::Live => false,
            Self::Gone => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspChildRootHealth {
    pub root: String,
    pub kind: String,
    pub count: usize,
    pub rss_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LspChildHealth {
    /// Age of the oldest reused PID probe, not of registry ownership metadata.
    pub process_probe_age_ms: u64,
    /// Compatibility fields retain the older `spawned` and `cwd_gone` names.
    pub spawned: usize,
    pub cwd_gone: usize,
    pub children_total: usize,
    pub children_by_root: Vec<LspChildRootHealth>,
    pub children_roots_total: usize,
    pub children_roots_omitted: usize,
    pub children_omitted_total: usize,
    pub children_omitted_rss_bytes: u64,
    pub children_without_client: usize,
    pub children_with_deleted_cwd: usize,
}

/// Identity recorded for a spawned language-server child.
///
/// `root` is the reclaim-marker path (often the session project). `server_root`
/// and `kind` identify the `(ServerKind, workspace root)` pair so a later spawn
/// can reap children that no live client still references.
#[derive(Clone, Debug, Default)]
struct TrackedChild {
    process: TrackedProcess,
    health_probe: Arc<health_probes::HealthProbeCache>,
    root: Option<PathBuf>,
    server_root: Option<PathBuf>,
    kind: Option<ServerKind>,
    /// Whether an `LspClient` owns this child. The orphan sweep and the health
    /// snapshot treat a tracked child with no client as an orphan, except
    /// while its client is still being constructed.
    ownership: ClientOwnership,
}

#[derive(Clone, Default)]
pub struct LspChildRegistry {
    // A child is registered with the project root that owns it. Maintenance
    // checks only these known roots for reclaim markers, avoiding directory
    // scans while still covering servers rooted below a task worktree.
    inner: Arc<Mutex<HashMap<u32, TrackedChild>>>,
    #[cfg(test)]
    force_kill_hook: Option<Arc<dyn Fn(ForceKillPhase) + Send + Sync>>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum ForceKillPhase {
    BeforeSignals,
    BeforeCleanup,
    AfterCleanup,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReapSignal {
    Sigterm,
}

impl LspChildRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a newly-spawned LSP child PID.
    pub fn track(&self, pid: u32) {
        self.track_in_root(pid, None);
    }

    /// Track a child under the project root that owns its language server.
    ///
    /// The root is retained solely for reclaim-marker checks; it is not used
    /// for process cwd inspection or normal shutdown.
    pub fn track_in_root(&self, pid: u32, root: Option<&Path>) {
        self.track_child(pid, root, None, None);
    }

    /// Track a child with the workspace root and server kind used to spawn it.
    ///
    /// `root` remains the reclaim-marker path. `server_root` and `kind` are the
    /// `(ServerKind, workspace root)` pair a later spawn uses to find orphans.
    pub fn track_child(
        &self,
        pid: u32,
        root: Option<&Path>,
        server_root: Option<&Path>,
        kind: Option<&ServerKind>,
    ) {
        if let Ok(mut children) = self.inner.lock() {
            children.insert(
                pid,
                TrackedChild {
                    process: TrackedProcess::new(pid, kind),
                    health_probe: Arc::default(),
                    root: root.map(Path::to_path_buf),
                    server_root: server_root.map(Path::to_path_buf),
                    kind: kind.cloned(),
                    ownership: ClientOwnership::Gone,
                },
            );
        }
    }

    /// Spawn a child while holding the same mutex used by signal cleanup, then
    /// insert its PID before releasing that mutex. This closes the SIGINT /
    /// SIGTERM spawn→track race: if cleanup starts concurrently, it blocks
    /// until the just-spawned child is present in the tracked set.
    pub fn spawn_tracked(&self, command: &mut Command) -> io::Result<Child> {
        self.spawn_tracked_in_root(command, None)
    }

    /// Spawn and register a child with the project root eligible for reclaim
    /// marker reaping.
    pub fn spawn_tracked_in_root(
        &self,
        command: &mut Command,
        root: Option<&Path>,
    ) -> io::Result<Child> {
        self.spawn_tracked_child(command, root, None, None)
            .map(|(child, _)| child)
    }

    /// Spawn and register a child with reclaim-marker root plus server identity.
    pub(super) fn spawn_tracked_child(
        &self,
        command: &mut Command,
        root: Option<&Path>,
        server_root: Option<&Path>,
        kind: Option<&ServerKind>,
    ) -> io::Result<(Child, TrackedProcess)> {
        let mut children = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("LSP child registry mutex poisoned"))?;
        let child = command.spawn()?;
        let process = TrackedProcess::new(child.id(), kind);
        // The caller is about to wrap this child in an `LspClient`. Until it
        // calls `mark_client_live`, the child must not look like an orphan:
        // a maintenance sweep blocked on this lock during the fork would
        // otherwise wake up, see a client-less child and SIGTERM it before
        // the `initialize` request is even written.
        children.insert(
            child.id(),
            TrackedChild {
                process: process.clone(),
                health_probe: Arc::default(),
                root: root.map(Path::to_path_buf),
                server_root: server_root.map(Path::to_path_buf),
                kind: kind.cloned(),
                ownership: ClientOwnership::Spawning {
                    since: Instant::now(),
                },
            },
        );
        Ok((child, process))
    }

    /// Mark that a returned `LspClient` owns a tracked child.
    pub(crate) fn mark_client_live(&self, pid: u32) {
        if let Ok(mut children) = self.inner.lock() {
            if let Some(child) = children.get_mut(&pid) {
                child.ownership = ClientOwnership::Live;
            }
        }
    }

    /// Leave a tracked PID visible to the reaper after its client disappears.
    /// This is primarily a crash/leak backstop: normal client teardown kills and
    /// untracks its child immediately, while a failed teardown remains observable.
    #[cfg(test)]
    pub(crate) fn mark_client_gone(&self, pid: u32) {
        if let Ok(mut children) = self.inner.lock() {
            if let Some(child) = children.get_mut(&pid) {
                child.ownership = ClientOwnership::Gone;
            }
        }
    }

    pub(super) fn finish_client(&self, process: &TrackedProcess) {
        self.update_process(process, GroupSignal::Observe, |child| {
            child.ownership = ClientOwnership::Gone;
            true
        });
    }

    #[cfg(unix)]
    pub(super) fn signal_client(&self, process: &TrackedProcess, force: bool) {
        let signal = if force {
            GroupSignal::Kill
        } else {
            GroupSignal::Maintain(Instant::now())
        };
        self.update_process(process, signal, |_| true);
    }

    pub(super) fn try_wait_client(
        &self,
        process: &TrackedProcess,
        child: &mut Child,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        #[cfg(unix)]
        if !process.exit_observed()? {
            return Ok(None);
        } else {
            // Kill lingering descendants before releasing the observed leader's
            // PID. The registry mutex is never held across the eventual reap.
            self.signal_client(process, true);
        }
        let status = process.reap(child)?;
        if status.is_some() {
            // Keep the entry through the final group signal, but not past
            // the owner's confirmed wait.
            self.finish_client(process);
        }
        Ok(status)
    }

    /// Forget a PID (called when the client is dropped or shut down gracefully).
    pub fn untrack(&self, pid: u32) {
        if let Ok(mut children) = self.inner.lock() {
            children.remove(&pid);
        }
    }

    /// Snapshot of currently-tracked PIDs.
    pub fn pids(&self) -> Vec<u32> {
        self.tracked_children()
            .into_iter()
            .map(|(pid, _)| pid)
            .collect()
    }

    #[doc(hidden)]
    pub fn has_children_without_waiting(&self) -> bool {
        self.inner
            .try_lock()
            .is_ok_and(|children| !children.is_empty())
    }

    fn tracked_children(&self) -> Vec<(u32, TrackedChild)> {
        self.inner
            .lock()
            .map(|children| {
                children
                    .iter()
                    .map(|(pid, tracked)| (*pid, tracked.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// PIDs registered for this workspace root and server kind.
    pub fn pids_for_server(&self, server_root: &Path, kind: &ServerKind) -> Vec<u32> {
        self.tracked_children()
            .into_iter()
            .filter(|(_, tracked)| {
                tracked.server_root.as_deref() == Some(server_root)
                    && tracked.kind.as_ref() == Some(kind)
            })
            .map(|(pid, _)| pid)
            .collect()
    }

    /// Signal unreferenced children from a previous start of this server.
    pub(super) fn reap_server_orphans(
        &self,
        root: &Path,
        kind: &ServerKind,
        live: &std::collections::HashSet<u32>,
    ) {
        let now = Instant::now();
        for (pid, child) in self.tracked_children() {
            if !live.contains(&pid)
                && child.server_root.as_deref() == Some(root)
                && child.kind.as_ref() == Some(kind)
            {
                self.update_process(&child.process, GroupSignal::Maintain(now), |_| true);
            }
        }
    }

    pub(super) fn tracked_processes(&self) -> Vec<TrackedProcess> {
        self.tracked_children()
            .into_iter()
            .map(|(_, child)| child.process)
            .collect()
    }

    pub(super) fn force_kill_server(&self, root: &Path, kind: &ServerKind) {
        for (_, child) in self.tracked_children() {
            if child.server_root.as_deref() == Some(root) && child.kind.as_ref() == Some(kind) {
                self.update_process(&child.process, GroupSignal::Kill, |_| true);
            }
        }
    }

    /// Dispatch to captured instances before taking the registry lock. A slow
    /// spawn must not delay the entire deadline batch, and a graceful worker
    /// may already have reaped its child despite not reporting completion yet.
    pub(super) fn force_kill_processes(&self, processes: &[TrackedProcess]) {
        #[cfg(test)]
        if let Some(hook) = &self.force_kill_hook {
            hook(ForceKillPhase::BeforeSignals);
        }
        let finished = processes
            .iter()
            .filter(|process| {
                matches!(
                    process.signal(GroupSignal::Kill),
                    GroupOutcome::Gone | GroupOutcome::Unproven
                )
            })
            .collect::<Vec<_>>();
        // Spawning a child holds the registry mutex across process creation.
        // Send every kill before cleanup can block on that mutex, otherwise a
        // slow fork can leave the remaining servers running without a signal.
        #[cfg(test)]
        if let Some(hook) = &self.force_kill_hook {
            hook(ForceKillPhase::BeforeCleanup);
        }
        for process in finished {
            self.update_process(process, GroupSignal::Observe, |_| true);
        }
        #[cfg(test)]
        if let Some(hook) = &self.force_kill_hook {
            hook(ForceKillPhase::AfterCleanup);
        }
    }

    fn update_process(
        &self,
        process: &TrackedProcess,
        signal: GroupSignal,
        eligible: impl FnOnce(&mut TrackedChild) -> bool,
    ) -> GroupOutcome {
        let Ok(mut children) = self.inner.lock() else {
            return GroupOutcome::Retain;
        };
        let std::collections::hash_map::Entry::Occupied(mut entry) = children.entry(process.pid())
        else {
            return GroupOutcome::Retain;
        };
        if !entry.get().process.same_entry(process) || !eligible(entry.get_mut()) {
            return GroupOutcome::Retain;
        }
        let result = entry.get().process.signal(signal);
        if matches!(result, GroupOutcome::Gone | GroupOutcome::Unproven) {
            entry.remove();
        }
        result
    }

    /// Snapshot tracked children without holding the registry lock across CWD
    /// and RSS probes. The health rollup worker calls this off the reply path.
    pub fn health_snapshot(&self) -> LspChildHealth {
        health_for_children(self.tracked_children())
    }

    /// Non-blocking health snapshot for latency-sensitive probes. The lock only
    /// protects cloning registry metadata; kernel process probes run afterwards.
    pub fn try_health_snapshot(&self) -> Option<LspChildHealth> {
        let children = self
            .inner
            .try_lock()
            .ok()?
            .iter()
            .map(|(pid, tracked)| (*pid, tracked.clone()))
            .collect::<Vec<_>>();
        Some(health_for_children(children))
    }

    /// Signal children that no longer have a client and count groups now gone.
    pub fn reap_children_without_client(&self) -> usize {
        self.reap_orphans_at(Instant::now())
    }

    fn reap_orphans_at(&self, now: Instant) -> usize {
        self.tracked_processes()
            .into_iter()
            .filter(|process| {
                self.update_process(process, GroupSignal::Maintain(now), |tracked| {
                    tracked.ownership.is_orphan(now)
                }) == GroupOutcome::Gone
            })
            .count()
    }

    #[cfg(test)]
    fn reap_children_without_client_using<Terminate>(&self, mut terminate: Terminate) -> usize
    where
        Terminate: FnMut(u32) -> bool,
    {
        let now = Instant::now();
        let mut reaped = 0;
        for (pid, tracked) in self.tracked_children() {
            if tracked.ownership.is_orphan(now) && terminate(pid) {
                self.untrack(pid);
                reaped += 1;
            }
        }
        reaped
    }

    #[cfg(test)]
    fn track_spawning_child_for_test(&self, pid: u32, since: Instant) {
        if let Ok(mut children) = self.inner.lock() {
            children.insert(
                pid,
                TrackedChild {
                    ownership: ClientOwnership::Spawning { since },
                    ..TrackedChild::default()
                },
            );
        }
    }

    /// Signal each child whose working directory no longer exists.
    /// This is a crash/leak backstop; ordinary root teardown still drops the
    /// owning `LspClient` and performs its normal process-group cleanup.
    pub fn reap_children_with_gone_cwd(&self) -> usize {
        self.reap_for_maintenance(false)
    }

    /// Signal children with a deleted cwd or a reclaimed project root.
    ///
    /// Alfonso leaves `<worktree>.reclaimed` beside a settled worktree before
    /// removing its directory. Checking the root registered at spawn time lets
    /// this periodic sweep release the analyzer immediately instead of waiting
    /// for the idle-root TTL or for the directory itself to disappear.
    pub fn reap_children_with_gone_cwd_or_reclaimed_root(&self) -> usize {
        self.reap_for_maintenance(true)
    }

    fn reap_for_maintenance(&self, include_reclaimed_roots: bool) -> usize {
        let now = Instant::now();
        self.tracked_children()
            .into_iter()
            .filter(|(pid, tracked)| {
                let root_ended = matches!(child_cwd_state(*pid), ChildCwdState::Gone)
                    || (include_reclaimed_roots
                        && tracked.root.as_deref().is_some_and(root_has_reclaim_marker));
                self.update_process(&tracked.process, GroupSignal::Maintain(now), |child| {
                    root_ended || child.ownership.is_orphan(now)
                }) == GroupOutcome::Gone
            })
            .count()
    }

    #[cfg(test)]
    fn reap_children_using<Terminate>(
        &self,
        include_reclaimed_roots: bool,
        mut terminate: Terminate,
    ) -> usize
    where
        Terminate: FnMut(u32, ReapSignal) -> bool,
    {
        let mut reaped = 0;
        for (pid, tracked) in self.tracked_children() {
            let has_gone_cwd = matches!(child_cwd_state(pid), ChildCwdState::Gone);
            let has_reclaimed_root = include_reclaimed_roots
                && tracked.root.as_deref().is_some_and(root_has_reclaim_marker);
            if !has_gone_cwd && !has_reclaimed_root {
                continue;
            }
            if terminate(pid, ReapSignal::Sigterm) {
                self.untrack(pid);
                reaped += 1;
            }
        }
        reaped
    }

    /// Force-kill every tracked child synchronously. Used by the signal
    /// handler to prevent orphaned LSP processes when aft is SIGTERM'd.
    /// Returns the number of process groups that were sent SIGKILL.
    ///
    /// On Unix, kills the entire process group (via `killpg`) rather than
    /// just the wrapper PID. Necessary because npm-wrapped LSP servers like
    /// biome ship as `node biome lsp-proxy` shims that spawn the real
    /// `cli-darwin-arm64 biome lsp-proxy` as a child; killing only the
    /// wrapper leaves the real server orphaned to PID 1.
    ///
    /// `LspClient::spawn` puts each child in its own session via `setsid()`
    /// so `pgid == child.id()`.
    #[cfg(unix)]
    pub fn kill_all(&self) -> usize {
        self.tracked_processes()
            .into_iter()
            .filter(|process| {
                self.update_process(process, GroupSignal::Kill, |_| true) == GroupOutcome::Signalled
            })
            .count()
    }

    /// Windows fallback: best-effort kill via `taskkill /F /T`. The `/T`
    /// flag kills the entire process tree (Windows analogue of process
    /// groups). Not technically async-signal-safe but Windows doesn't
    /// deliver signals the same way.
    #[cfg(not(unix))]
    pub fn kill_all(&self) -> usize {
        let pids = self.pids();
        let mut killed = 0;
        for pid in pids {
            if std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status()
                .is_ok()
            {
                killed += 1;
            }
        }
        killed
    }
}

/// Return the sibling marker path written when a worktree is reclaimed.
pub fn reclaim_marker_path(root: &Path) -> PathBuf {
    let mut marker = root.as_os_str().to_os_string();
    marker.push(".reclaimed");
    PathBuf::from(marker)
}

fn root_has_reclaim_marker(root: &Path) -> bool {
    reclaim_marker_path(root).is_file()
}

const LSP_CHILD_ROOT_DETAIL_CAP: usize = 8;

#[path = "child_registry/health_probes.rs"]
mod health_probes;

fn health_for_children(children: Vec<(u32, TrackedChild)>) -> LspChildHealth {
    #[derive(Default)]
    struct RootAggregate {
        count: usize,
        rss_bytes: u64,
        by_kind: BTreeMap<String, (usize, u64)>,
    }

    let mut roots = BTreeMap::<String, RootAggregate>::new();
    let mut cwd_gone = 0;
    let mut without_client = 0;
    let mut process_probe_age_ms = 0;
    let now = Instant::now();
    for (pid, tracked) in &children {
        let (probe, age_ms) = tracked
            .health_probe
            .read(now, || health_probes::ProcessProbe {
                cwd: child_cwd_state(*pid),
                rss_bytes: child_rss_bytes(*pid),
            });
        process_probe_age_ms = process_probe_age_ms.max(age_ms);
        if matches!(probe.cwd, ChildCwdState::Gone) {
            cwd_gone += 1;
        }
        if tracked.ownership.is_orphan(now) {
            without_client += 1;
        }
        let root = tracked
            .server_root
            .as_ref()
            .or(tracked.root.as_ref())
            .map(|root| root.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let kind = tracked
            .kind
            .as_ref()
            .map(|kind| kind.id_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let rss_bytes = probe.rss_bytes;
        let aggregate = roots.entry(root).or_default();
        aggregate.count += 1;
        aggregate.rss_bytes = aggregate.rss_bytes.saturating_add(rss_bytes);
        let kind_aggregate = aggregate.by_kind.entry(kind).or_default();
        kind_aggregate.0 += 1;
        kind_aggregate.1 = kind_aggregate.1.saturating_add(rss_bytes);
    }

    let roots_total = roots.len();
    let mut roots = roots.into_iter().collect::<Vec<_>>();
    roots.sort_by(|(left_root, left), (right_root, right)| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left_root.cmp(right_root))
    });
    let omitted = if roots.len() > LSP_CHILD_ROOT_DETAIL_CAP {
        roots.split_off(LSP_CHILD_ROOT_DETAIL_CAP)
    } else {
        Vec::new()
    };
    let children_omitted_total = omitted.iter().map(|(_, root)| root.count).sum();
    let children_omitted_rss_bytes = omitted
        .iter()
        .map(|(_, root)| root.rss_bytes)
        .fold(0u64, u64::saturating_add);
    let mut children_by_root = roots
        .into_iter()
        .flat_map(|(root, aggregate)| {
            aggregate
                .by_kind
                .into_iter()
                .map(move |(kind, (count, rss_bytes))| LspChildRootHealth {
                    root: root.clone(),
                    kind,
                    count,
                    rss_bytes,
                })
        })
        .collect::<Vec<_>>();
    children_by_root.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.kind.cmp(&right.kind))
    });

    LspChildHealth {
        process_probe_age_ms,
        spawned: children.len(),
        cwd_gone,
        children_total: children.len(),
        children_by_root,
        children_roots_total: roots_total,
        children_roots_omitted: omitted.len(),
        children_omitted_total,
        children_omitted_rss_bytes,
        children_without_client: without_client,
        children_with_deleted_cwd: cwd_gone,
    }
}

#[cfg(target_os = "linux")]
fn child_rss_bytes(pid: u32) -> u64 {
    let Ok(statm) = std::fs::read_to_string(format!("/proc/{pid}/statm")) else {
        return 0;
    };
    let Some(resident_pages) = statm
        .split_whitespace()
        .nth(1)
        .and_then(|pages| pages.parse::<u64>().ok())
    else {
        return 0;
    };
    // SAFETY: sysconf has no pointer arguments and `_SC_PAGESIZE` is valid.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(page_size)
        .ok()
        .and_then(|page_size| resident_pages.checked_mul(page_size))
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn child_rss_bytes(pid: u32) -> u64 {
    const PROC_PIDTASKINFO: libc::c_int = 4;

    #[repr(C)]
    struct ProcTaskInfo {
        _virtual_size: u64,
        resident_size: u64,
        _total_user: u64,
        _total_system: u64,
        _threads_user: u64,
        _threads_system: u64,
        _policy: i32,
        _faults: i32,
        _pageins: i32,
        _cow_faults: i32,
        _messages_sent: i32,
        _messages_received: i32,
        _syscalls_mach: i32,
        _syscalls_unix: i32,
        _context_switches: i32,
        _thread_count: i32,
        _running_thread_count: i32,
        _priority: i32,
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

    let Ok(pid) = libc::c_int::try_from(pid) else {
        return 0;
    };
    // SAFETY: proc_pidinfo fills this C structure before it is read.
    let mut info: ProcTaskInfo = unsafe { std::mem::zeroed() };
    let Ok(buffer_size) = libc::c_int::try_from(std::mem::size_of::<ProcTaskInfo>()) else {
        return 0;
    };
    // SAFETY: `info` is valid writable storage for the exact byte count passed.
    let bytes = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTASKINFO,
            0,
            (&mut info as *mut ProcTaskInfo).cast(),
            buffer_size,
        )
    };
    (bytes > 0).then_some(info.resident_size).unwrap_or(0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn child_rss_bytes(_pid: u32) -> u64 {
    0
}

#[derive(Clone, Copy, Debug)]
enum ChildCwdState {
    Present,
    Gone,
    Unknown,
}

fn child_cwd_state(pid: u32) -> ChildCwdState {
    let Ok(cwd) = child_cwd(pid) else {
        return ChildCwdState::Unknown;
    };
    match cwd.try_exists() {
        Ok(true) => ChildCwdState::Present,
        Ok(false) => ChildCwdState::Gone,
        Err(_) => ChildCwdState::Unknown,
    }
}

#[cfg(target_os = "linux")]
fn child_cwd(pid: u32) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
}

#[cfg(target_os = "macos")]
fn child_cwd(pid: u32) -> io::Result<PathBuf> {
    use std::ffi::CStr;
    use std::mem::{size_of, zeroed};
    use std::os::unix::ffi::OsStrExt;

    const PROC_PIDVNODEPATHINFO: libc::c_int = 9;

    #[repr(C)]
    struct VInfoStat {
        dev: u32,
        mode: u16,
        nlink: u16,
        ino: u64,
        uid: u32,
        gid: u32,
        atime: i64,
        atime_nsec: i64,
        mtime: i64,
        mtime_nsec: i64,
        ctime: i64,
        ctime_nsec: i64,
        birthtime: i64,
        birthtime_nsec: i64,
        size: i64,
        blocks: i64,
        block_size: i32,
        flags: u32,
        generation: u32,
        raw_device: u32,
        spare: [i64; 2],
    }

    #[repr(C)]
    struct VnodeInfo {
        stat: VInfoStat,
        vnode_type: i32,
        pad: i32,
        fsid: [i32; 2],
    }

    #[repr(C)]
    struct VnodeInfoPath {
        info: VnodeInfo,
        path: [libc::c_char; libc::MAXPATHLEN as usize],
    }

    #[repr(C)]
    struct ProcVnodePathInfo {
        cwd: VnodeInfoPath,
        root: VnodeInfoPath,
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

    let pid = libc::c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds c_int"))?;
    // SAFETY: the value is plain C data and proc_pidinfo receives its exact size.
    let mut info: ProcVnodePathInfo = unsafe { zeroed() };
    let buffer_size = libc::c_int::try_from(size_of::<ProcVnodePathInfo>())
        .map_err(|_| io::Error::other("proc vnode path buffer is too large"))?;
    // SAFETY: `info` is valid writable storage for `buffer_size` bytes, and the
    // libproc call does not retain the pointer after returning.
    let bytes = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut ProcVnodePathInfo).cast(),
            buffer_size,
        )
    };
    if bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel writes a NUL-terminated MAXPATHLEN path into this field.
    let cwd = unsafe { CStr::from_ptr(info.cwd.path.as_ptr()) };
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(cwd.to_bytes())))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn child_cwd(_pid: u32) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "child cwd lookup is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn maintenance_keeps_sigterm_ignoring_group_visible_to_shutdown() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::process::Stdio;

        let registry = LspChildRegistry::new();
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; echo ready; read ignored"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .process_group(0);
        let mut child = registry.spawn_tracked(&mut command).unwrap();
        let pid = child.id();
        let mut readiness = String::new();
        let ready = BufReader::new(child.stdout.take().unwrap()).read_line(&mut readiness);
        registry.mark_client_gone(pid);

        let now = Instant::now();
        let reaped = registry.reap_orphans_at(now);
        let tracked = registry.pids();
        let grace = crate::bash_background::process::TERMINATE_GRACE;
        let during_grace = registry.reap_orphans_at(now + grace / 2);
        let before_wait = registry.reap_orphans_at(now + grace);
        // Close stdin even on a missed escalation so the fixture always exits.
        let status = child.wait().unwrap();
        let confirmed = registry.reap_children_without_client();

        assert!(ready.is_ok());
        assert_eq!(readiness.trim(), "ready");
        assert_eq!(reaped, 0, "sending SIGTERM did not reap the group");
        assert_eq!(tracked, vec![pid]);
        assert_eq!(during_grace, 0);
        assert_eq!(before_wait, 0, "SIGKILL delivery is not a reap either");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(confirmed, 1);
        assert!(registry.pids().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_does_not_signal_a_group_with_a_different_start_time() {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let registry = LspChildRegistry::new();
        let mut command = Command::new("sh");
        command
            .args(["-c", "read line; exit 0"])
            .stdin(Stdio::piped())
            .process_group(0);
        let mut child = registry.spawn_tracked(&mut command).unwrap();
        let process = registry.tracked_processes().pop().unwrap();
        process.replace_start_time_for_test();
        registry.mark_client_gone(child.id());
        let reaped = registry.reap_children_without_client();
        let tracked = registry.pids();
        let status = child.wait().unwrap();
        assert_eq!(reaped, 0);
        assert!(tracked.is_empty());
        assert!(
            status.success(),
            "a different leader identity was signalled"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_dispatch_leaves_group_alone_after_its_leader_was_reaped() {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let registry = LspChildRegistry::new();
        let mut leader = Command::new("sh");
        leader
            .args(["-c", "read line; exit 0"])
            .stdin(Stdio::piped())
            .process_group(0);
        let mut leader = registry.spawn_tracked(&mut leader).unwrap();
        let mut member = Command::new("sh")
            .args(["-c", "read line; exit 0"])
            .stdin(Stdio::piped())
            .process_group(leader.id() as i32)
            .spawn()
            .unwrap();
        let snapshot = registry.tracked_processes();
        let leader_status = leader.wait().unwrap();

        registry.force_kill_processes(&snapshot);

        let member_status = member.wait().unwrap();
        assert!(leader_status.success());
        assert!(
            member_status.success(),
            "a group without its leader was signalled"
        );
        assert!(registry.pids().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_deadline_does_not_wait_for_registry_cleanup() {
        use crate::lsp::client::LspClient;
        use crate::lsp::manager::{LspManager, LSP_SHUTDOWN_ALL_BUDGET};
        use crate::lsp::roots::ServerKey;

        let (cleanup_tx, cleanup_rx) = crossbeam_channel::bounded(1);
        let mut registry = LspChildRegistry::new();
        registry.force_kill_hook = Some(Arc::new(move |phase| {
            if matches!(phase, ForceKillPhase::BeforeCleanup) {
                cleanup_tx.send(()).unwrap();
            }
        }));
        let temp = tempfile::tempdir().unwrap();
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let mut clients = Vec::new();
        for index in 0..6 {
            let ready = temp.path().join(format!("ready-{index}"));
            let client = LspClient::spawn(
                ServerKind::Biome,
                temp.path().to_path_buf(),
                Path::new("sh"),
                &[
                    "-c".into(),
                    "trap '' TERM; printf ready > \"$1\"; exec sleep 30".into(),
                    "aft-lsp-test".into(),
                    ready.to_string_lossy().into_owned(),
                ],
                &HashMap::new(),
                event_tx.clone(),
                registry.clone(),
            )
            .unwrap();
            let ready_by = Instant::now() + Duration::from_secs(2);
            while !ready.exists() && Instant::now() < ready_by {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                ready.exists(),
                "stand-in must ignore SIGTERM before shutdown"
            );
            clients.push((
                ServerKey {
                    kind: ServerKind::Biome,
                    root: temp.path().join(index.to_string()),
                },
                client,
            ));
        }
        let pids = registry.pids();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let held_registry = registry.clone();
        let holder = std::thread::spawn(move || {
            let _guard = held_registry.inner.lock().unwrap();
            locked_tx.send(()).unwrap();
            // Process creation can hold this same lock. Keep it held until
            // shutdown returns; the timeout only prevents a broken test hanging.
            release_rx.recv_timeout(Duration::from_secs(10)).is_ok()
        });
        locked_rx.recv().unwrap();

        let started = Instant::now();
        let mut deadlines = Vec::new();
        let outcome = LspManager::shutdown_all_clients_with_wait(
            clients,
            registry.clone(),
            LSP_SHUTDOWN_ALL_BUDGET,
            false,
            started,
            |rx, until| {
                deadlines.push(until.duration_since(started));
                if deadlines.len() == 2 {
                    // All signals have been sent, but cleanup and client drops
                    // cannot finish while the registry mutex is still held.
                    cleanup_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    assert!(rx.try_recv().is_err());
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            },
        );
        release_tx.send(()).ok();
        let released = holder.join().unwrap();

        assert!(
            released,
            "registry cleanup must remain blocked until shutdown returns"
        );
        assert_eq!(
            deadlines,
            [Duration::from_millis(1200), LSP_SHUTDOWN_ALL_BUDGET]
        );
        assert_eq!(outcome.graceful, 0, "{outcome:?}");
        assert_eq!(outcome.forced, 6, "{outcome:?}");
        assert_eq!(
            outcome.unreaped, 6,
            "completion must not precede client drop"
        );
        let reaped_by = Instant::now() + Duration::from_secs(2);
        while Instant::now() < reaped_by
            && (!registry.pids().is_empty()
                || pids
                    .iter()
                    .any(|pid| crate::process_identity::is_process_alive(*pid)))
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(registry.pids().is_empty(), "background cleanup must finish");
        for pid in pids {
            assert!(!crate::process_identity::is_process_alive(pid));
        }
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_exit_reports_unreaped_until_detached_owner_waits() {
        use crate::lsp::manager::{LspManager, LSP_SHUTDOWN_ALL_BUDGET};

        let (killed_tx, killed_rx) = crossbeam_channel::bounded(1);
        let mut registry = LspChildRegistry::new();
        registry.force_kill_hook = Some(Arc::new(move |phase| {
            if matches!(phase, ForceKillPhase::AfterCleanup) {
                killed_tx.send(()).unwrap();
            }
        }));
        let mut child = spawn_shutdown_child(&registry);
        let pid = child.id();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            // Keep the killed child waitable after the dispatcher signals it.
            // A successful signal is not evidence of a reap.
            let released = release_rx.recv_timeout(Duration::from_secs(10)).is_ok();
            child.wait().unwrap();
            released
        });
        let started = Instant::now();
        let mut deadlines = Vec::new();
        let outcome = LspManager::shutdown_all_clients_with_wait(
            Vec::new(),
            registry.clone(),
            LSP_SHUTDOWN_ALL_BUDGET,
            true,
            started,
            |_, until| {
                deadlines.push(until.duration_since(started));
                if deadlines.len() == 2 {
                    // Simulate the final timeout only after the real dispatcher
                    // has signalled the still-waitable child. The
                    // ten-second waits are deadlock guards, not timing oracles.
                    killed_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    assert_eq!(registry.pids(), vec![pid]);
                    assert!(crate::process_identity::is_process_alive(pid));
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            },
        );
        release_tx.send(()).ok();
        assert!(
            owner.join().unwrap(),
            "shutdown must return before the owner waits"
        );
        assert_eq!(
            deadlines,
            [Duration::from_millis(1200), LSP_SHUTDOWN_ALL_BUDGET]
        );
        assert_eq!(outcome.forced, 1, "{outcome:?}");
        assert_eq!(
            outcome.unreaped, 1,
            "a signal must not hide a waitable child"
        );
        assert!(!crate::process_identity::is_process_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_deadline_does_not_wait_for_forced_kill() {
        use crate::lsp::manager::{LspManager, LSP_SHUTDOWN_ALL_BUDGET};

        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let (finished_tx, finished_rx) = crossbeam_channel::bounded(1);
        let mut registry = LspChildRegistry::new();
        registry.force_kill_hook = Some(Arc::new(move |phase| {
            if matches!(phase, ForceKillPhase::BeforeSignals) {
                entered_tx.send(std::thread::current().id()).unwrap();
                let released = release_rx.recv_timeout(Duration::from_secs(10)).is_ok();
                finished_tx.send(released).unwrap();
            }
        }));
        let mut child = spawn_shutdown_child(&registry);
        let started = Instant::now();
        let mut deadlines = Vec::new();
        let outcome = LspManager::shutdown_all_clients_with_wait(
            Vec::new(),
            registry,
            LSP_SHUTDOWN_ALL_BUDGET,
            true,
            started,
            |_, until| {
                // Timeout delivery can be arbitrarily late on a loaded runner.
                // Returning it immediately instead tests the two fixed cutoffs
                // without asking the operating system to schedule us on time.
                deadlines.push(until.duration_since(started));
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            },
        );
        let dispatch_thread = entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).ok();
        let released = finished_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        child.wait().unwrap();
        assert!(released, "the caller must not join or run the blocked kill");
        assert_ne!(dispatch_thread, std::thread::current().id());
        assert_eq!(
            deadlines,
            [Duration::from_millis(1200), LSP_SHUTDOWN_ALL_BUDGET]
        );
        assert_eq!(outcome.forced, 1, "{outcome:?}");
        assert_eq!(outcome.unreaped, 1, "{outcome:?}");
    }

    #[cfg(unix)]
    fn spawn_shutdown_child(registry: &LspChildRegistry) -> Child {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sleep");
        command.arg("30");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        registry.spawn_tracked(&mut command).unwrap()
    }

    #[test]
    fn track_untrack_pids_round_trip() {
        let reg = LspChildRegistry::new();
        reg.track(100);
        reg.track(200);
        let mut pids = reg.pids();
        pids.sort();
        assert_eq!(pids, vec![100, 200]);
        reg.untrack(100);
        assert_eq!(reg.pids(), vec![200]);
    }

    #[test]
    fn clones_share_state() {
        let a = LspChildRegistry::new();
        let b = a.clone();
        a.track(42);
        assert_eq!(b.pids(), vec![42]);
        b.untrack(42);
        assert!(a.pids().is_empty());
    }

    #[test]
    fn pids_for_server_filters_by_root_and_kind() {
        let reg = LspChildRegistry::new();
        let root_a = PathBuf::from("/tmp/a");
        let root_b = PathBuf::from("/tmp/b");
        reg.track_child(1, Some(&root_a), Some(&root_a), Some(&ServerKind::Rust));
        reg.track_child(
            2,
            Some(&root_a),
            Some(&root_a),
            Some(&ServerKind::TypeScript(None)),
        );
        reg.track_child(3, Some(&root_b), Some(&root_b), Some(&ServerKind::Rust));
        let mut rust_a = reg.pids_for_server(&root_a, &ServerKind::Rust);
        rust_a.sort();
        assert_eq!(rust_a, vec![1]);
        reg.untrack(1);
        reg.untrack(2);
        reg.untrack(3);
    }

    #[test]
    fn untracking_unknown_pid_is_safe() {
        let reg = LspChildRegistry::new();
        reg.untrack(999); // no-op, no panic
        assert!(reg.pids().is_empty());
    }

    #[test]
    fn health_snapshot_counts_spawned_child_with_live_cwd() {
        let reg = LspChildRegistry::new();
        reg.track(std::process::id());
        let health = reg.health_snapshot();
        assert_eq!(health.spawned, 1);
        assert_eq!(health.children_total, 1);
        assert_eq!(health.cwd_gone, 0);
        assert_eq!(health.children_with_deleted_cwd, 0);
        assert_eq!(health.children_without_client, 1);
        reg.untrack(std::process::id());
    }

    #[test]
    fn kill_all_with_no_pids_returns_zero() {
        let reg = LspChildRegistry::new();
        assert_eq!(reg.kill_all(), 0);
    }

    #[test]
    fn spawn_tracked_records_pid_before_returning() {
        let reg = LspChildRegistry::new();
        let mut command = if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit", "0"]);
            command
        } else {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };

        let mut child = reg.spawn_tracked(&mut command).expect("spawn tracked");
        let pid = child.id();
        assert!(reg.pids().contains(&pid));
        let _ = child.wait();
        reg.untrack(pid);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn maintenance_reaps_child_whose_cwd_was_deleted() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let root = tempfile::tempdir().expect("tempdir");
        let reg = LspChildRegistry::new();
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec sleep 60"])
            .current_dir(root.path());
        // Match LspClient: each child leads its own process group, allowing the
        // maintenance backstop to kill wrappers and descendants together.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = reg.spawn_tracked(&mut command).expect("spawn child");
        reg.mark_client_live(child.id());
        root.close().expect("delete child cwd");

        let health = reg.health_snapshot();
        assert_eq!(health.spawned, 1);
        assert_eq!(health.children_total, 1);
        assert_eq!(health.cwd_gone, 1);
        assert_eq!(health.children_with_deleted_cwd, 1);
        assert_eq!(health.children_without_client, 0);
        assert_eq!(reg.reap_children_with_gone_cwd(), 0);
        assert_eq!(
            child.wait().expect("reap child").signal(),
            Some(libc::SIGTERM)
        );
        reg.mark_client_gone(child.id());
        // The kernel can still report the group present after wait. The
        // registry must drop it without claiming a confirmed group reap.
        reg.reap_children_without_client();
        assert_eq!(reg.health_snapshot(), LspChildHealth::default());
    }

    // Regression for the npm-wrapper orphan bug: biome ships as `node
    // biome lsp-proxy` (the wrapper) that spawns
    // `cli-darwin-arm64 biome lsp-proxy` (the actual server) as a child.
    // Killing just the wrapper PID via `kill(2)` leaves the real server
    // orphaned to PID 1. `killpg(2)` kills the whole group.
    //
    // This test simulates that two-process structure with a shell pipeline:
    // a parent shell that forks a child `sleep`. The parent stays attached
    // (via wait), so both die when the group is killed.
    #[cfg(unix)]
    #[test]
    fn kill_all_kills_process_group_not_just_wrapper_pid() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::thread;
        use std::time::{Duration, Instant};

        /// Running process (excludes zombies: kill(0) still succeeds on zombies).
        fn process_running(pid: u32) -> bool {
            let Ok(pid_i) = i32::try_from(pid) else {
                return false;
            };
            let output = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid_i.to_string()])
                .output()
                .expect("ps");
            if !output.status.success() {
                return false;
            }
            let stat = String::from_utf8_lossy(&output.stdout);
            !stat.is_empty() && !stat.contains('Z')
        }

        fn wait_until_not_running(pid: u32, timeout: Duration) -> bool {
            let started = Instant::now();
            while started.elapsed() < timeout {
                if !process_running(pid) {
                    return true;
                }
                thread::sleep(Duration::from_millis(50));
            }
            false
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("grandchild.pid");
        // Pass the path via env so the shell never interpolates TMPDIR characters
        // (e.g. embedded single quotes) into the script literal.
        const PID_FILE_ENV: &str = "AFT_LSP_KILLALL_TEST_PID_FILE";

        let mut child = unsafe {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("sleep 60 & echo $! > \"$AFT_LSP_KILLALL_TEST_PID_FILE\"; wait")
                .env(PID_FILE_ENV, &pid_file);
            // setsid() so wrapper becomes its own process-group leader,
            // matching what LspClient::spawn does.
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
            cmd.spawn().expect("spawn wrapper")
        };

        let wrapper_pid = child.id();
        let started = Instant::now();
        // Wait for parseable CONTENT, not mere existence: the shell's `>`
        // redirect creates the file before `echo` writes into it, so an
        // existence check can win the race against an empty file and fail the
        // parse. Under a loaded machine that window is wide enough to hit.
        let grandchild_pid: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|contents| contents.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "timed out waiting for a parseable grandchild pid file"
            );
            thread::sleep(Duration::from_millis(20));
        };

        assert!(process_running(wrapper_pid), "wrapper should be running");
        assert!(
            process_running(grandchild_pid),
            "grandchild should be running"
        );

        let reg = LspChildRegistry::new();
        reg.track(wrapper_pid);
        let killed = reg.kill_all();
        assert_eq!(killed, 1, "should report 1 group killed");

        let _ = child.wait();

        assert!(
            wait_until_not_running(wrapper_pid, Duration::from_secs(5)),
            "wrapper must stop after killpg"
        );
        // without killpg() the grandchild would survive as an orphan.
        assert!(
            wait_until_not_running(grandchild_pid, Duration::from_secs(5)),
            "grandchild must stop after killpg (this was the npm-wrapper orphan bug)"
        );
    }

    /// A child that `LspClient::spawn` has just started is not an orphan: its
    /// client simply has not finished taking the pipes yet. The periodic
    /// maintenance sweep used to block on the registry lock while the spawn
    /// held it, then saw the new child as client-less and SIGTERMed it.
    #[cfg(unix)]
    #[test]
    fn maintenance_leaves_child_whose_client_is_still_being_built() {
        use std::os::unix::process::CommandExt;

        let reg = LspChildRegistry::new();
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 60"]);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let (mut child, _) = reg
            .spawn_tracked_child(&mut command, None, None, None)
            .expect("spawn child");
        let pid = child.id();

        let reaped = reg.reap_children_with_gone_cwd_or_reclaimed_root();
        let still_running = child.try_wait().expect("poll child").is_none();
        let orphan_count = reg.health_snapshot().children_without_client;
        let _ = child.kill();
        let _ = child.wait();
        reg.untrack(pid);

        assert_eq!(
            reaped, 0,
            "a child still being handed to its client was reaped"
        );
        assert!(
            still_running,
            "the sweep signalled a child still being handed to its client"
        );
        assert_eq!(
            orphan_count, 0,
            "a child mid-spawn must not count as an orphan"
        );
    }

    /// A child whose client never took ownership (for example because
    /// `LspClient::spawn` failed after starting it) must not stay protected
    /// forever: once the claim window has passed, the sweep reaps it.
    #[test]
    fn maintenance_reaps_spawning_child_that_was_never_claimed() {
        let registry = LspChildRegistry::new();
        registry.track_spawning_child_for_test(42, Instant::now() - SPAWN_CLAIM_WINDOW);
        assert_eq!(registry.health_snapshot().children_without_client, 1);
        let mut signals = Vec::new();
        let reaped = registry.reap_children_without_client_using(|pid| {
            signals.push(pid);
            true
        });
        assert_eq!(reaped, 1);
        assert_eq!(signals, vec![42]);
        assert!(registry.pids().is_empty());
    }

    #[test]
    fn maintenance_reaps_child_at_existing_reclaimed_worktree() {
        let parent = tempfile::tempdir().expect("tempdir");
        let worktree = parent.path().join("task-worktree");
        std::fs::create_dir(&worktree).expect("create worktree");
        std::fs::write(reclaim_marker_path(&worktree), "settled\n").expect("write reclaim marker");

        let registry = LspChildRegistry::new();
        registry.track_in_root(42, Some(&worktree));
        registry.mark_client_live(42);
        let mut signals = Vec::new();
        let reaped = registry.reap_children_using(true, |pid, signal| {
            signals.push((pid, signal));
            true
        });

        assert!(
            worktree.is_dir(),
            "the marker must reap an existing worktree"
        );
        assert_eq!(reaped, 1);
        assert_eq!(signals, vec![(42, ReapSignal::Sigterm)]);
        assert!(registry.pids().is_empty(), "reaped child must be untracked");
    }

    #[test]
    fn maintenance_keeps_child_when_existing_worktree_has_no_reclaim_marker() {
        let parent = tempfile::tempdir().expect("tempdir");
        let worktree = parent.path().join("active-worktree");
        std::fs::create_dir(&worktree).expect("create worktree");

        let registry = LspChildRegistry::new();
        registry.track_in_root(42, Some(&worktree));
        registry.mark_client_live(42);
        let mut signals = Vec::new();
        let reaped = registry.reap_children_using(true, |pid, signal| {
            signals.push((pid, signal));
            true
        });

        assert_eq!(reaped, 0);
        assert!(
            signals.is_empty(),
            "active worktrees must not receive SIGTERM"
        );
        assert_eq!(registry.pids(), vec![42]);
    }
}
