use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex, OnceLock, Weak,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{slog_debug, slog_error, slog_info, slog_warn};

/// Owns an already-acquired OS file lock, unlike the path/heartbeat lease below.
/// Closing a file alone can leave its lock held by a forked child's descriptor
/// until exec or exit. Explicitly unlock when the protected operation ends.
/// Keep one guard per acquisition; share the guard, not cloned file handles.
#[derive(Debug)]
pub(crate) struct AdvisoryLockGuard(File);

impl AdvisoryLockGuard {
    pub(crate) fn from_locked_file(file: File) -> Self {
        Self(file)
    }
}

impl Drop for AdvisoryLockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const STALE_HEARTBEAT_MS: u64 = 15_000;
pub const LIVE_OWNER_WARN_MS: u64 = 600_000;
pub const POLL_INTERVAL_MS: u64 = 100;

/// Max consecutive transient OS errors tolerated while creating the lock file
/// before giving up. On Windows, two processes/threads racing to create (or one
/// creating while another deletes) the same path can momentarily return
/// ERROR_ACCESS_DENIED (5) or ERROR_SHARING_VIOLATION (32) instead of a clean
/// "already exists". Those windows close in milliseconds, so a small bounded
/// retry rides them out while a genuinely persistent permission/IO failure still
/// surfaces promptly.
const MAX_TRANSIENT_CREATE_RETRIES: u32 = 50;
const RECLAIM_TOKEN_MALFORMED_STALE_AGE: Duration = Duration::from_secs(60);
const RECLAIM_BLOCK_LOG_INTERVAL: Duration = Duration::from_secs(60);
const DEAD_RECLAIM_INITIAL_BACKOFF_MS: u64 = 250;
const DEAD_RECLAIM_MAX_BACKOFF_MS: u64 = 5_000;
const RECLAIM_TOKEN_SWEEP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// A heartbeat normally keeps one descriptor per lease open between beats
/// instead of opening and closing the file on every beat. Closing a lease
/// descriptor is expensive on the live daemon: it pulls the write-back of the
/// lease page into the closing thread, and with hundreds of long-lived leases on
/// macOS that was 10% of a core. The handle is released only when this process
/// replaces the lease, or when a beat finds the lease no longer its own. These
/// entries let those replacements find the open handles; acquisition, ownership
/// checks and reclaim paths on other threads keep their own short-lived opens.
static NEXT_LEASE_SLOT: AtomicU64 = AtomicU64::new(0);
static LEASE_FILES: OnceLock<Mutex<HashMap<usize, LeaseSlot>>> = OnceLock::new();
static LEASE_SLOT_BY_PATH: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

/// One heartbeat handle, filed under the slot that owns it and, in
/// `LEASE_SLOT_BY_PATH`, under the lease path it was opened for. The path entry
/// is what lets a replacement release the handle for *its* lease without
/// touching any other lease's handle.
#[derive(Clone)]
struct LeaseSlot {
    handle: Weak<Mutex<Option<LeaseFile>>>,
}

// Lock files in these storage domains have the fixed layout
// `<storage>/<domain>/<key>/<lock>`. Add a domain here when a new persistent
// artifact lock is introduced. BackupStore's deeper `.locks` shape is handled
// separately so maintenance never walks backup entry histories.
const RECLAIM_TOKEN_SWEEP_DOMAINS: &[&str] = &[
    "index",       // Search-index cache locks.
    "callgraph",   // Callgraph root-cache writer leases.
    "inspect",     // Inspect root-cache writer leases.
    "semantic",    // Semantic-index cache locks.
    "symbols",     // On-disk symbol-cache locks.
    "checkpoints", // Checkpoint-store mutation locks.
    ".aft",        // Storage-migration locks.
];
const RECLAIM_TOKEN_SWEEP_MAX_DOMAIN_DEPTH: usize = 2;
// compress::trust owns this one lock directly under the storage root.
const ROOT_RECLAIM_TOKEN_PATHS: &[&str] = &[".trusted-filter-projects.json.lock.reclaim"];
// BackupStore scopes locks below each harness and session. Prefixes cover the
// client-specific MCP and fingerprint-specific federated harness directories.
const FIXED_BACKUP_HARNESS_DIRS: &[&str] = &["opencode", "pi", "runner"];
const BACKUP_HARNESS_DIR_PREFIXES: &[&str] = &["mcp--", "fed--"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReclaimTokenState {
    Alive,
    DeadForeignHost,
    Malformed,
    Dead,
}

impl ReclaimTokenState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::DeadForeignHost => "dead-foreign-host",
            Self::Malformed => "malformed",
            Self::Dead => "dead",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReclaimTokenHeld {
    pid: Option<u32>,
    state: ReclaimTokenState,
}

#[derive(Debug)]
enum ReclaimResult {
    Removed,
    Blocked(ReclaimTokenHeld),
    Unchanged,
}

#[derive(Clone, Debug)]
struct ReclaimBlockLogRecord {
    last_emitted: Instant,
    suppressed: u64,
}

/// Which live-owner conditions one acquisition already reported, so a single
/// attempt that polls many times (a daemon waiting on its own long-lived lease)
/// logs each condition once instead of on every poll.
#[derive(Clone, Debug, Default)]
struct LiveOwnerLogRecord {
    stale_logged: bool,
    long_held_logged: bool,
}

/// Per-acquisition record of which live-owner conditions it already reported.
struct LiveOwnerLog {
    record: LiveOwnerLogRecord,
}

impl LiveOwnerLog {
    fn new() -> Self {
        Self {
            record: LiveOwnerLogRecord::default(),
        }
    }

    fn first_stale(&mut self) -> bool {
        !std::mem::replace(&mut self.record.stale_logged, true)
    }

    fn first_long_held(&mut self) -> bool {
        !std::mem::replace(&mut self.record.long_held_logged, true)
    }
}

static RECLAIM_BLOCK_LOGS: OnceLock<Mutex<HashMap<PathBuf, ReclaimBlockLogRecord>>> =
    OnceLock::new();
static RECLAIM_TOKEN_SWEEP_LAST_RUN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// True for OS errors that mean "another actor is touching this exact lock path
/// right now", as opposed to a real, persistent failure. On Windows a contended
/// create/delete on the same file surfaces as ERROR_ACCESS_DENIED (5) or
/// ERROR_SHARING_VIOLATION (32); `PermissionDenied` covers the former across
/// platforms. These are retried as contention, never treated as fatal.
fn is_transient_create_contention(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::PermissionDenied {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_SHARING_VIOLATION = 32. ERROR_ACCESS_DENIED = 5 maps to
        // PermissionDenied above, but match it explicitly too in case the OS
        // surfaces it as an Other-kind raw error.
        if let Some(code) = error.raw_os_error() {
            if code == 32 || code == 5 {
                return true;
            }
        }
    }
    false
}

#[derive(Clone, Copy, Debug)]
struct LockConfig {
    heartbeat_interval_ms: u64,
    stale_heartbeat_ms: u64,
    live_owner_warn_ms: u64,
    poll_interval_ms: u64,
}

impl LockConfig {
    fn cross_host_stale_heartbeat_ms(self) -> u64 {
        self.stale_heartbeat_ms.saturating_mul(5)
    }
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: HEARTBEAT_INTERVAL_MS,
            stale_heartbeat_ms: STALE_HEARTBEAT_MS,
            live_owner_warn_ms: LIVE_OWNER_WARN_MS,
            poll_interval_ms: POLL_INTERVAL_MS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessIdentity {
    start_time: u64,
    boot_id: Option<String>,
    exited: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct LockMetadata {
    pid: u32,
    hostname: String,
    /// Identifies this PID incarnation so a process in a restarted PID namespace
    /// cannot be mistaken for an owner that was hard-killed in an earlier launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_start_time: Option<u64>,
    /// Linux start times are relative to boot. Keeping the boot ID alongside the
    /// raw jiffies prevents a reboot from making a newly recycled PID look like
    /// an earlier owner whose start time happened to match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot_id: Option<String>,
    created_at_ms: u64,
    heartbeat_at_ms: u64,
    /// Fencing nonce for writer leases. The owner re-reads the lock immediately
    /// before publishing/writing and aborts if a stale guard has been usurped.
    #[serde(default)]
    writer_epoch: String,
}

/// Acquire a filesystem lock at `path`. Blocks until the lock is held.
///
/// The returned guard owns a background heartbeat thread; dropping it releases
/// the lock and removes the lock file.
pub fn acquire(path: &Path) -> Result<LockGuard, AcquireError> {
    acquire_with_config(path, None, LockConfig::default())
}

/// Try to acquire a filesystem lock at `path` within `timeout`.
pub fn try_acquire(path: &Path, timeout: Duration) -> Result<LockGuard, AcquireError> {
    acquire_with_config(path, Some(timeout), LockConfig::default())
}

/// Preserve a caller's deadline across filesystem preparation instead of
/// starting a fresh timeout after that work has finished. Always allow the
/// first attempt: slow preparation does not imply another writer owns the lock.
pub(crate) fn try_acquire_until(path: &Path, deadline: Instant) -> Result<LockGuard, AcquireError> {
    crate::production_storage::refuse_write(path)?;
    #[cfg(test)]
    retry_clock::before_acquire();
    acquire_with_deadline(path, Some(deadline), false, true, LockConfig::default())
}

/// Try one lock acquisition attempt, then check once whether an existing stale
/// lock can be taken over.
///
/// Read-only cache openers use this to switch to writer mode without waiting
/// behind another process that is still building the cache. Malformed leases
/// receive one poll interval of grace so a creator can finish writing before
/// the existing guarded reclaim path is tried.
pub fn try_acquire_once(path: &Path) -> Result<LockGuard, AcquireError> {
    try_acquire(path, Duration::ZERO)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockAvailability {
    Available,
    LiveOwner,
    Stale,
    Malformed,
}

enum ObservedLock {
    Available,
    LiveOwner(LockMetadata),
    Stale(LockMetadata),
    Malformed(serde_json::Error),
}

impl ObservedLock {
    fn availability(&self) -> LockAvailability {
        match self {
            Self::Available => LockAvailability::Available,
            Self::LiveOwner(_) => LockAvailability::LiveOwner,
            Self::Stale(_) => LockAvailability::Stale,
            Self::Malformed(_) => LockAvailability::Malformed,
        }
    }
}

/// Observe the same lease states used by acquisition, without taking a lock.
/// Malformed contents are not proof of a dead owner: acquisition must still
/// perform its grace-period reread and guarded reclaim. I/O failures remain
/// errors rather than evidence about ownership.
pub(crate) fn lock_availability(path: &Path) -> io::Result<LockAvailability> {
    observe_lock(path, &current_hostname(), LockConfig::default())
        .map(|observed| observed.availability())
}

fn observe_lock(path: &Path, hostname: &str, config: LockConfig) -> io::Result<ObservedLock> {
    match read_lock_metadata(path) {
        Ok(metadata) => {
            if metadata_blocks_acquisition(&metadata, hostname, config, now_ms()) {
                Ok(ObservedLock::LiveOwner(metadata))
            } else {
                Ok(ObservedLock::Stale(metadata))
            }
        }
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Ok(ObservedLock::Available)
        }
        Err(ReadLockError::Io(error)) => Err(error),
        Err(ReadLockError::Malformed(error)) => Ok(ObservedLock::Malformed(error)),
    }
}

fn metadata_blocks_acquisition(
    metadata: &LockMetadata,
    hostname: &str,
    config: LockConfig,
    now: u64,
) -> bool {
    if metadata.hostname != hostname {
        now.saturating_sub(metadata.heartbeat_at_ms) <= config.cross_host_stale_heartbeat_ms()
    } else {
        lock_owner_is_alive(metadata)
    }
}

pub struct LockGuard {
    path: PathBuf,
    metadata: LockMetadata,
    shutdown: Arc<AtomicBool>,
    heartbeat_failed: Arc<AtomicBool>,
    heartbeat_done: mpsc::Receiver<()>,
    heartbeat: Option<JoinHandle<()>>,
    pump: HeartbeatPump,
    /// The heartbeat thread's handle on the lease, shared with it so the guard
    /// can close it before any replacement of the lease and so this process's
    /// lease index can tell other threads to wait for it.
    heartbeat_file: Arc<Mutex<Option<LeaseFile>>>,
}

impl LockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writer_epoch(&self) -> &str {
        &self.metadata.writer_epoch
    }

    /// Whether this guard's heartbeat is still running. `verify_writer_epoch`
    /// treats a guard whose heartbeat stopped as unverified, because nothing
    /// else would keep the lease's heartbeat fresh for other processes.
    pub(crate) fn heartbeat_is_visible(&self) -> bool {
        self.pump.is_visible()
    }

    /// Refresh this lease once through the guard's own handle, so a same-length
    /// beat costs one write and no descriptor open or close. Tests use this to
    /// drive beats from a single thread; runtime beats go through the pump.
    #[cfg(test)]
    fn heartbeat_once(&self) -> Result<(), HeartbeatError> {
        heartbeat_once(&self.path, &self.metadata, &self.heartbeat_file)
    }

    /// Re-read the lock file and confirm that this guard still owns the writer
    /// token. Writers call this right before saving published data or starting
    /// SQLite writes so they stop if another process has taken over the lock.
    pub fn verify_writer_epoch(&self) -> io::Result<bool> {
        if self.heartbeat_failed.load(Ordering::Acquire) || !self.heartbeat_is_visible() {
            return Ok(false);
        }
        match read_lock_metadata(&self.path) {
            Ok(metadata) => Ok(lock_identity_matches(&metadata, &self.metadata)),
            Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(ReadLockError::Io(error)) => Err(error),
            Err(ReadLockError::Malformed(_)) => Ok(false),
        }
    }
    /// Stop and join the heartbeat, closing its kept descriptor without
    /// unlinking the lease. Retention uses this immediately before moving or
    /// removing a cache tree: Windows cannot rename a directory containing an
    /// open lease file. The lease remains on disk and excludes other writers
    /// during the move; the guard must be dropped after the operation, even on
    /// error. Do not stop it early: remote hosts still rely on heartbeat age.
    pub(crate) fn stop_heartbeat_for_removal(&mut self) {
        // Signal shutdown then unconditionally join the heartbeat thread
        // BEFORE removing the lockfile. The earlier `recv_timeout(100ms)`
        // implementation could let `remove_lock_if_owned` race with a
        // still-alive heartbeat:
        //
        //   1. Drop signals shutdown, ack times out under CI load.
        //   2. Drop calls `remove_lock_if_owned` → file removed.
        //   3. Another caller acquires the lock → writes its metadata.
        //   4. Our heartbeat (still alive, mid-rewrite
        //      from before shutdown was checked) overwrites the new
        //      owner's file with our stale metadata. heartbeat_once's
        //      ownership check happens BEFORE the write, so it can race
        //      with a concurrent acquire that flips ownership in between.
        //   5. The new owner's heartbeat sees foreign metadata, exits
        //      `NotOwner`. The new owner's drop sees foreign metadata,
        //      `remove_lock_if_owned` returns `Ok(false)`, file persists.
        //
        // Always-joining bounds drop latency to one `park_timeout`
        // iteration (~25ms) plus the current `heartbeat_once` IO —
        // typically <500ms under CI load. The unused `heartbeat_done`
        // channel is kept for backward compatibility with any external
        // code that may still construct LockGuard manually, but Drop no
        // longer relies on it.
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.heartbeat.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
        // Drain any pending ack so the receiver doesn't carry stale state
        // if this LockGuard is somehow re-used (it isn't today, but be
        // defensive).
        while self.heartbeat_done.try_recv().is_ok() {}

        // Release the heartbeat handle before the lease file: closing any other
        // descriptor to it now would drop the advisory locks this process holds
        // on it, so the unlink and any concurrent replacement wait for this.
        let mut handle = self
            .heartbeat_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *handle = None;
        drop(handle);
        // Stop publishing the slot so a later replacement for this path never
        // waits on this guard's closed handle.
        unregister_lease_slot(self.pump.slot, &self.path);
        self.pump.visible.store(false, Ordering::Release);
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.stop_heartbeat_for_removal();
        match remove_lock_if_owned(&self.path, &self.metadata) {
            Ok(true) => slog_debug!("released filesystem lock at {}", self.path.display()),
            Ok(false) => {}
            Err(error) => slog_warn!(
                "failed to release filesystem lock at {}: {}",
                self.path.display(),
                error
            ),
        }
    }
}

/// Run `replace`, a replacement of a lease file, after releasing the heartbeat
/// handles this process publishes for leases.
///
/// Closing one descriptor to a file drops *all* the POSIX advisory locks this
/// process holds on it, including on a descriptor another thread still keeps.
/// A heartbeat holds its lease open between beats, so the process must not leave
/// that descriptor to be closed by a replacement: this takes each published
/// handle, drops the descriptor, and holds the handle's lock across `replace`.
/// Because the guards are taken before the lock is released, a beat cannot open
/// a lease into a slot this call is holding; a beat waits for the replacement
/// and then opens the live file.
fn with_lease_handles_closed<T>(
    path: &Path,
    replace: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    // Only this path's handle. Releasing every published handle would empty
    // another lease's slot, and its next beat would reopen the file, giving
    // back the per-beat descriptor churn the kept handle exists to avoid.
    let slot = lease_slots_by_path()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(path)
        .copied();
    let handle = slot.and_then(|slot| {
        lease_files()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&slot)
            .and_then(|entry| entry.handle.upgrade())
    });
    // Take the handle without waiting. A held lock means the heartbeat is
    // mid-beat on this lease (or the caller is the beat itself), and a beat that
    // already holds the descriptor keeps its own safety: it re-checks the path's
    // identity and the owner before every write. Waiting here would deadlock
    // against the very thread that has to finish the beat.
    let mut released = Vec::new();
    for handle in handle.iter() {
        let Ok(mut slot) = handle.try_lock() else {
            continue;
        };
        *slot = None;
        released.push(slot);
    }
    let result = replace();
    drop(released);
    result
}

#[derive(Debug)]
pub enum AcquireError {
    Io(io::Error),
    Timeout,
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AcquireError::Io(error) => write!(f, "filesystem lock I/O error: {error}"),
            AcquireError::Timeout => write!(f, "timed out acquiring filesystem lock"),
        }
    }
}

impl std::error::Error for AcquireError {}

impl From<io::Error> for AcquireError {
    fn from(error: io::Error) -> Self {
        AcquireError::Io(error)
    }
}

fn acquire_with_config(
    path: &Path,
    timeout: Option<Duration>,
    config: LockConfig,
) -> Result<LockGuard, AcquireError> {
    crate::production_storage::refuse_write(path)?;
    #[cfg(test)]
    retry_clock::before_acquire();
    let deadline = timeout.map(|timeout| acquisition_now() + timeout);
    acquire_with_deadline(
        path,
        deadline,
        timeout == Some(Duration::ZERO),
        false,
        config,
    )
}

fn acquire_with_deadline(
    path: &Path,
    deadline: Option<Instant>,
    allow_malformed_grace: bool,
    strict_deadline: bool,
    config: LockConfig,
) -> Result<LockGuard, AcquireError> {
    let hostname = current_hostname();
    // Report a live blocking owner once, not once per poll this acquisition
    // makes: a daemon with many roots re-attempts its own long-lived leases
    // constantly, and every attempt would otherwise repeat the same warning.
    let mut live_owner_log = LiveOwnerLog::new();
    let mut transient_create_failures: u32 = 0;
    let mut attempted_once = false;
    let mut dead_reclaim_blocked_attempts = 0_u32;
    // A zero-timeout acquire still gets one immediate retry after it removes a
    // stale lock; otherwise it would reap the dead owner and report Timeout.
    let mut immediate_retry_budget = 0_u8;
    let mut malformed_grace_used = false;

    loop {
        if attempted_once {
            if immediate_retry_budget > 0 && !strict_deadline {
                immediate_retry_budget -= 1;
            } else if let Some(deadline) = deadline {
                if acquisition_now() >= deadline {
                    return Err(AcquireError::Timeout);
                }
            }
        }
        attempted_once = true;
        #[cfg(test)]
        retry_clock::note_attempt();

        match create_new_lock(path, &hostname, config) {
            Ok(guard) => return Ok(guard),
            // The lock file already exists — fall through to inspect its owner.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            // Transient contention (chiefly Windows: a concurrent create/delete
            // on this exact path surfaces as access-denied/sharing-violation
            // rather than already-exists). Back off one poll interval and retry,
            // bounded so a persistent failure still propagates instead of
            // spinning forever.
            Err(error) if is_transient_create_contention(&error) => {
                transient_create_failures += 1;
                if transient_create_failures > MAX_TRANSIENT_CREATE_RETRIES {
                    return Err(error.into());
                }
                sleep_until_retry(deadline, config.poll_interval_ms)?;
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        transient_create_failures = 0;

        let observed = match observe_lock(path, &hostname, config) {
            Ok(observed) => observed,
            // The same Windows contention the create arm above tolerates can
            // land here instead: a lock file pending deletion is visible to
            // the create attempt but denies the open that reads it. Only the
            // create path recognised that spelling of "busy", so a race that
            // was meant to be retried propagated as a hard failure and
            // panicked a caller mid-write (Windows CI, 2026-09-22).
            Err(error) if is_transient_create_contention(&error) => {
                transient_create_failures += 1;
                if transient_create_failures > MAX_TRANSIENT_CREATE_RETRIES {
                    return Err(error.into());
                }
                sleep_until_retry(deadline, config.poll_interval_ms)?;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let availability = observed.availability();
        let metadata = match observed {
            ObservedLock::LiveOwner(metadata) | ObservedLock::Stale(metadata) => metadata,
            ObservedLock::Available => {
                immediate_retry_budget = 1;
                continue;
            }
            ObservedLock::Malformed(error) => {
                // A just-created O_EXCL file is visible before its owner has
                // finished writing JSON. Give that transient creation window
                // one poll interval before treating malformed contents as stale.
                // Zero-timeout callers also need this one grace read to reach
                // reclaim, but must not loop if another actor keeps corrupting
                // the lease or prevents its removal.
                if allow_malformed_grace && !malformed_grace_used {
                    malformed_grace_used = true;
                    sleep_until_retry(None, config.poll_interval_ms)?;
                } else {
                    sleep_until_retry(deadline, config.poll_interval_ms)?;
                }
                match read_lock_metadata(path) {
                    Ok(_) => continue,
                    Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                        continue;
                    }
                    Err(ReadLockError::Io(error)) => return Err(error.into()),
                    Err(ReadLockError::Malformed(_)) => {}
                }
                slog_warn!(
                    "removing malformed filesystem lock at {}: {}",
                    path.display(),
                    error
                );
                let judged = fs::read(path)?;
                if serde_json::from_slice::<LockMetadata>(&judged).is_ok() {
                    continue;
                }
                #[cfg(test)]
                run_acquisition_hook(&BEFORE_MALFORMED_RECLAIM);
                if reclaim_malformed_lock(path, &judged)? {
                    immediate_retry_budget = 1;
                }
                continue;
            }
        };

        let now = now_ms();
        let since_heartbeat = now.saturating_sub(metadata.heartbeat_at_ms);

        if metadata.hostname != hostname {
            dead_reclaim_blocked_attempts = 0;
            let cross_host_stale_ms = config.cross_host_stale_heartbeat_ms();
            if availability == LockAvailability::Stale {
                match reclaim_lock_file(path, &metadata)? {
                    ReclaimResult::Removed => {
                        slog_warn!(
                            "reclaimed cross-host filesystem lock at {} from host {} after stale heartbeat ({}ms > {}ms)",
                            path.display(),
                            metadata.hostname,
                            since_heartbeat,
                            cross_host_stale_ms
                        );
                        immediate_retry_budget = 1;
                    }
                    ReclaimResult::Blocked(holder) => {
                        log_reclaim_blocked(path, &holder);
                        sleep_until_retry(deadline, config.poll_interval_ms)?;
                    }
                    ReclaimResult::Unchanged => {
                        sleep_until_retry(deadline, config.poll_interval_ms)?;
                    }
                }
                continue;
            }
            sleep_until_retry(deadline, config.poll_interval_ms)?;
            continue;
        }

        if availability == LockAvailability::Stale {
            match reclaim_lock_file(path, &metadata)? {
                ReclaimResult::Removed => {
                    slog_warn!(
                        "removing filesystem lock at {} from dead or recycled PID {}",
                        path.display(),
                        metadata.pid
                    );
                    immediate_retry_budget = 1;
                    dead_reclaim_blocked_attempts = 0;
                }
                ReclaimResult::Blocked(holder) => {
                    log_reclaim_blocked(path, &holder);
                    let backoff_ms = dead_reclaim_backoff_ms(
                        dead_reclaim_blocked_attempts,
                        config.poll_interval_ms,
                    );
                    dead_reclaim_blocked_attempts = dead_reclaim_blocked_attempts.saturating_add(1);
                    sleep_until_retry(deadline, backoff_ms)?;
                }
                ReclaimResult::Unchanged => {
                    dead_reclaim_blocked_attempts = 0;
                    sleep_until_retry(deadline, config.poll_interval_ms)?;
                }
            }
            continue;
        }
        dead_reclaim_blocked_attempts = 0;

        if since_heartbeat > config.stale_heartbeat_ms && live_owner_log.first_stale() {
            // Same-host PID plus start-time identity is authoritative. A
            // SIGSTOP'd process, suspended VM, or sleeping laptop can miss
            // heartbeats and later resume inside the critical section. Breaking
            // that lock would allow split-brain writers, so a paused matching
            // owner blocks acquirers until it resumes and releases the lock or
            // dies. PID namespaces restart numbering after hard-killed owners,
            // which turns this safety rule into a deadlock unless the start time
            // distinguishes the unrelated process that reused the PID.
            slog_warn!(
                "filesystem lock at {} held by live PID {} has stale heartbeat ({}ms); NOT breaking",
                path.display(),
                metadata.pid,
                since_heartbeat
            );
        }

        let held_for = now.saturating_sub(metadata.created_at_ms);
        if held_for > config.live_owner_warn_ms && live_owner_log.first_long_held() {
            if metadata.pid == std::process::id() {
                // A lease this process holds and keeps heartbeating is a
                // long-lived writer (an inspect or callgraph writer lease lives
                // as long as its cache), so nothing is wrong and nothing is at
                // risk. Report it at debug level so the acquisition trace still
                // shows why it is waiting.
                slog_debug!(
                    "filesystem lock at {} is held by a live lease of this process for >10min; \
                     waiting without breaking it",
                    path.display()
                );
            } else {
                slog_warn!(
                    "filesystem lock at {} held >10min by live heartbeating PID {}; NOT breaking",
                    path.display(),
                    metadata.pid
                );
            }
        }

        sleep_until_retry(deadline, config.poll_interval_ms)?;
    }
}

fn create_new_lock(path: &Path, hostname: &str, config: LockConfig) -> io::Result<LockGuard> {
    let now = now_ms();
    let pid = std::process::id();
    let process_identity = process_identity(pid);
    let metadata = LockMetadata {
        pid,
        hostname: hostname.to_string(),
        process_start_time: process_identity
            .as_ref()
            .map(|identity| identity.start_time),
        boot_id: process_identity.and_then(|identity| identity.boot_id),
        created_at_ms: now,
        heartbeat_at_ms: now,
        writer_epoch: format!("{pid}-{}", now_nanos()),
    };

    create_lock_file_atomically(path, &metadata)?;

    let (heartbeat_pump, heartbeat_file) =
        HeartbeatPump::attach(path, &metadata).inspect_err(|_| {
            let _ = remove_lock_if_owned(path, &metadata);
        })?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let heartbeat_failed = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    let heartbeat_path = path.to_path_buf();
    let heartbeat_shutdown = Arc::clone(&shutdown);
    let heartbeat_failed_for_thread = Arc::clone(&heartbeat_failed);
    let heartbeat_pump_for_thread = heartbeat_pump.clone();
    let heartbeat = spawn_heartbeat(move || {
        let heartbeat_shutdown_for_run = Arc::clone(&heartbeat_shutdown);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_heartbeat(
                heartbeat_path,
                &heartbeat_pump_for_thread,
                heartbeat_shutdown_for_run,
                config,
            );
        }));
        if result.is_err() || !heartbeat_shutdown.load(Ordering::Acquire) {
            heartbeat_failed_for_thread.store(true, Ordering::Release);
        }
        let _ = done_tx.send(());
    })
    .inspect_err(|_| {
        // Thread creation failed after publication; do not strand this PID's
        // lease, or unlink a replacement that no longer has our epoch.
        let _ = remove_lock_if_owned(path, &metadata);
    })?;

    slog_debug!("acquired filesystem lock at {}", path.display());
    #[cfg(test)]
    io_ledger::record(|ledger| ledger.acquisitions += 1);

    Ok(LockGuard {
        path: path.to_path_buf(),
        metadata,
        shutdown,
        heartbeat_failed,
        heartbeat_done: done_rx,
        heartbeat: Some(heartbeat),
        pump: heartbeat_pump,
        heartbeat_file,
    })
}

#[cfg(test)]
thread_local! {
    static BEFORE_MALFORMED_RECLAIM: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_HEARTBEAT_SPAWN_FAILURE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_acquisition_hook(
    hook: &'static std::thread::LocalKey<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>,
) -> bool {
    let action = hook.with(|slot| slot.borrow_mut().take());
    if let Some(action) = action {
        action();
        true
    } else {
        false
    }
}

fn spawn_heartbeat(run: impl FnOnce() + Send + 'static) -> io::Result<thread::JoinHandle<()>> {
    #[cfg(test)]
    if run_acquisition_hook(&BEFORE_HEARTBEAT_SPAWN_FAILURE) {
        return Err(io::Error::other("injected heartbeat spawn failure"));
    }
    thread::Builder::new()
        .name("aft-fs-lock-heartbeat".to_string())
        .spawn(run)
}

fn run_heartbeat(
    path: PathBuf,
    pump: &HeartbeatPump,
    shutdown: Arc<AtomicBool>,
    config: LockConfig,
) {
    // Number of consecutive heartbeat intervals that can be missed before the
    // same-host stale window elapses and another process may reclaim the lock.
    // Beyond this point a sustained failure is genuinely dangerous, so we
    // escalate the log from warn to error — but we still keep retrying.
    let stale_intervals = config
        .stale_heartbeat_ms
        .checked_div(config.heartbeat_interval_ms.max(1))
        .unwrap_or(3)
        .max(1);
    let mut consecutive_transient_failures: u64 = 0;

    loop {
        thread::park_timeout(Duration::from_millis(config.heartbeat_interval_ms));
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        match pump.heartbeat_once() {
            Ok(()) => {
                if consecutive_transient_failures > 0 {
                    slog_info!(
                        "filesystem lock at {} heartbeat recovered after {} transient failure(s)",
                        path.display(),
                        consecutive_transient_failures
                    );
                    consecutive_transient_failures = 0;
                }
            }
            Err(error) if heartbeat_error_is_terminal(&error) => {
                // Terminal states: the lock is provably gone or owned by
                // someone else. Continuing to write would clobber a new owner's
                // metadata (the exact race documented in LockGuard::drop), so
                // stop heartbeating.
                #[cfg(test)]
                pump.mark_stopped();
                slog_error!(
                    "{}; stopping heartbeat",
                    terminal_heartbeat_message(&path, &error)
                );
                return;
            }
            Err(error) => {
                // Transient states: a temporary I/O hiccup (disk/NFS blip,
                // quota) or a read that raced a concurrent writer mid-write
                // (momentarily unparseable file). A single such error must NOT
                // permanently kill the heartbeat — that would silently stop
                // refreshing heartbeat_at_ms while the guard holder keeps
                // running its critical section, letting another process reclaim
                // the lock after the stale window and produce concurrent
                // writers. Log and retry on the next interval; a later success
                // resumes heartbeating automatically.
                consecutive_transient_failures += 1;
                log_transient_heartbeat_failure(
                    &path,
                    &transient_heartbeat_reason(&error),
                    consecutive_transient_failures,
                    stale_intervals,
                );
            }
        }
    }
}

/// A heartbeat failure is terminal when the lock is provably no longer ours to
/// refresh: it was removed (`LockGone`) or a different owner now holds it
/// (`NotOwner`). I/O and malformed-read failures are treated as transient —
/// they are typically temporary disk/NFS hiccups or a read that raced a
/// concurrent writer — so the heartbeat retries rather than dying.
fn heartbeat_error_is_terminal(error: &HeartbeatError) -> bool {
    matches!(
        error,
        HeartbeatError::LockGone | HeartbeatError::NotOwner | HeartbeatError::NotPumped
    )
}

fn terminal_heartbeat_message(path: &Path, error: &HeartbeatError) -> String {
    match error {
        HeartbeatError::LockGone => {
            format!("filesystem lock at {} disappeared", path.display())
        }
        HeartbeatError::NotOwner => format!(
            "filesystem lock at {} is no longer owned by this guard",
            path.display()
        ),
        HeartbeatError::NotPumped => format!(
            "filesystem lock at {} heartbeat handle was released by this process",
            path.display()
        ),
        // Not reachable for non-terminal errors, but keep a sensible string.
        HeartbeatError::Io(error) => {
            format!("filesystem lock at {} I/O error: {error}", path.display())
        }
        HeartbeatError::Malformed(error) => {
            format!(
                "filesystem lock at {} became malformed: {error}",
                path.display()
            )
        }
    }
}

fn transient_heartbeat_reason(error: &HeartbeatError) -> String {
    match error {
        HeartbeatError::Io(error) => format!("I/O error: {error}"),
        HeartbeatError::Malformed(error) => format!("became malformed: {error}"),
        HeartbeatError::LockGone => "lock disappeared".to_string(),
        HeartbeatError::NotOwner => "lock no longer owned".to_string(),
        HeartbeatError::NotPumped => "heartbeat handle released".to_string(),
    }
}

/// Log a transient heartbeat failure, escalating to error exactly once when the
/// failures have lasted long enough that the lock is now reclaimable by another
/// owner. Beyond that point we stay quiet to avoid log spam while still
/// retrying — the holder has already been warned the lock is at risk.
fn log_transient_heartbeat_failure(
    path: &Path,
    reason: &str,
    consecutive_failures: u64,
    stale_intervals: u64,
) {
    if consecutive_failures < stale_intervals {
        slog_warn!(
            "transient failure to heartbeat filesystem lock at {}: {}; retrying (attempt {})",
            path.display(),
            reason,
            consecutive_failures
        );
    } else if consecutive_failures == stale_intervals {
        slog_error!(
            "filesystem lock at {} has failed {} consecutive heartbeats: {}; \
             the lock may now be reclaimed by another owner — continuing to retry",
            path.display(),
            consecutive_failures,
            reason
        );
    }
}

/// The heartbeat thread's handle on one lease.
///
/// The thread keeps the lease open between beats so a beat costs one write
/// instead of an open, a read and a close. A `File` released on its own thread
/// closes there, and on macOS the close is where the lease's write-back runs,
/// so per-beat close/open churn put the write-back cost on the heartbeat
/// thread. Three rules bound how long the handle lives:
///
/// * Another thread of this process that has to replace the lease needs every
///   handle to it released first (`LEASE_FILES`), because closing any
///   descriptor to a file drops the advisory locks this process holds on it.
///   That is why the replacement paths close before they rename.
/// * A beat never trusts the handle blindly. Before reading it compares the
///   file the handle refers to with the file the path now names (device and
///   inode, or on Windows volume serial number and file index). Unix reads both
///   with a stat; Windows reads the descriptor's identity from its own handle
///   and opens the path for attributes only, so that arm does take one
///   short-lived handle per beat. A different file retires the old handle. If
///   the path still carries this guard's owner identity, the beat reopens it
///   immediately; a different owner stops the heartbeat instead of refreshing
///   the orphan the handle still holds.
///   When the handle was released by a replacement, the next beat opens the path
///   again. Releases happen on the heartbeat thread, or on the thread running a
///   replacement through `with_lease_handles_closed`.
/// * Failing that check, the bytes read through the handle must still carry this
///   guard's identity, and a path that is gone is reported as `LockGone`. A
///   guard whose heartbeat stopped reports the lease as unverified; see
///   `heartbeat_is_visible`.
#[derive(Debug)]
struct HeartbeatPump {
    path: PathBuf,
    slot: usize,
    /// Full owner identity, recorded at attach. The path can later hold a
    /// different owner: a reclaim by any process mints a new epoch, so a path
    /// whose identity no longer matches was released and re-indexed for its new
    /// owner. A reclaim happens when the owner is judged dead, or when another
    /// host's heartbeat went stale beyond its extended window.
    owner: Mutex<LockMetadata>,
    visible: AtomicBool,
    /// Set when the heartbeat ends for a reason other than the guard closing.
    /// Tests read it to tell "a later beat failed" from "the beat was still
    /// retrying", which a file timestamp cannot show.
    #[cfg(test)]
    stopped: Arc<AtomicBool>,
}

fn lease_files() -> &'static Mutex<HashMap<usize, LeaseSlot>> {
    LEASE_FILES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lease_slots_by_path() -> &'static Mutex<HashMap<PathBuf, usize>> {
    LEASE_SLOT_BY_PATH.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a handle under its slot and its path. A path holds one heartbeat's
/// handle, so a later registration for the same path supersedes the earlier
/// entry; the old handle belongs to a guard that has released or lost the lease.
fn register_lease_slot(slot: usize, path: &Path, handle: &Arc<Mutex<Option<LeaseFile>>>) {
    lease_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            slot,
            LeaseSlot {
                handle: Arc::downgrade(handle),
            },
        );
    lease_slots_by_path()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path.to_path_buf(), slot);
}

/// Forget a slot and, if it still owns the path entry, that entry too.
fn unregister_lease_slot(slot: usize, path: &Path) {
    lease_files()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&slot);
    let mut by_path = lease_slots_by_path()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if by_path.get(path).copied() == Some(slot) {
        by_path.remove(path);
    }
}

impl Clone for HeartbeatPump {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            slot: self.slot,
            owner: Mutex::new(
                self.owner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            ),
            visible: AtomicBool::new(self.is_visible()),
            #[cfg(test)]
            stopped: Arc::clone(&self.stopped),
        }
    }
}

impl HeartbeatPump {
    /// Register a heartbeat handle for a newly created lease and publish it in
    /// this process's lease index.
    fn attach(
        path: &Path,
        owner: &LockMetadata,
    ) -> io::Result<(Self, Arc<Mutex<Option<LeaseFile>>>)> {
        let file = Arc::new(Mutex::new(Some(
            open_lease_for_heartbeat(path).map_err(heartbeat_io_error)?,
        )));
        let slot = NEXT_LEASE_SLOT.fetch_add(1, Ordering::Relaxed) as usize;
        {
            let mut registry = lease_files()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if registry.len() > 4096 {
                registry.retain(|_, known| known.handle.strong_count() > 0);
            }
        }
        register_lease_slot(slot, path, &file);
        Ok((
            Self {
                path: path.to_path_buf(),
                slot,
                owner: Mutex::new(owner.clone()),
                visible: AtomicBool::new(true),
                #[cfg(test)]
                stopped: Arc::new(AtomicBool::new(false)),
            },
            file,
        ))
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Whether the heartbeat thread ended by itself instead of at guard drop.
    #[cfg(test)]
    pub(super) fn heartbeat_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// One beat: read the lease through the kept handle, refresh the timestamp,
    /// and write it back through the same handle, opening the lease only when
    /// no handle is kept.
    fn heartbeat_once(&self) -> Result<(), HeartbeatError> {
        if !self.is_visible() {
            return Err(HeartbeatError::NotPumped);
        }
        let mut reopened = false;
        loop {
            let handle = {
                let registry = lease_files()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                registry
                    .get(&self.slot)
                    .and_then(|entry| entry.handle.upgrade())
            };
            let Some(handle) = handle else {
                return Err(HeartbeatError::NotPumped);
            };
            match self.beat(&handle) {
                Ok(()) => return Ok(()),
                // The lease on disk is not the one this handle was opened for.
                // If it is no longer this process's lease, stop instead of
                // writing over another owner's heartbeat; otherwise the path
                // was replaced under the handle (a plain rewrite, or a stale
                // rename), so the next attempt reads the live file.
                Err(HeartbeatError::NotOwner) => {
                    self.retire_handle(&handle);
                    if self.release_if_reclaimed() {
                        return Err(HeartbeatError::NotOwner);
                    }
                    // A same-owner replacement is already safe to reopen. An
                    // extra heartbeat-interval sleep here leaves the live path
                    // stale even though the inode change was detected. Retry
                    // once now; repeated replacements remain transient and are
                    // retried by the normal heartbeat loop without spinning.
                    if reopened {
                        return Err(HeartbeatError::Io(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "lease replaced repeatedly during heartbeat",
                        )));
                    }
                    reopened = true;
                }
                // A partial rewrite does not prove ownership was lost. Retire
                // the descriptor so the next scheduled beat opens the live
                // path, including when recovery replaces the inode atomically.
                // Let the heartbeat loop schedule that retry: waiting here can
                // turn a transient read into a terminal NotOwner timeout.
                Err(error @ HeartbeatError::Malformed(_)) => {
                    self.retire_handle(&handle);
                    return Err(error);
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn beat(&self, handle: &Arc<Mutex<Option<LeaseFile>>>) -> Result<(), HeartbeatError> {
        let owner = self
            .owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let needs_open = handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none();
        if needs_open {
            // Publish the reopened descriptor under the slot before the beat:
            // the handle another thread's replacement waits for must always be
            // the one this process holds. Opening inside the beat would leave a
            // window where a replacement renames the lease the beat is not
            // looking at.
            let file = open_lease_for_heartbeat(&self.path)?;
            let mut slot = handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_none() {
                *slot = Some(file);
            }
        }
        heartbeat_once(&self.path, &owner, handle)
    }

    /// Release the lease descriptor but keep the slot registered, so the next
    /// beat reopens the lease into it. Dropping the slot instead would leave the
    /// heartbeat with no handle to find, and it would stop.
    fn retire_handle(&self, handle: &Arc<Mutex<Option<LeaseFile>>>) {
        let mut slot = handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
    }

    fn release_if_reclaimed(&self) -> bool {
        match read_lock_metadata(&self.path) {
            Err(ReadLockError::Malformed(_)) => false,
            Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => true,
            Err(ReadLockError::Io(_)) => false,
            Ok(on_disk) => {
                let owner = self
                    .owner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                !lock_identity_matches(&on_disk, &owner)
            }
        }
    }
}

/// Refresh `heartbeat_at_ms` in a lease this guard owns.
///
/// Durability: a heartbeat only has to be visible to other processes, so this
/// path never fsyncs the file or its directory. If the machine crashes before
/// the page cache is written back, the lease on disk just carries an older
/// heartbeat and looks stale sooner, which the stale-lease rules already
/// handle. Creation and reclaim tokens also need only visibility: ownership
/// depends on a live process identity, which cannot survive a machine crash.
///
/// The rewrite happens in place on `handle`, whose contents were just checked
/// for ownership: no new inode per beat, and no window in which the path is
/// missing (which other code reads as `LockGone`). Serialization is
/// deterministic, so only the digits of `heartbeat_at_ms` differ and the file
/// keeps its length; overwriting the same length from offset zero never leaves
/// a truncated or zero-length file. If the length would change (the timestamp
/// gained a digit), fall back to an unsynced temp-file rename, which still
/// replaces the path atomically.
///
/// `handle` is empty for a beat that has no kept descriptor; that beat opens
/// the lease for itself and releases it when it returns. A heartbeat thread
/// passes the handle it keeps, so a same-length beat opens and closes nothing;
/// a length change still writes a temp file and renames it.
fn heartbeat_once(
    path: &Path,
    owner: &LockMetadata,
    handle: &Mutex<Option<LeaseFile>>,
) -> Result<(), HeartbeatError> {
    let mut slot = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // A kept descriptor only speaks for the lease while it is still the file
    // the path names. Another process's reclaim renames its own lease over the
    // path, which leaves our descriptor holding the old, unlinked file: reading
    // it would show our own identity, the beat would refresh the orphan and
    // report success, and the live lease would silently go stale. On Unix both
    // identities come from a stat, so this costs no descriptor there.
    if let Some(kept) = slot.as_mut() {
        let on_path = match LeaseFileIdentity::of_path(path) {
            Ok(identity) => identity,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(HeartbeatError::LockGone);
            }
            Err(error) => return Err(HeartbeatError::Io(error)),
        };
        // A stat that cannot read the descriptor's identity is the only reason
        // this comparison is unavailable, and it may be the same failure that
        // hides a replacement. Retire the handle instead of trusting it: the
        // read below then opens the live file and compares the bytes, which is
        // the behaviour before the descriptor was kept between beats.
        match LeaseFileIdentity::of_file(kept.file()) {
            Some(held) if held != on_path => return Err(HeartbeatError::NotOwner),
            Some(_) => {}
            None => *slot = None,
        }
    }
    let mut opened = None;
    let file = match slot.as_mut() {
        Some(kept) => kept.file(),
        None => opened.insert(open_lease_for_heartbeat(path)?).file(),
    };
    file.seek(SeekFrom::Start(0)).map_err(HeartbeatError::Io)?;
    let mut current = Vec::new();
    file.read_to_end(&mut current).map_err(HeartbeatError::Io)?;
    let mut metadata: LockMetadata =
        serde_json::from_slice(&current).map_err(HeartbeatError::Malformed)?;

    if !lock_identity_matches(&metadata, owner) {
        return Err(HeartbeatError::NotOwner);
    }

    metadata.heartbeat_at_ms = now_ms();
    let next = lock_metadata_bytes(&metadata).map_err(HeartbeatError::Io)?;
    if next.len() == current.len() {
        file.seek(SeekFrom::Start(0)).map_err(HeartbeatError::Io)?;
        return write_lock_bytes(file, &next).map_err(HeartbeatError::Io);
    }
    // Close before replacing the path: a handle stays usable for later beats
    // only while it refers to the lease at this path. The replacement itself
    // also releases any handle this process publishes for the path.
    *slot = None;
    drop(opened);
    replace_lock_bytes_unsynced(path, &next).map_err(HeartbeatError::Io)
}

/// What identifies one lease file on disk, independent of the name pointing at
/// it: device and inode on Unix, volume serial number and file index on
/// Windows. A rename gives the path a different file, so comparing the file a
/// heartbeat holds with the file the path now names tells whether the lease was
/// replaced under the kept descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LeaseFileIdentity {
    volume: u64,
    file: u64,
}

impl LeaseFileIdentity {
    #[cfg(unix)]
    fn of_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            volume: metadata.dev(),
            file: metadata.ino(),
        }
    }

    /// Volume serial number and 64-bit file index from a handle. On Windows
    /// that pair is what an inode is on Unix. Creation time would not do: NTFS
    /// file-name tunneling gives a name reused within about 15 s the creation
    /// time of the entry that last had it, so a replaced lease could look
    /// unchanged.
    #[cfg(windows)]
    fn from_raw_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<Self> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value (plain
        // integers and FILETIME structs); the call only writes into it.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the caller owns `handle` and keeps it alive across this call,
        // and `info` is a valid out pointer.
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
        ok.then(|| Self {
            volume: u64::from(info.dwVolumeSerialNumber),
            file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }

    /// The identity of the file behind an open descriptor, read from the
    /// descriptor's own handle: nothing is opened.
    #[cfg(unix)]
    fn of_file(file: &File) -> Option<Self> {
        file.metadata()
            .ok()
            .map(|metadata| Self::of_metadata(&metadata))
    }

    #[cfg(windows)]
    fn of_file(file: &File) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        Self::from_raw_handle(file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE)
    }

    /// The identity of the file the path names now. On Unix this is a stat and
    /// opens nothing. Windows has no handle-free file index, so that arm opens
    /// the path for attributes only, sharing every access a heartbeat holds, and
    /// closes it before returning.
    #[cfg(unix)]
    fn of_path(path: &Path) -> io::Result<Self> {
        fs::metadata(path).map(|metadata| Self::of_metadata(&metadata))
    }

    #[cfg(windows)]
    fn of_path(path: &Path) -> io::Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        /// Closes the attribute handle on every path out of this function.
        struct Closed(windows_sys::Win32::Foundation::HANDLE);
        impl Drop for Closed {
            fn drop(&mut self) {
                // SAFETY: this wrapper is only ever built from the handle
                // `CreateFileW` returned after it was checked against
                // `INVALID_HANDLE_VALUE`, and it is dropped exactly once.
                unsafe { CloseHandle(self.0) };
            }
        }

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
        // the remaining arguments are plain flags and null pointers.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = Closed(handle);
        Self::from_raw_handle(handle.0)
            .ok_or_else(|| io::Error::other("lease path has no file identity"))
    }

    #[cfg(not(any(unix, windows)))]
    fn of_file(_file: &File) -> Option<Self> {
        None
    }

    #[cfg(not(any(unix, windows)))]
    fn of_path(_path: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no file identity on this platform",
        ))
    }
}

fn open_lease_for_heartbeat(path: &Path) -> Result<LeaseFile, HeartbeatError> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(HeartbeatError::LockGone);
        }
        Err(error) => return Err(HeartbeatError::Io(error)),
    };
    io_ledger::record(|ledger| ledger.opens += 1);
    Ok(LeaseFile(file))
}

/// A descriptor the heartbeat opened, so its release is counted wherever it
/// happens: a beat that drops it, `LockGuard::drop`, or a lease replacement
/// that releases the published handle.
#[derive(Debug)]
struct LeaseFile(File);

impl LeaseFile {
    fn file(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for LeaseFile {
    fn drop(&mut self) {
        io_ledger::record(|ledger| ledger.closes += 1);
    }
}

fn heartbeat_io_error(error: HeartbeatError) -> io::Error {
    match error {
        HeartbeatError::Io(error) => error,
        other => io::Error::other(format!("{other:?}")),
    }
}

#[derive(Debug)]
enum HeartbeatError {
    Io(io::Error),
    LockGone,
    Malformed(serde_json::Error),
    NotOwner,
    NotPumped,
}

#[derive(Debug)]
enum ReadLockError {
    Io(io::Error),
    Malformed(serde_json::Error),
}

/// Read a lease file, re-reading until two consecutive reads agree.
///
/// Heartbeats overwrite lease files in place, and a read that overlaps such a
/// write is not guaranteed to be atomic on every filesystem: it could mix old
/// and new digits of the timestamp. The contents are otherwise identical, so a
/// mixed read still parses, but the timestamp could look older than either
/// write. Two identical reads in a row rule that out. The retry is bounded;
/// after it the last read is returned as-is.
pub(crate) fn read_lease_settled(path: &Path) -> io::Result<Vec<u8>> {
    let mut previous = fs::read(path)?;
    for _ in 0..3 {
        let next = fs::read(path)?;
        if next == previous {
            return Ok(next);
        }
        previous = next;
    }
    Ok(previous)
}

/// Parse a lease. An empty or partial file (for example one left by a crash
/// after an unsynced rename) is `Malformed`, which acquirers treat as a stale
/// lease to remove, never as a live owner.
fn read_lock_metadata(path: &Path) -> Result<LockMetadata, ReadLockError> {
    let bytes = read_lease_settled(path).map_err(ReadLockError::Io)?;
    serde_json::from_slice(&bytes).map_err(ReadLockError::Malformed)
}

fn open_new_lock_file(path: &Path) -> io::Result<File> {
    let file = crate::private_storage::options()
        .write(true)
        .create_new(true)
        .open(path)?;
    io_ledger::record(|ledger| ledger.new_files += 1);
    Ok(file)
}

fn lock_metadata_bytes(metadata: &LockMetadata) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(metadata).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_lock_bytes(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    io_ledger::record(|ledger| ledger.bytes_written += bytes.len() as u64);
    Ok(())
}

fn write_lock_metadata_to_file(file: &mut File, metadata: &LockMetadata) -> io::Result<()> {
    write_lock_bytes(file, &lock_metadata_bytes(metadata)?)
}

fn create_lock_file_atomically(path: &Path, metadata: &LockMetadata) -> io::Result<()> {
    let tmp_path = temp_path_for_lock(path);
    let result = (|| {
        let mut file = open_new_lock_file(&tmp_path)?;
        write_lock_metadata_to_file(&mut file, metadata)?;
        drop(file);

        fs::hard_link(&tmp_path, path)?;
        Ok(())
    })();

    let _ = fs::remove_file(&tmp_path);
    result
}

/// Replace a lease's contents through a temp file and an atomic rename,
/// without fsync. Only the heartbeat uses this, and only when the in-place
/// rewrite cannot keep the file length; see `heartbeat_once` for why a
/// heartbeat needs no durability.
fn replace_lock_bytes_unsynced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp_path = temp_path_for_lock(path);
    let write_result = (|| {
        let mut file = open_new_lock_file(&tmp_path)?;
        write_lock_bytes(&mut file, bytes)?;
        drop(file);
        with_lease_handles_closed(path, || rename_over(&tmp_path, path))
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }

    write_result
}

#[cfg(any(windows, test))]
fn rename_over_with(
    from: &Path,
    to: &Path,
    replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    replace(from, to)
}

#[cfg(windows)]
pub(crate) fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    // MoveFileExW with MOVEFILE_REPLACE_EXISTING is the only replacement path
    // here that preserves the old destination on failure. A copy fallback
    // truncates the destination before copying and can expose partial bytes or
    // destroy the last valid artifact if the copy fails midway. Callers retain
    // their temp file cleanup and retry policy when an open handle prevents the
    // atomic replacement.
    // Closure instead of the bare `fs::rename` fn item: the generic fn item
    // instantiates with concrete reference lifetimes and fails the
    // higher-ranked `FnOnce(&Path, &Path)` bound on some targets.
    rename_over_with(from, to, |from, to| fs::rename(from, to))
}

#[cfg(not(windows))]
pub(crate) fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

// Per-thread counter that disambiguates temp lockfile paths for callers
// inside the same process. `now_nanos()` alone is not unique enough on
// Windows when two threads race to acquire the same lock (caught by the
// `acquire_serializes_concurrent_callers` test): two threads sampling the
// nanosecond clock within the same scheduler quantum produce identical
// timestamps, both write to the same `.lock.tmp.<pid>.<nanos>` file, one
// thread's `fs::remove_file(&tmp_path)` cleanup deletes the file before
// the other thread's `fs::hard_link(&tmp_path, ...)` runs, and the loser
// panics with `Io(Os { code: 2, NotFound })`.
//
// `AtomicU64` shared across threads makes every temp path unique within
// the process regardless of clock resolution or scheduling races.
static TEMP_LOCK_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path_for_lock(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lock");
    let seq = TEMP_LOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}.{}",
        std::process::id(),
        now_nanos(),
        seq
    ))
}

fn lock_identity_matches(left: &LockMetadata, right: &LockMetadata) -> bool {
    left.pid == right.pid
        && left.hostname == right.hostname
        && left.process_start_time == right.process_start_time
        && left.boot_id == right.boot_id
        && left.created_at_ms == right.created_at_ms
        && left.writer_epoch == right.writer_epoch
}

fn remove_lock_if_owned(path: &Path, owner: &LockMetadata) -> io::Result<bool> {
    let metadata = match read_lock_metadata(path) {
        Ok(metadata) => metadata,
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(ReadLockError::Io(error)) => return Err(error),
        Err(ReadLockError::Malformed(_)) => return Ok(false),
    };

    if lock_identity_matches(&metadata, owner) {
        remove_lock_file(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn remove_lock_file(path: &Path) -> io::Result<()> {
    match with_lease_handles_closed(path, || fs::remove_file(path)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Reclaim (delete) a lock file we judged stale/dead, but ONLY if it still holds
/// the SAME owner identity we evaluated. Between reading the metadata and
/// deleting, the stale owner could release and a FRESH owner acquire — blindly
/// `remove_file` would then delete the fresh owner's lock, allowing split-brain
/// writers. Re-read immediately before the unlink and bail if the full owner
/// identity changed or the file vanished. POSIX has no atomic compare-and-unlink,
/// so a microscopic residual race remains, but this shrinks the window from the
/// whole judgment/poll duration to a couple of syscalls — the standard mitigation.
fn reclaim_lock_file(path: &Path, judged: &LockMetadata) -> io::Result<ReclaimResult> {
    let token = match acquire_reclaim_token(path)? {
        ReclaimTokenAcquire::Acquired(token) => token,
        ReclaimTokenAcquire::Held(holder) => return Ok(ReclaimResult::Blocked(holder)),
    };
    let _token = token;
    match read_lock_metadata(path) {
        Ok(current) => {
            if lock_identity_matches(&current, judged) {
                remove_lock_file(path)?;
                Ok(ReclaimResult::Removed)
            } else {
                // A different owner acquired it in the gap — do NOT delete.
                Ok(ReclaimResult::Unchanged)
            }
        }
        // Already gone (released/reclaimed by someone else) — nothing to do.
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Ok(ReclaimResult::Unchanged)
        }
        // Malformed now (mid-write by a new owner) — don't delete; retry next poll.
        Err(ReadLockError::Malformed(_)) => Ok(ReclaimResult::Unchanged),
        Err(ReadLockError::Io(error)) => Err(error),
    }
}

fn reclaim_malformed_lock(path: &Path, judged: &[u8]) -> io::Result<bool> {
    let _token = match acquire_reclaim_token(path)? {
        ReclaimTokenAcquire::Acquired(token) => token,
        ReclaimTokenAcquire::Held(_) => return Ok(false),
    };
    match fs::read(path) {
        Ok(current)
            if current == judged && serde_json::from_slice::<LockMetadata>(&current).is_err() =>
        {
            remove_lock_file(path)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

struct ReclaimTokenGuard {
    path: PathBuf,
}

impl Drop for ReclaimTokenGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_reclaim_token(lock_path: &Path) -> io::Result<ReclaimTokenAcquire> {
    let token_path = reclaim_token_path(lock_path);
    let metadata = current_reclaim_token_metadata();

    match create_reclaim_token(&token_path, &metadata) {
        Ok(guard) => return Ok(ReclaimTokenAcquire::Acquired(guard)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let stale = match inspect_reclaim_token(&token_path)? {
        ExistingReclaimToken::Held(holder) => return Ok(ReclaimTokenAcquire::Held(holder)),
        ExistingReclaimToken::StaleValid(owner) => remove_lock_if_owned(&token_path, &owner)?,
        ExistingReclaimToken::StaleMalformed => remove_malformed_reclaim_token(&token_path)?,
        ExistingReclaimToken::Missing => true,
    };
    if !stale {
        return inspect_reclaim_token_as_held(&token_path);
    }

    // A stale-token takeover gets exactly one O_EXCL retry. Another contender may
    // win after the unlink; never loop and accidentally turn reclamation into a
    // second lock-acquisition spin.
    match create_reclaim_token(&token_path, &metadata) {
        Ok(guard) => Ok(ReclaimTokenAcquire::Acquired(guard)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            inspect_reclaim_token_as_held(&token_path)
        }
        Err(error) => Err(error),
    }
}

enum ReclaimTokenAcquire {
    Acquired(ReclaimTokenGuard),
    Held(ReclaimTokenHeld),
}

enum ExistingReclaimToken {
    Held(ReclaimTokenHeld),
    StaleValid(LockMetadata),
    StaleMalformed,
    Missing,
}

fn current_reclaim_token_metadata() -> LockMetadata {
    let pid = std::process::id();
    let process_identity = process_identity(pid);
    let now = now_ms();
    LockMetadata {
        pid,
        hostname: current_hostname(),
        process_start_time: process_identity
            .as_ref()
            .map(|identity| identity.start_time),
        boot_id: process_identity.and_then(|identity| identity.boot_id),
        created_at_ms: now,
        heartbeat_at_ms: now,
        writer_epoch: format!("reclaim-{pid}-{}", now_nanos()),
    }
}

fn create_reclaim_token(
    token_path: &Path,
    metadata: &LockMetadata,
) -> io::Result<ReclaimTokenGuard> {
    let mut file = open_new_lock_file(token_path)?;
    if let Err(error) = write_lock_metadata_to_file(&mut file, metadata) {
        let _ = fs::remove_file(token_path);
        return Err(error);
    }
    Ok(ReclaimTokenGuard {
        path: token_path.to_path_buf(),
    })
}

fn inspect_reclaim_token(token_path: &Path) -> io::Result<ExistingReclaimToken> {
    match read_lock_metadata(token_path) {
        Ok(metadata) if metadata.hostname != current_hostname() => {
            Ok(ExistingReclaimToken::Held(ReclaimTokenHeld {
                pid: Some(metadata.pid),
                state: ReclaimTokenState::DeadForeignHost,
            }))
        }
        Ok(metadata) if lock_owner_is_alive(&metadata) => {
            Ok(ExistingReclaimToken::Held(ReclaimTokenHeld {
                pid: Some(metadata.pid),
                state: ReclaimTokenState::Alive,
            }))
        }
        Ok(metadata) => Ok(ExistingReclaimToken::StaleValid(metadata)),
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Ok(ExistingReclaimToken::Missing)
        }
        Err(ReadLockError::Io(error)) => Err(error),
        Err(ReadLockError::Malformed(_)) => {
            let old_enough = fs::metadata(token_path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > RECLAIM_TOKEN_MALFORMED_STALE_AGE);
            if old_enough {
                Ok(ExistingReclaimToken::StaleMalformed)
            } else {
                Ok(ExistingReclaimToken::Held(ReclaimTokenHeld {
                    pid: malformed_token_pid(token_path),
                    state: ReclaimTokenState::Malformed,
                }))
            }
        }
    }
}

fn inspect_reclaim_token_as_held(token_path: &Path) -> io::Result<ReclaimTokenAcquire> {
    let holder = match inspect_reclaim_token(token_path)? {
        ExistingReclaimToken::Held(holder) => holder,
        ExistingReclaimToken::StaleValid(metadata) => ReclaimTokenHeld {
            pid: Some(metadata.pid),
            state: ReclaimTokenState::Dead,
        },
        ExistingReclaimToken::StaleMalformed | ExistingReclaimToken::Missing => ReclaimTokenHeld {
            pid: malformed_token_pid(token_path),
            state: ReclaimTokenState::Malformed,
        },
    };
    Ok(ReclaimTokenAcquire::Held(holder))
}

fn malformed_token_pid(token_path: &Path) -> Option<u32> {
    let bytes = fs::read(token_path).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("pid")?
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
}

fn remove_malformed_reclaim_token(token_path: &Path) -> io::Result<bool> {
    match fs::remove_file(token_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Remove abandoned reclaim tokens from the bounded set of storage domains that
/// own persistent artifact locks, under every root in `roots`. A lock's token
/// sits beside the lock, so with `storage.cache_dir` set the cache-domain
/// tokens live under the cache root and the durable-state tokens under the
/// storage root; one pass covers both. The first maintenance pass in a
/// process runs; later passes are suppressed for 24 hours.
pub(crate) fn sweep_stale_reclaim_tokens(roots: &[&Path]) -> io::Result<Option<usize>> {
    let last_run = RECLAIM_TOKEN_SWEEP_LAST_RUN.get_or_init(|| Mutex::new(None));
    sweep_stale_reclaim_tokens_at(roots, Instant::now(), last_run)
}

fn sweep_stale_reclaim_tokens_at(
    roots: &[&Path],
    now: Instant,
    last_run: &Mutex<Option<Instant>>,
) -> io::Result<Option<usize>> {
    {
        let mut last_run = last_run
            .lock()
            .map_err(|_| io::Error::other("reclaim-token sweep cadence mutex poisoned"))?;
        if last_run
            .is_some_and(|last| now.saturating_duration_since(last) < RECLAIM_TOKEN_SWEEP_INTERVAL)
        {
            return Ok(None);
        }
        *last_run = Some(now);
    }

    let mut removed = 0_usize;
    for root in roots {
        for relative in ROOT_RECLAIM_TOKEN_PATHS {
            removed =
                removed.saturating_add(remove_stale_reclaim_token(&root.join(relative))? as usize);
        }
        for domain in RECLAIM_TOKEN_SWEEP_DOMAINS {
            removed = removed.saturating_add(sweep_reclaim_token_domain(&root.join(domain))?);
        }
        removed = removed.saturating_add(sweep_backup_reclaim_tokens(root)?);
    }
    Ok(Some(removed))
}

fn sweep_backup_reclaim_tokens(root: &Path) -> io::Result<usize> {
    let harnesses = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0_usize;
    for harness in harnesses {
        let harness = harness?;
        if !harness.file_type()?.is_dir() || !is_backup_harness_dir(&harness.file_name()) {
            continue;
        }
        let sessions = match fs::read_dir(harness.path().join("backups")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for session in sessions {
            let session = session?;
            if !session.file_type()?.is_dir() {
                continue;
            }
            let lock_entries = match fs::read_dir(session.path().join(".locks")) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for lock_entry in lock_entries {
                let lock_entry = lock_entry?;
                if lock_entry.file_type()?.is_file() && is_reclaim_token_path(&lock_entry.path()) {
                    removed = removed
                        .saturating_add(remove_stale_reclaim_token(&lock_entry.path())? as usize);
                }
            }
        }
    }
    Ok(removed)
}

fn is_backup_harness_dir(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    FIXED_BACKUP_HARNESS_DIRS.contains(&name)
        || BACKUP_HARNESS_DIR_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn sweep_reclaim_token_domain(domain_root: &Path) -> io::Result<usize> {
    let mut directories = vec![(domain_root.to_path_buf(), 0_usize)];
    let mut removed = 0_usize;
    while let Some((directory, depth)) = directories.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() && depth < RECLAIM_TOKEN_SWEEP_MAX_DOMAIN_DEPTH {
                directories.push((path, depth + 1));
            } else if file_type.is_file() && is_reclaim_token_path(&path) {
                removed = removed.saturating_add(remove_stale_reclaim_token(&path)? as usize);
            }
        }
    }
    Ok(removed)
}

fn remove_stale_reclaim_token(path: &Path) -> io::Result<bool> {
    let removed = match inspect_reclaim_token(path)? {
        ExistingReclaimToken::StaleValid(owner) => remove_lock_if_owned(path, &owner)?,
        ExistingReclaimToken::StaleMalformed => remove_malformed_reclaim_token(path)?,
        ExistingReclaimToken::Held(_) | ExistingReclaimToken::Missing => false,
    };
    Ok(removed)
}

fn is_reclaim_token_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".reclaim"))
}

fn reclaim_token_path(lock_path: &Path) -> PathBuf {
    let file_name = lock_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lock");
    lock_path.with_file_name(format!(".{file_name}.reclaim"))
}

fn dead_reclaim_backoff_ms(blocked_attempt: u32, poll_interval_ms: u64) -> u64 {
    let base = poll_interval_ms.max(DEAD_RECLAIM_INITIAL_BACKOFF_MS);
    base.saturating_mul(
        1_u64
            .checked_shl(blocked_attempt.min(20))
            .unwrap_or(u64::MAX),
    )
    .min(DEAD_RECLAIM_MAX_BACKOFF_MS)
}

fn log_reclaim_blocked(path: &Path, holder: &ReclaimTokenHeld) {
    let now = Instant::now();
    let logs = RECLAIM_BLOCK_LOGS.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut logs) = logs.lock() else {
        return;
    };
    let message = match logs.get_mut(path) {
        Some(record)
            if now.saturating_duration_since(record.last_emitted) < RECLAIM_BLOCK_LOG_INTERVAL =>
        {
            record.suppressed = record.suppressed.saturating_add(1);
            return;
        }
        Some(record) => {
            let suppressed = record.suppressed;
            record.last_emitted = now;
            record.suppressed = 0;
            format_reclaim_blocked(path, holder, suppressed)
        }
        None => {
            logs.insert(
                path.to_path_buf(),
                ReclaimBlockLogRecord {
                    last_emitted: now,
                    suppressed: 0,
                },
            );
            format_reclaim_blocked(path, holder, 0)
        }
    };
    drop(logs);
    emit_reclaim_warning(message);
}

fn format_reclaim_blocked(path: &Path, holder: &ReclaimTokenHeld, suppressed: u64) -> String {
    let pid = holder
        .pid
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut message = format!(
        "reclaim of {} blocked: reclaim token held by pid {} ({})",
        path.display(),
        pid,
        holder.state.as_str()
    );
    if suppressed > 0 {
        message.push_str(&format!(" (repeated {suppressed}x in 60s)"));
    }
    message
}

fn emit_reclaim_warning(message: String) {
    #[cfg(test)]
    RECLAIM_TEST_LOGS.with(|logs| logs.borrow_mut().push(message.clone()));
    slog_warn!("{}", message);
}

#[cfg(test)]
thread_local! {
    static RECLAIM_TEST_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };

    // The observer is thread-local so concurrent lock tests cannot record one
    // another's retry decisions.
    static RETRY_SLEEP_OBSERVER: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicUsize>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct RetrySleepObserverGuard {
    previous: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[cfg(test)]
impl Drop for RetrySleepObserverGuard {
    fn drop(&mut self) {
        RETRY_SLEEP_OBSERVER.with(|observer| {
            *observer.borrow_mut() = self.previous.take();
        });
    }
}

#[cfg(test)]
pub(crate) fn observe_retry_sleeps_for_test(
    observer: Arc<std::sync::atomic::AtomicUsize>,
) -> RetrySleepObserverGuard {
    let previous = RETRY_SLEEP_OBSERVER.with(|slot| slot.replace(Some(observer)));
    RetrySleepObserverGuard { previous }
}

#[cfg(test)]
fn note_retry_sleep_for_test() {
    RETRY_SLEEP_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow().as_ref() {
            observer.fetch_add(1, Ordering::SeqCst);
        }
    });
}

pub(crate) fn acquisition_now() -> Instant {
    #[cfg(test)]
    if let Some(now) = retry_clock::now() {
        return now;
    }
    Instant::now()
}

#[cfg(test)]
pub(crate) mod retry_clock {
    use super::*;

    struct State {
        started: Instant,
        now: Instant,
        preparation_delay: Duration,
        sleeps: Vec<Duration>,
        attempts: usize,
    }

    thread_local! {
        static CLOCK: std::cell::RefCell<Option<State>> = const { std::cell::RefCell::new(None) };
    }

    pub(crate) struct Guard;

    impl Guard {
        pub(crate) fn install(preparation_delay: Duration) -> Self {
            CLOCK.with(|clock| {
                let mut clock = clock.borrow_mut();
                assert!(clock.is_none(), "retry clock already installed");
                let started = Instant::now();
                *clock = Some(State {
                    started,
                    now: started,
                    preparation_delay,
                    sleeps: Vec::new(),
                    attempts: 0,
                });
            });
            Self
        }

        pub(crate) fn observations(&self) -> (Duration, Vec<Duration>, usize) {
            CLOCK.with(|clock| {
                let clock = clock.borrow();
                let state = clock.as_ref().unwrap();
                (
                    state.now - state.started,
                    state.sleeps.clone(),
                    state.attempts,
                )
            })
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            CLOCK.with(|clock| *clock.borrow_mut() = None);
        }
    }

    pub(super) fn now() -> Option<Instant> {
        CLOCK.with(|clock| clock.borrow().as_ref().map(|state| state.now))
    }

    pub(super) fn before_acquire() {
        CLOCK.with(|clock| {
            if let Some(state) = clock.borrow_mut().as_mut() {
                // Model filesystem preparation after the caller chose its deadline.
                state.now += std::mem::take(&mut state.preparation_delay);
            }
        });
    }

    pub(super) fn note_attempt() {
        CLOCK.with(|clock| {
            if let Some(state) = clock.borrow_mut().as_mut() {
                state.attempts += 1;
            }
        });
    }

    pub(super) fn sleep(duration: Duration) -> bool {
        CLOCK.with(|clock| {
            let mut clock = clock.borrow_mut();
            let Some(state) = clock.as_mut() else {
                return false;
            };
            state.sleeps.push(duration);
            state.now += duration;
            assert!(
                state.sleeps.len() <= 1_000,
                "retry loop ignored its deadline"
            );
            true
        })
    }
}

pub(crate) fn sleep_until_retry(
    deadline: Option<Instant>,
    poll_interval_ms: u64,
) -> Result<(), AcquireError> {
    let poll = Duration::from_millis(poll_interval_ms);
    let sleep_for = match deadline {
        Some(deadline) => {
            let now = acquisition_now();
            if now >= deadline {
                return Err(AcquireError::Timeout);
            }
            poll.min(deadline.saturating_duration_since(now))
        }
        None => poll,
    };
    #[cfg(test)]
    note_retry_sleep_for_test();
    #[cfg(test)]
    if retry_clock::sleep(sleep_for) {
        return Ok(());
    }
    thread::sleep(sleep_for);
    Ok(())
}

pub(crate) fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if crate::durability::sync_dir(parent).is_ok() {
            #[cfg(unix)]
            io_ledger::record(|ledger| ledger.dir_syncs += 1);
        }
    }
}

/// Per-thread counters of successful acquisitions and lease I/O: file fsyncs,
/// directory fsyncs, newly created files (new inodes) and payload bytes.
/// Tests read them to prove which paths sync and how much a heartbeat
/// writes; outside tests every call compiles to nothing. The counters are
/// thread-local so tests running in parallel do not see each other's I/O.
pub(crate) mod io_ledger {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) struct IoLedger {
        pub(crate) acquisitions: u64,
        /// Descriptors opened for a heartbeat read or write (`fopen`/`open`).
        pub(crate) opens: u64,
        /// Descriptors released by a heartbeat (`fclose`/`close`), at the
        /// `fclose` that `close()` spends its time in on the performance sweep.
        pub(crate) closes: u64,
        pub(crate) file_syncs: u64,
        pub(crate) dir_syncs: u64,
        pub(crate) new_files: u64,
        pub(crate) bytes_written: u64,
    }

    #[cfg(test)]
    thread_local! {
        static LEDGER: std::cell::Cell<IoLedger> = std::cell::Cell::new(IoLedger::default());
    }

    #[cfg(test)]
    pub(crate) fn record(update: impl FnOnce(&mut IoLedger)) {
        LEDGER.with(|cell| {
            let mut ledger = cell.get();
            update(&mut ledger);
            cell.set(ledger);
        });
    }

    #[cfg(not(test))]
    #[inline(always)]
    pub(crate) fn record(_update: impl FnOnce(&mut IoLedger)) {}

    /// Return this thread's counters and reset them to zero.
    #[cfg(test)]
    pub(crate) fn take() -> IoLedger {
        LEDGER.with(|cell| cell.replace(IoLedger::default()))
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

#[cfg(unix)]
pub(crate) fn current_hostname() -> String {
    let mut buffer = [0u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result == 0 {
        let len = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        if len > 0 {
            return String::from_utf8_lossy(&buffer[..len]).into_owned();
        }
    }

    crate::environment::non_empty_var("HOSTNAME").unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(windows)]
pub(crate) fn current_hostname() -> String {
    crate::environment::non_empty_var("COMPUTERNAME")
        .or_else(|| crate::environment::non_empty_var("HOSTNAME"))
        .unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn current_hostname() -> String {
    crate::environment::non_empty_var("HOSTNAME").unwrap_or_else(|| "unknown-host".to_string())
}

/// Returns whether a same-host lock owner is still the same process instance.
///
/// Old leases have no start time, so they deliberately retain PID-only liveness
/// for backward compatibility. If an OS lookup cannot attest a recorded start
/// time, keep the PID alive: incorrectly reclaiming a paused owner is worse than
/// waiting for a process that may release the lock later.
fn lock_owner_is_alive(metadata: &LockMetadata) -> bool {
    if !process_alive(metadata.pid) {
        return false;
    }

    let Some(recorded_start_time) = metadata.process_start_time else {
        return true;
    };
    let Some(current_identity) = process_identity(metadata.pid) else {
        return true;
    };

    if current_identity.start_time != recorded_start_time {
        return false;
    }

    match &metadata.boot_id {
        Some(recorded_boot_id) => current_identity
            .boot_id
            .as_deref()
            .map_or(true, |current_boot_id| current_boot_id == recorded_boot_id),
        None => true,
    }
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    // Field 22 is starttime. Split after the final ')' because a process name is
    // allowed to contain spaces and parentheses.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let start_time = stat
        .rsplit_once(") ")?
        .1
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()?
        .trim()
        .to_owned();
    if boot_id.is_empty() {
        return None;
    }

    Some(ProcessIdentity {
        start_time,
        boot_id: Some(boot_id),
        exited: matches!(
            stat.rsplit_once(") ")?.1.split_ascii_whitespace().next()?,
            "Z" | "X"
        ),
    })
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }

    // PROC_PIDTBSDINFO supplies the kernel-recorded process birth time without
    // spawning a command or trusting user-controlled process metadata.
    const PROC_PIDTBSDINFO: libc::c_int = 3;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let start_time = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;

    Some(ProcessIdentity {
        start_time,
        boot_id: None,
        exited: info.pbi_status == 5,
    })
}

#[cfg(windows)]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    windows_process_creation_time(pid)
        .ok()
        .flatten()
        .map(|start_time| ProcessIdentity {
            start_time,
            boot_id: None,
            exited: false,
        })
}

/// Exact birth and boot identity for speech redemption. Missing evidence is denial,
/// unlike the deliberately conservative "still alive" policy used by lock cleanup.
pub(crate) fn speech_process_identity(pid: u32) -> Option<String> {
    let identity = process_identity(pid)?;
    if identity.exited {
        return None;
    }
    #[cfg(target_os = "linux")]
    let boot = identity.boot_id?;
    #[cfg(target_os = "macos")]
    let boot = crate::storage_retention::boot_id()?;
    #[cfg(windows)]
    let boot = speech_windows_boot_id()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let boot = return None;
    Some(format!(
        "{}:{boot}:{}",
        std::env::consts::OS,
        identity.start_time
    ))
}

#[cfg(windows)]
fn speech_windows_boot_id() -> Option<String> {
    #[repr(C)]
    struct BootInfo {
        id: [u8; 16],
        firmware: u32,
        flags: u64,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        #[link_name = "NtQuerySystemInformation"]
        fn nt_query_system_information(
            class: u32,
            buffer: *mut std::ffi::c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    let mut info = BootInfo {
        id: [0; 16],
        firmware: 0,
        flags: 0,
    };
    let mut returned = 0;
    // SystemBootEnvironmentInformation (90) supplies the kernel's per-boot GUID.
    let status = unsafe {
        nt_query_system_information(
            90,
            (&mut info as *mut BootInfo).cast(),
            std::mem::size_of::<BootInfo>() as u32,
            &mut returned,
        )
    };
    if status < 0 || returned < 16 || info.id == [0; 16] {
        return None;
    }
    Some(info.id.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn process_identity(_pid: u32) -> Option<ProcessIdentity> {
    None
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }

    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }

    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
pub(crate) fn process_alive(pid: u32) -> bool {
    // A denied or inconclusive probe must not permit reclaiming a live owner.
    windows_process_creation_time(pid).map_or(true, |created| created.is_some())
}

#[cfg(windows)]
fn windows_process_creation_time(pid: u32) -> io::Result<Option<u64>> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return Ok(None);
    }
    const SYNCHRONIZE_RIGHT: u32 = 0x0010_0000;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_RIGHT,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let handle = ProcessHandle(handle);
    // Waiting on the process object distinguishes exit from liveness even when
    // an application exits with 259, the ambiguous STILL_ACTIVE exit-code value.
    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_OBJECT_0 => return Ok(None),
        WAIT_TIMEOUT => {}
        _ => return Err(io::Error::last_os_error()),
    }
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    if unsafe { GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
    ))
}

#[cfg(windows)]
pub(crate) fn windows_process_start_time_ms(pid: u32) -> Option<u64> {
    windows_creation_ticks_to_unix_ms(windows_process_creation_time(pid).ok().flatten()?)
}

#[cfg(windows)]
fn windows_creation_ticks_to_unix_ms(ticks: u64) -> Option<u64> {
    // FILETIME counts 100-nanosecond intervals since 1601; manifests use Unix ms.
    const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;
    ticks
        .checked_sub(UNIX_EPOCH_FILETIME)
        .map(|ticks| ticks / 10_000)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Child, ChildStdin, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier};

    #[cfg(unix)]
    #[test]
    fn advisory_lock_guard_releases_lock_with_a_live_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("advisory.lock");
        for shared in [true, false] {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap();
            if shared {
                file.lock_shared().unwrap();
            } else {
                file.lock().unwrap();
            }
            // A duplicate shares the open file description, just like fork's copy.
            let duplicate = file.try_clone().unwrap();
            let guard = AdvisoryLockGuard::from_locked_file(file);
            let probe = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            assert!(matches!(
                probe.try_lock(),
                Err(fs::TryLockError::WouldBlock)
            ));
            drop(guard);
            probe
                .try_lock()
                .expect("the owner released exclusion even with a live duplicate");
            assert!(duplicate.metadata().is_ok());
            probe.unlock().unwrap();
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_process_probe_records_creation_identity() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("current Windows process creation identity");
        assert!(identity.start_time > 0);
        assert!(windows_process_start_time_ms(pid).unwrap() > 0);
        assert_eq!(
            windows_creation_ticks_to_unix_ms(116_444_736_000_000_000),
            Some(0)
        );
        assert_eq!(
            windows_creation_ticks_to_unix_ms(116_444_736_010_000_000),
            Some(1000)
        );
        assert_eq!(windows_creation_ticks_to_unix_ms(0), None);
        assert!(process_alive(pid));
        assert!(!process_alive(0));
        assert!(!process_alive(u32::MAX));
        let mut metadata = current_process_metadata();
        metadata.process_start_time = Some(identity.start_time.saturating_add(1));
        assert!(
            !lock_owner_is_alive(&metadata),
            "recycled PID must not keep an old lease alive"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_process_probe_recognizes_exited_status_259() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 259"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert_eq!(child.wait().unwrap().code(), Some(259));
        assert!(
            !process_alive(pid),
            "a signaled process object is exited even when its exit code is STILL_ACTIVE"
        );
    }

    #[test]
    fn malformed_reclaim_preserves_replacement_epoch() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"malformed").unwrap();
        let replacement = synthetic_metadata(std::process::id(), current_hostname(), now_ms());
        let expected_epoch = replacement.writer_epoch.clone();
        let replace_path = path.clone();
        BEFORE_MALFORMED_RECLAIM.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&replace_path).unwrap();
                write_synthetic_lock(&replace_path, &replacement);
            }))
        });
        let result = acquire_with_config(&path, Some(Duration::from_millis(100)), test_config());
        assert!(
            matches!(result, Err(AcquireError::Timeout)),
            "replacement owner must block acquisition"
        );
        assert_eq!(
            read_lock_metadata(&path).unwrap().writer_epoch,
            expected_epoch
        );
    }

    #[test]
    fn heartbeat_spawn_failure_removes_own_lock() {
        let (_dir, path) = test_lock_path();
        BEFORE_HEARTBEAT_SPAWN_FAILURE.with(|slot| *slot.borrow_mut() = Some(Box::new(|| {})));
        assert!(create_new_lock(&path, &current_hostname(), test_config()).is_err());
        assert!(
            !path.exists(),
            "failed acquisition must not leave a live-PID lock"
        );
        drop(create_new_lock(&path, &current_hostname(), test_config()).unwrap());
    }

    #[test]
    fn heartbeat_spawn_failure_preserves_replacement_lock() {
        let (_dir, path) = test_lock_path();
        let replacement = synthetic_metadata(std::process::id(), current_hostname(), now_ms());
        let expected_epoch = replacement.writer_epoch.clone();
        let replace_path = path.clone();
        BEFORE_HEARTBEAT_SPAWN_FAILURE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&replace_path).unwrap();
                write_synthetic_lock(&replace_path, &replacement);
            }))
        });
        assert!(create_new_lock(&path, &current_hostname(), test_config()).is_err());
        assert_eq!(
            read_lock_metadata(&path).unwrap().writer_epoch,
            expected_epoch
        );
    }

    /// A lock file whose metadata cannot be read because another actor is
    /// mid-create/delete is contention, not a failure. The create arm already
    /// treated it that way; the metadata read beside it did not, so on Windows
    /// a pending-deletion lock denied the read and propagated a hard error to a
    /// caller that then panicked mid-write. Permission-denied is classified as
    /// contention on every platform, so this reproduces without Windows: with
    /// the retry the acquisition spends its budget and times out; without it
    /// the raw permission error escapes immediately.
    #[cfg(unix)]
    #[test]
    fn unreadable_lock_metadata_is_retried_as_contention_not_propagated() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cache.lock");
        std::fs::write(&path, b"{}\n").expect("seed lock file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("deny reads");
        if std::fs::read(&path).is_ok() {
            // Running as a user that bypasses the mode bits (root in some CI
            // images); the premise cannot hold, so assert nothing.
            return;
        }

        let outcome = try_acquire(&path, Duration::from_millis(250));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        match outcome {
            Err(AcquireError::Timeout) => {}
            Err(AcquireError::Io(error)) => {
                panic!("contended metadata read propagated instead of retrying: {error:?}")
            }
            Ok(_) => panic!("acquired a lock whose metadata could not be read"),
        }
    }

    const HARNESS_TEST: &str = "fs_lock::tests::fs_lock_separate_process_harness";
    const HARNESS_MODE_INSPECT: &str = "inspect";

    fn test_config() -> LockConfig {
        LockConfig {
            heartbeat_interval_ms: 25,
            stale_heartbeat_ms: 2_000,
            live_owner_warn_ms: LIVE_OWNER_WARN_MS,
            poll_interval_ms: 10,
        }
    }

    fn test_lock_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test.lock");
        (dir, path)
    }

    fn write_synthetic_lock(path: &Path, metadata: &LockMetadata) {
        let mut file = open_new_lock_file(path).expect("create synthetic lock");
        write_lock_metadata_to_file(&mut file, metadata).expect("write synthetic lock");
    }

    #[derive(Serialize)]
    struct LegacyLockMetadata<'a> {
        pid: u32,
        hostname: &'a str,
        created_at_ms: u64,
        heartbeat_at_ms: u64,
        writer_epoch: &'a str,
    }

    fn write_legacy_lock(path: &Path, metadata: &LockMetadata) -> String {
        let legacy = LegacyLockMetadata {
            pid: metadata.pid,
            hostname: &metadata.hostname,
            created_at_ms: metadata.created_at_ms,
            heartbeat_at_ms: metadata.heartbeat_at_ms,
            writer_epoch: &metadata.writer_epoch,
        };
        let contents = format!(
            "{}\n",
            serde_json::to_string(&legacy).expect("serialize legacy lock")
        );
        fs::write(path, &contents).expect("write legacy synthetic lock");
        contents
    }

    fn synthetic_metadata(pid: u32, hostname: String, created_at_ms: u64) -> LockMetadata {
        LockMetadata {
            pid,
            hostname,
            // Synthetic metadata defaults to the legacy shape so tests must opt
            // in when they need to exercise process-instance identity.
            process_start_time: None,
            boot_id: None,
            created_at_ms,
            heartbeat_at_ms: created_at_ms,
            writer_epoch: format!("synthetic-{pid}-{created_at_ms}"),
        }
    }

    fn current_process_metadata() -> LockMetadata {
        let now = now_ms();
        let pid = std::process::id();
        let process_identity = process_identity(pid);
        let mut metadata = synthetic_metadata(pid, current_hostname(), now);
        metadata.process_start_time = process_identity
            .as_ref()
            .map(|identity| identity.start_time);
        metadata.boot_id = process_identity.and_then(|identity| identity.boot_id);
        metadata
    }

    fn different_start_time(start_time: u64) -> u64 {
        start_time.checked_add(1).unwrap_or(start_time - 1)
    }

    fn write_reclaim_token(lock_path: &Path, metadata: &LockMetadata) -> PathBuf {
        let token_path = reclaim_token_path(lock_path);
        write_synthetic_lock(&token_path, metadata);
        token_path
    }

    fn take_reclaim_test_logs() -> Vec<String> {
        RECLAIM_TEST_LOGS.with(|logs| std::mem::take(&mut *logs.borrow_mut()))
    }

    #[test]
    fn lock_operation_trace_lines_are_debug_not_info() {
        let source = include_str!("fs_lock.rs");
        assert!(source.contains("slog_debug!(\"acquired filesystem lock at {}\", path.display())"));
        assert!(
            source.contains("slog_debug!(\"released filesystem lock at {}\", self.path.display())")
        );
        assert!(!source.contains("slog_info!(\"acquired filesystem lock at {}\", path.display())"));
        assert!(
            !source.contains("slog_info!(\"released filesystem lock at {}\", self.path.display())")
        );
    }

    #[test]
    fn acquire_creates_lockfile_and_unlocks_on_drop() {
        let (_dir, path) = test_lock_path();

        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let metadata = read_lock_metadata(&path).expect("read lock metadata");
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(metadata.hostname, current_hostname());
        assert_eq!(metadata.created_at_ms, guard.metadata.created_at_ms);
        assert_eq!(metadata.writer_epoch, guard.metadata.writer_epoch);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn permission_denied_is_treated_as_transient_create_contention() {
        // Windows surfaces a contended create/delete on the same lock path as
        // access-denied; acquire must retry these rather than fail the caller.
        let err = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(is_transient_create_contention(&err));
    }

    #[test]
    fn unrelated_io_errors_are_not_treated_as_contention() {
        // A genuinely fatal error (e.g. the parent dir is missing) must still
        // propagate, not spin in the transient-retry arm.
        let err = io::Error::from(io::ErrorKind::NotFound);
        assert!(!is_transient_create_contention(&err));
    }

    #[cfg(windows)]
    #[test]
    fn windows_sharing_violation_is_treated_as_transient_create_contention() {
        // ERROR_SHARING_VIOLATION (32) is the other contention code Windows
        // returns when a concurrent actor holds the path open mid-create.
        let err = io::Error::from_raw_os_error(32);
        assert!(is_transient_create_contention(&err));
    }

    #[test]
    fn reclaim_refuses_to_delete_a_different_owners_lock() {
        let (_dir, path) = test_lock_path();

        // A lock currently owned by "owner B".
        let owner_b = synthetic_metadata(4242, "host-b".to_string(), now_ms());
        create_lock_file_atomically(&path, &owner_b).expect("write owner B lock");

        // We judged a DIFFERENT (older) owner A as stale. Reclaiming must NOT
        // delete B's lock (the TOCTOU split-brain guard).
        let judged_a = synthetic_metadata(1111, "host-a".to_string(), now_ms() - 1_000_000);
        let outcome = reclaim_lock_file(&path, &judged_a).expect("reclaim");
        assert!(
            matches!(outcome, ReclaimResult::Unchanged),
            "must not remove a different owner's lock"
        );
        assert!(path.exists(), "owner B's lock must survive");
        let still = read_lock_metadata(&path).expect("still readable");
        assert_eq!(still.pid, 4242, "owner B's lock intact");
    }

    #[test]
    fn reclaim_deletes_when_identity_still_matches() {
        let (_dir, path) = test_lock_path();
        let owner = synthetic_metadata(1111, "host-a".to_string(), 5_000);
        create_lock_file_atomically(&path, &owner).expect("write lock");

        // Same identity we judged → safe to remove.
        let outcome = reclaim_lock_file(&path, &owner).expect("reclaim");
        assert!(
            matches!(outcome, ReclaimResult::Removed),
            "matching-identity stale lock should be removed"
        );
        assert!(!path.exists());

        // Reclaiming a now-absent lock is a no-op, not an error.
        assert!(matches!(
            reclaim_lock_file(&path, &owner).expect("reclaim missing"),
            ReclaimResult::Unchanged
        ));
    }

    #[test]
    fn try_acquire_once_reclaims_malformed_after_one_grace_read() {
        for contents in [
            b"".as_slice(),
            b"{\"pid\":999999999,\"hostname\":".as_slice(),
        ] {
            let (_dir, path) = test_lock_path();
            fs::write(&path, contents).unwrap();
            let sleeper_entries = Arc::new(AtomicUsize::new(0));
            let _observer = observe_retry_sleeps_for_test(Arc::clone(&sleeper_entries));
            let guard = try_acquire_once(&path).expect("stable malformed lease must be reclaimed");
            assert!(guard.verify_writer_epoch().unwrap());
            assert_eq!(sleeper_entries.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn try_acquire_once_malformed_reclaim_stops_when_token_is_held() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"malformed").unwrap();
        write_reclaim_token(&path, &current_process_metadata());
        let sleeper_entries = Arc::new(AtomicUsize::new(0));
        let _observer = observe_retry_sleeps_for_test(Arc::clone(&sleeper_entries));
        assert!(matches!(
            try_acquire_once(&path),
            Err(AcquireError::Timeout)
        ));
        assert_eq!(sleeper_entries.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read(&path).unwrap(), b"malformed");
    }

    #[test]
    fn try_acquire_once_malformed_reclaim_preserves_live_replacement() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"malformed").unwrap();
        let replacement = current_process_metadata();
        let expected_epoch = replacement.writer_epoch.clone();
        let replace_path = path.clone();
        BEFORE_MALFORMED_RECLAIM.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(&replace_path).unwrap();
                write_synthetic_lock(&replace_path, &replacement);
            }))
        });
        assert!(matches!(
            try_acquire_once(&path),
            Err(AcquireError::Timeout)
        ));
        assert_eq!(
            read_lock_metadata(&path).unwrap().writer_epoch,
            expected_epoch
        );
    }

    #[test]
    fn try_acquire_once_never_waits_behind_live_owner() {
        const OUTER_THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(30);

        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let contender_path = path.clone();
        let sleeper_entries = Arc::new(AtomicUsize::new(0));
        let contender_sleeper_entries = Arc::clone(&sleeper_entries);
        let (announced_tx, announced_rx) = mpsc::sync_channel(1);
        let (enter_tx, enter_rx) = mpsc::sync_channel::<()>(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let contender = std::thread::spawn(move || {
            let _ = announced_tx.send(());
            let _ = enter_rx.recv();
            let _observer = observe_retry_sleeps_for_test(contender_sleeper_entries);
            let _ = result_tx.send(try_acquire_once(&contender_path));
        });

        announced_rx
            .recv_timeout(OUTER_THREAD_JOIN_TIMEOUT)
            .expect("contender should announce before the controlled enter gate");
        // Keep the announce-to-enter gap under channel control rather than
        // charging an arbitrary scheduler pause to the acquisition decision.
        enter_tx
            .send(())
            .expect("contender should wait at the controlled enter gate");

        let result = match result_rx.recv_timeout(OUTER_THREAD_JOIN_TIMEOUT) {
            Ok(result) => result,
            Err(error) => {
                drop(guard);
                let _ = result_rx.recv_timeout(OUTER_THREAD_JOIN_TIMEOUT);
                let _ = contender.join();
                panic!("contender did not finish before the outer join bound: {error}");
            }
        };

        contender.join().expect("contender should exit");
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(
            sleeper_entries.load(Ordering::SeqCst),
            0,
            "zero-timeout acquisition must return Timeout without sleeping"
        );
    }

    #[test]
    fn acquire_serializes_concurrent_callers() {
        let (_dir, path) = test_lock_path();
        let path = Arc::new(path);
        let barrier = Arc::new(Barrier::new(3));
        let inside = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(AtomicUsize::new(0));
        let max_inside = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let inside = Arc::clone(&inside);
            let entered = Arc::clone(&entered);
            let max_inside = Arc::clone(&max_inside);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
                    .expect("thread acquire lock");
                let previous = inside.fetch_add(1, Ordering::SeqCst);
                assert_eq!(previous, 0, "two lock holders overlapped");
                entered.fetch_add(1, Ordering::SeqCst);
                max_inside.fetch_max(previous + 1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(75));
                inside.fetch_sub(1, Ordering::SeqCst);
                drop(guard);
            }));
        }

        barrier.wait();
        for handle in handles {
            handle.join().expect("join worker");
        }

        assert_eq!(entered.load(Ordering::SeqCst), 2);
        assert_eq!(max_inside.load(Ordering::SeqCst), 1);
        assert!(!path.exists());
    }

    #[test]
    fn failed_atomic_replacement_preserves_existing_destination() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let source = dir.path().join("source.tmp");
        let destination = dir.path().join("artifact.bin");
        fs::write(&source, b"new artifact").expect("write source");
        fs::write(&destination, b"valid old artifact").expect("write destination");

        let error = rename_over_with(&source, &destination, |_from, _to| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replacement failure",
            ))
        })
        .expect_err("replacement must fail");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            fs::read(&destination).expect("read preserved destination"),
            b"valid old artifact"
        );
        assert_eq!(
            fs::read(&source).expect("read retained source"),
            b"new artifact"
        );
    }

    /// Number of lease files and beats per lease that model sixty seconds of
    /// heartbeats at the production interval.
    const LEDGER_LEASES: usize = 8;
    const LEDGER_BEATS_PER_LEASE: usize = (60_000 / HEARTBEAT_INTERVAL_MS) as usize;

    fn create_owned_lock(path: &Path) -> LockMetadata {
        let metadata = current_process_metadata();
        create_lock_file_atomically(path, &metadata).expect("create lock");
        metadata
    }

    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).expect("lock metadata").ino()
    }

    /// Sixty seconds of heartbeats on several leases must not fsync anything
    /// and must not create a new file per beat. The ledger totals are printed
    /// so the cost can be compared across changes.
    #[test]
    fn heartbeat_ledger_for_sixty_seconds_has_no_fsync_and_no_new_inodes() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let leases = (0..LEDGER_LEASES)
            .map(|index| {
                let path = dir.path().join(format!("writer-{index}.lease"));
                let owner = create_owned_lock(&path);
                (path, owner)
            })
            .collect::<Vec<_>>();
        #[cfg(unix)]
        let inodes_before = leases
            .iter()
            .map(|(path, _)| inode(path))
            .collect::<Vec<_>>();
        let file_bytes: u64 = leases
            .iter()
            .map(|(path, _)| fs::metadata(path).expect("lock size").len())
            .sum();

        let _ = io_ledger::take();
        for _ in 0..LEDGER_BEATS_PER_LEASE {
            for (path, owner) in &leases {
                heartbeat_once(path, owner, &Mutex::new(None)).expect("heartbeat");
            }
        }
        let ledger = io_ledger::take();
        eprintln!(
            "fs_lock heartbeat ledger: leases={LEDGER_LEASES} beats_per_lease={LEDGER_BEATS_PER_LEASE} {ledger:?}"
        );

        assert_eq!(ledger.file_syncs, 0, "heartbeats must not fsync the lease");
        assert_eq!(
            ledger.dir_syncs, 0,
            "heartbeats must not fsync the directory"
        );
        assert_eq!(ledger.new_files, 0, "heartbeats must rewrite in place");
        assert_eq!(
            ledger.bytes_written,
            file_bytes * LEDGER_BEATS_PER_LEASE as u64,
            "each beat rewrites exactly one lease's bytes"
        );
        #[cfg(unix)]
        assert_eq!(
            leases
                .iter()
                .map(|(path, _)| inode(path))
                .collect::<Vec<_>>(),
            inodes_before,
            "heartbeats must keep the lease inode"
        );
        for (path, owner) in &leases {
            let current = read_lock_metadata(path).expect("read lease");
            assert!(lock_identity_matches(&current, owner));
            assert!(current.heartbeat_at_ms >= owner.heartbeat_at_ms);
        }
    }

    /// The old contract asserted file + directory syncs at creation and during
    /// reclaim. Ownership instead depends on the live process identity: after
    /// power loss the owner is dead, and torn leases are stale. The atomic hard
    /// link and reclaim token still serialize live contenders without making
    /// those process-lifetime records power-loss durable.
    #[test]
    fn durability_lease_create_and_reclaim_unsynced() {
        let (_dir, path) = test_lock_path();
        let _ = io_ledger::take();
        crate::durability::take();
        let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
            .expect("create lease");
        let created = io_ledger::take();
        drop(guard);
        assert_eq!(
            (created.file_syncs, created.dir_syncs, created.new_files),
            (0, 0, 1),
            "lease creation must not sync: {created:?}"
        );
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);

        let dead = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &dead);
        let _ = io_ledger::take();
        crate::durability::take();
        let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
            .expect("reclaim dead lease");
        let reclaimed = io_ledger::take();
        drop(guard);
        // The failed create's temp file, reclaim token and new lease still
        // require three inodes, but none need to survive their owner's death.
        assert_eq!(
            (
                reclaimed.file_syncs,
                reclaimed.dir_syncs,
                reclaimed.new_files
            ),
            (0, 0, 3),
            "lease reclaim must not sync: {reclaimed:?}"
        );
        assert_eq!(crate::durability::sync_count(&crate::durability::take()), 0);
    }

    /// Leases a live daemon of this size holds at once: one root-keyed inspect
    /// writer lease and one root-keyed callgraph writer lease per bound project,
    /// for about a hundred projects with both caches open.
    const DAEMON_SCALE_LEASES: usize = 200;

    /// Fails if a beat opened or closed a descriptor instead of writing through
    /// the handle the guard keeps, which is what a regression to open-per-beat
    /// would show up as in the ledger.
    fn assert_beat_opens_no_descriptor(beat: &io_ledger::IoLedger) {
        assert_eq!(
            (beat.opens, beat.closes),
            (0, 0),
            "a beat must write through the kept handle, without opening or \
             closing a descriptor: {beat:?}"
        );
    }

    /// What the heartbeat costs for a daemon-shaped set of long-lived leases.
    ///
    /// Before the kept handle, every beat of every lease opened the file,
    /// read it, wrote it and closed it: 200 opens and 200 closes per beat, 40
    /// per second at the five-second interval, and the macOS `close()` that
    /// pulls the page write-back ran on the heartbeat thread. A beat now writes
    /// through the handle the heartbeat already holds. `kept_descriptors` counts
    /// how many descriptor opens occur across all the beats, so a regression to
    /// open-per-beat moves it from zero to the number of beats.
    #[test]
    fn daemon_scale_heartbeats_keep_descriptors_open_between_beats() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let _ = io_ledger::take();
        let guards = (0..DAEMON_SCALE_LEASES)
            .map(|index| {
                acquire_with_config(
                    &dir.path().join(format!("root-{index}.writer.lease")),
                    Some(Duration::from_secs(2)),
                    test_config(),
                )
                .expect("acquire live lease")
            })
            .collect::<Vec<_>>();
        // Open-only: every lease created here had no prior contents, so its
        // heartbeat started from the handle acquisition opened. A kept
        // descriptor released since would have taken the close path below.
        let setup = io_ledger::take();
        assert_eq!(
            setup.closes, 0,
            "acquiring a fresh lease must not close a descriptor: {setup:?}"
        );

        let beats = 3;
        let _ = io_ledger::take();
        for _ in 0..beats {
            for guard in &guards {
                guard.heartbeat_once().expect("beat");
            }
        }
        let beat_cost = io_ledger::take();
        eprintln!(
            "fs_lock heartbeat cost: leases={DAEMON_SCALE_LEASES} beats_per_lease={beats} \
             opens={} closes={} bytes={} (open+close per beat would be opens={} closes={})",
            beat_cost.opens,
            beat_cost.closes,
            beat_cost.bytes_written,
            DAEMON_SCALE_LEASES * beats,
            DAEMON_SCALE_LEASES * beats,
        );
        assert_beat_opens_no_descriptor(&beat_cost);
        assert!(beat_cost.bytes_written > 0, "{beat_cost:?}");

        for guard in &guards {
            let current = read_lock_metadata(guard.path()).expect("read lease");
            assert!(lock_identity_matches(&current, &guard.metadata));
        }

        drop(guards);
        let released = io_ledger::take();
        // Release closes each handle once. Other tests in this process run
        // their own leases on their own threads, so this asks only that the
        // releases are accounted for, never for exactly this test's count.
        assert!(
            released.closes >= DAEMON_SCALE_LEASES as u64,
            "each live lease closes its handle once, at release: {released:?}"
        );
    }

    /// A crash after an unsynced rename can leave a zero-length lease. Readers
    /// must treat it as stale: acquisition removes it and takes the lock, the
    /// owner's heartbeat reports a transient malformed read, and nothing panics.
    #[test]
    fn zero_length_lease_is_treated_as_stale() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"").expect("write empty lease");
        let owner = current_process_metadata();
        assert!(matches!(
            heartbeat_once(&path, &owner, &Mutex::new(None)),
            Err(HeartbeatError::Malformed(_))
        ));
        assert!(!heartbeat_error_is_terminal(&HeartbeatError::Malformed(
            serde_json::from_slice::<LockMetadata>(b"").unwrap_err()
        )));

        let guard = acquire_with_config(&path, Some(Duration::from_secs(2)), test_config())
            .expect("empty lease must be reclaimed");
        assert!(guard.verify_writer_epoch().expect("verify new owner"));
    }

    /// When the new timestamp would change the file length, the heartbeat
    /// falls back to an atomic rename, still without fsync.
    #[test]
    fn heartbeat_length_change_falls_back_to_unsynced_rename() {
        let (_dir, path) = test_lock_path();
        let mut owner = current_process_metadata();
        owner.heartbeat_at_ms = 7;
        create_lock_file_atomically(&path, &owner).expect("create lock");

        let _ = io_ledger::take();
        heartbeat_once(&path, &owner, &Mutex::new(None)).expect("heartbeat");
        let ledger = io_ledger::take();
        assert_eq!((ledger.file_syncs, ledger.dir_syncs), (0, 0), "{ledger:?}");
        assert_eq!(ledger.new_files, 1, "{ledger:?}");
        let current = read_lock_metadata(&path).expect("read lease");
        assert!(lock_identity_matches(&current, &owner));
        assert!(current.heartbeat_at_ms > 7);
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp file must not be left behind");
    }

    #[test]
    fn heartbeat_updates_lockfile_timestamp() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let initial_metadata = read_lock_metadata(&path).expect("read initial metadata");
        let initial = initial_metadata.heartbeat_at_ms;

        // Poll for up to 2s rather than sleeping a fixed multiple of the
        // heartbeat interval. `park_timeout` is a *maximum* wait, not a
        // guaranteed periodic timer — under load (shared macOS CI runners
        // running other cargo-test threads concurrently) the heartbeat
        // thread may not fire 3 times within 75ms even though
        // heartbeat_interval_ms=25. The contract being asserted is "the
        // heartbeat advances eventually", not "it advances within N
        // heartbeat intervals".
        //
        let deadline = std::time::Instant::now() + Duration::from_millis(2_000);
        let mut updated = initial;
        while std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            match read_lock_metadata(&path) {
                Ok(meta) => {
                    updated = meta.heartbeat_at_ms;
                    if updated > initial {
                        break;
                    }
                }
                Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    // Heartbeat thread is mid-rewrite (Windows
                    // remove-then-rename window). Retry next iteration.
                    continue;
                }
                Err(other) => panic!("read updated metadata: {other:?}"),
            }
        }
        assert!(
            updated > initial,
            "heartbeat timestamp did not advance within 2s"
        );
        let updated_metadata = read_lock_metadata(&path).expect("read final metadata");
        assert_eq!(
            updated_metadata.process_start_time, guard.metadata.process_start_time,
            "heartbeat rewrite must preserve the owner's process start time"
        );
        assert_eq!(
            updated_metadata.boot_id, guard.metadata.boot_id,
            "heartbeat rewrite must preserve the owner's boot identity"
        );
        drop(guard);
    }

    #[test]
    fn dead_pid_lock_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("reclaim dead pid lock");
        let metadata = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn zero_timeout_dead_pid_reclaim_acquires_after_removing_stale_file() {
        let (_dir, path) = test_lock_path();
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should claim the reaped stale lock");
        let metadata = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn dead_same_host_reclaim_token_is_reaped_with_stale_lock() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let token_path = write_reclaim_token(&path, &stale);

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("dead token must not wedge stale-lock reclamation");
        assert!(!token_path.exists(), "stale reclaim token must be removed");
        drop(guard);
        assert!(!path.exists(), "acquired lock must be released normally");
    }

    #[test]
    fn live_reclaim_token_blocks_and_is_untouched() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let live = current_process_metadata();
        let token_path = write_reclaim_token(&path, &live);

        let result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&token_path).expect("live token"), live);
    }

    #[test]
    fn foreign_host_reclaim_token_is_authoritative() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let mut foreign = stale.clone();
        foreign.hostname = format!("{}-foreign", current_hostname());
        let token_path = write_reclaim_token(&path, &foreign);

        let result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(
            read_lock_metadata(&token_path).expect("foreign token"),
            foreign
        );
    }

    #[test]
    fn malformed_reclaim_token_is_held_until_older_than_sixty_seconds() {
        use filetime::{set_file_mtime, FileTime};

        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let token_path = reclaim_token_path(&path);
        fs::write(&token_path, b"{ malformed").expect("write malformed token");

        let young_result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(young_result, Err(AcquireError::Timeout)));
        assert_eq!(fs::read(&token_path).expect("young token"), b"{ malformed");

        let old = SystemTime::now()
            .checked_sub(RECLAIM_TOKEN_MALFORMED_STALE_AGE + Duration::from_secs(1))
            .expect("old timestamp");
        set_file_mtime(&token_path, FileTime::from_system_time(old)).expect("age token");
        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("old malformed token should be reclaimed");
        assert!(!token_path.exists());
        drop(guard);
    }

    #[test]
    fn held_reclaim_token_logs_blocked_without_claiming_lock_removal() {
        let (_dir, path) = test_lock_path();
        let stale = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &stale);
        let live = current_process_metadata();
        write_reclaim_token(&path, &live);
        take_reclaim_test_logs();

        let result = acquire_with_config(&path, Some(Duration::ZERO), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        let logs = take_reclaim_test_logs();
        assert!(logs.iter().any(|line| {
            line.contains(&format!("reclaim of {} blocked", path.display()))
                && line.contains(&format!("pid {} (alive)", live.pid))
        }));
        assert!(!logs
            .iter()
            .any(|line| line.contains("removing filesystem lock")));
    }

    #[test]
    fn dead_unreclaimable_lock_retry_backoff_stays_bounded() {
        let mut elapsed_ms = 0_u64;
        let mut attempts = 0_u32;
        while elapsed_ms < 60_000 {
            elapsed_ms = elapsed_ms.saturating_add(dead_reclaim_backoff_ms(attempts, 100));
            attempts += 1;
        }

        assert!(
            attempts <= 17,
            "{attempts} retries exceed the one-minute bound"
        );
        assert_eq!(dead_reclaim_backoff_ms(0, 100), 250);
        assert_eq!(dead_reclaim_backoff_ms(20, 100), 5_000);
    }

    #[test]
    fn maintenance_sweep_is_bounded_to_known_lock_domains() {
        let root = tempfile::tempdir().expect("temporary storage root");
        let cache_dir = root.path().join("index").join("project");
        fs::create_dir_all(&cache_dir).expect("create nested cache");
        let dead = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        let dead_token = write_reclaim_token(&cache_dir.join("cache.lock"), &dead);
        let live = current_process_metadata();
        let live_token = write_reclaim_token(&cache_dir.join("live.lock"), &live);
        let backup_session = root.path().join("opencode/backups/session");
        let backup_entry = backup_session.join("path_hash");
        let backup_locks = backup_session.join(".locks");
        fs::create_dir_all(&backup_entry).expect("create backup entry tree");
        fs::create_dir_all(&backup_locks).expect("create backup lock directory");
        let unrelated_token = write_reclaim_token(&backup_entry.join("foo.lock"), &dead);
        let backup_token = write_reclaim_token(&backup_locks.join("x.lock"), &dead);
        let cadence = Mutex::new(None);

        let removed = sweep_stale_reclaim_tokens_at(&[root.path()], Instant::now(), &cadence)
            .expect("sweep tokens");

        assert_eq!(removed, Some(2));
        assert!(!dead_token.exists());
        assert!(
            !backup_token.exists(),
            "BackupStore lock tokens must be swept"
        );
        assert_eq!(read_lock_metadata(&live_token).expect("live token"), live);
        assert!(
            unrelated_token.exists(),
            "maintenance must not descend into backup entry trees"
        );
    }

    /// With `storage.cache_dir` set, cache-domain locks (and so their reclaim
    /// tokens) are under the cache root while durable-state locks stay under
    /// the storage root. One pass must sweep both roots.
    #[test]
    fn maintenance_sweep_covers_the_cache_root_and_the_durable_root() {
        let durable = tempfile::tempdir().expect("temporary durable root");
        let cache = tempfile::tempdir().expect("temporary cache root");
        let dead = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        let index_dir = cache.path().join("index").join("project");
        fs::create_dir_all(&index_dir).expect("create cache index");
        let cache_token = write_reclaim_token(&index_dir.join("cache.lock"), &dead);
        let checkpoints = durable.path().join("checkpoints").join("project");
        fs::create_dir_all(&checkpoints).expect("create checkpoints");
        let durable_token = write_reclaim_token(&checkpoints.join("store.lock"), &dead);
        let cadence = Mutex::new(None);

        let removed = sweep_stale_reclaim_tokens_at(
            &[cache.path(), durable.path()],
            Instant::now(),
            &cadence,
        )
        .expect("sweep tokens");

        assert_eq!(removed, Some(2));
        assert!(!cache_token.exists(), "cache-root token kept");
        assert!(!durable_token.exists(), "durable-root token kept");
    }

    #[test]
    fn maintenance_sweep_runs_first_then_obeys_daily_cadence() {
        let root = tempfile::tempdir().expect("temporary storage root");
        let cache_dir = root.path().join("index").join("project");
        fs::create_dir_all(&cache_dir).expect("create index cache");
        let dead = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        let first_token = write_reclaim_token(&cache_dir.join("cache.lock"), &dead);
        let cadence = Mutex::new(None);
        let start = Instant::now();

        assert_eq!(
            sweep_stale_reclaim_tokens_at(&[root.path()], start, &cadence).expect("first sweep"),
            Some(1)
        );
        assert!(!first_token.exists());

        let second_token = write_reclaim_token(&cache_dir.join("cache.lock"), &dead);
        assert_eq!(
            sweep_stale_reclaim_tokens_at(
                &[root.path()],
                start + Duration::from_secs(60),
                &cadence,
            )
            .expect("suppressed sweep"),
            None
        );
        assert!(second_token.exists());
        assert_eq!(
            sweep_stale_reclaim_tokens_at(
                &[root.path()],
                start + RECLAIM_TOKEN_SWEEP_INTERVAL + Duration::from_secs(1),
                &cadence,
            )
            .expect("next-day sweep"),
            Some(1)
        );
        assert!(!second_token.exists());
    }

    #[test]
    fn stale_heartbeat_from_live_pid_blocks() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let identity = process_identity(std::process::id())
                .expect("current process should have a start-time identity");
            assert_eq!(metadata.process_start_time, Some(identity.start_time));
            assert_eq!(metadata.boot_id, identity.boot_id);
        }
        metadata.created_at_ms = now_ms().saturating_sub(60_000);
        metadata.heartbeat_at_ms = now_ms().saturating_sub(60_000);
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[test]
    fn availability_probe_preserves_a_live_owner_with_stale_heartbeat() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        metadata.heartbeat_at_ms = 0;
        write_synthetic_lock(&path, &metadata);
        let original = fs::read(&path).unwrap();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::LiveOwner
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn availability_probe_observes_dead_and_missing_owners_without_reclaiming() {
        let (_dir, path) = test_lock_path();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::Available
        );
        assert!(!path.exists());
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        write_synthetic_lock(&path, &metadata);
        let original = fs::read(&path).unwrap();
        assert_eq!(lock_availability(&path).unwrap(), LockAvailability::Stale);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn availability_probe_uses_extended_cross_host_heartbeat_policy() {
        let (_dir, path) = test_lock_path();
        let mut metadata = synthetic_metadata(
            999_999_999,
            format!("other-{}", current_hostname()),
            now_ms(),
        );
        write_synthetic_lock(&path, &metadata);
        assert!(
            lock_availability(&path).unwrap() == LockAvailability::LiveOwner,
            "a local PID probe cannot attest a remote owner"
        );
        metadata.heartbeat_at_ms = now_ms().saturating_sub(STALE_HEARTBEAT_MS * 2);
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert!(
            lock_availability(&path).unwrap() == LockAvailability::LiveOwner,
            "cross-host reclamation needs the extended threshold"
        );
        metadata.heartbeat_at_ms = now_ms().saturating_sub(STALE_HEARTBEAT_MS * 5 + 1_000);
        fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert_eq!(lock_availability(&path).unwrap(), LockAvailability::Stale);
        assert_eq!(read_lock_metadata(&path).unwrap(), metadata);
    }

    #[test]
    fn availability_probe_does_not_treat_malformed_metadata_as_available() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"not-json").unwrap();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::Malformed
        );
        assert_eq!(fs::read(&path).unwrap(), b"not-json");
    }

    #[test]
    fn availability_probe_keeps_io_errors_distinct_and_can_retry() {
        let (_dir, path) = test_lock_path();
        fs::create_dir(&path).unwrap();
        assert!(lock_availability(&path).is_err());
        assert!(path.is_dir());
        fs::remove_dir(&path).unwrap();
        assert_eq!(
            lock_availability(&path).unwrap(),
            LockAvailability::Available
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn live_pid_with_wrong_start_time_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        let current_identity = process_identity(std::process::id())
            .expect("current process should have a start-time identity");
        metadata.process_start_time = Some(different_start_time(current_identity.start_time));
        metadata.boot_id = current_identity.boot_id;
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should reclaim a reused PID");
        assert_eq!(guard.metadata.pid, std::process::id());
        assert_eq!(
            guard.metadata.process_start_time,
            Some(current_identity.start_time)
        );
        drop(guard);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_pid_with_wrong_boot_id_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        let current_identity = process_identity(std::process::id())
            .expect("current process should have a start-time identity");
        metadata.boot_id = Some(format!("wrong-{}", current_identity.boot_id.unwrap()));
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should reclaim a rebooted PID identity");
        assert_eq!(guard.metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn legacy_live_pid_lock_keeps_pid_only_liveness() {
        let (_dir, path) = test_lock_path();
        let stale_at = now_ms().saturating_sub(60_000);
        let mut metadata = synthetic_metadata(std::process::id(), current_hostname(), stale_at);
        metadata.heartbeat_at_ms = stale_at;
        let original = write_legacy_lock(&path, &metadata);
        assert!(!original.contains("process_start_time"));
        assert!(!original.contains("boot_id"));

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(
            fs::read_to_string(&path).expect("read legacy lock"),
            original
        );

        remove_lock_file(&path).expect("cleanup legacy lock");
    }

    #[test]
    fn legacy_dead_pid_lock_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let metadata = synthetic_metadata(999_999_999, current_hostname(), now_ms());
        let original = write_legacy_lock(&path, &metadata);
        assert!(!original.contains("process_start_time"));
        assert!(!original.contains("boot_id"));

        let guard = acquire_with_config(&path, Some(Duration::ZERO), test_config())
            .expect("zero-timeout acquire should reclaim a legacy dead PID lock");
        assert_eq!(guard.metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn healthy_live_owner_blocks() {
        let (_dir, path) = test_lock_path();
        let metadata = current_process_metadata();
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[test]
    fn malformed_lockfile_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        fs::write(&path, b"not valid json").expect("write malformed lock");

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("reclaim malformed lock");
        let metadata = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(metadata.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn cross_host_lock_is_not_stolen_before_extended_stale_threshold() {
        let (_dir, path) = test_lock_path();
        let now = now_ms();
        let mut metadata = current_process_metadata();
        metadata.hostname = format!("{}-other", current_hostname());
        metadata.process_start_time = metadata.process_start_time.map(different_start_time);
        metadata.created_at_ms = now;
        metadata.heartbeat_at_ms = now;
        metadata.writer_epoch = format!("cross-host-{now}");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert_ne!(
            metadata.process_start_time,
            process_identity(std::process::id()).map(|identity| identity.start_time)
        );
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[test]
    fn stale_cross_host_lock_is_reclaimed_after_extended_threshold() {
        let (_dir, path) = test_lock_path();
        let stale_at =
            now_ms().saturating_sub(test_config().cross_host_stale_heartbeat_ms() + 1_000);
        let mut metadata = current_process_metadata();
        metadata.hostname = format!("{}-other", current_hostname());
        metadata.process_start_time = metadata.process_start_time.map(different_start_time);
        metadata.created_at_ms = stale_at;
        metadata.heartbeat_at_ms = stale_at;
        metadata.writer_epoch = format!("cross-host-{stale_at}");
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("reclaim stale cross-host lock");
        let reclaimed = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_eq!(reclaimed.hostname, current_hostname());
        assert_ne!(reclaimed.created_at_ms, metadata.created_at_ms);
        drop(guard);
    }

    #[test]
    fn live_owner_over_10min_warns_but_blocks() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        metadata.created_at_ms = now_ms().saturating_sub(11 * 60 * 1_000);
        metadata.heartbeat_at_ms = now_ms();
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct HeartbeatObservation {
        step: usize,
        observed_at_ms: u64,
        heartbeat_at_ms: Option<u64>,
        volume: Option<u64>,
        inode: Option<u64>,
        modified_at_ms: Option<u64>,
        error: Option<String>,
    }

    fn observe_heartbeat(path: &Path, step: usize) -> HeartbeatObservation {
        let mut observation = HeartbeatObservation {
            step,
            observed_at_ms: now_ms(),
            heartbeat_at_ms: None,
            volume: None,
            inode: None,
            modified_at_ms: None,
            error: None,
        };
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) => {
                observation.error = Some(error.to_string());
                return observation;
            }
        };
        // Identity and contents come from the same open, including if the path
        // is atomically replaced while the observer is reading it.
        if let Some(identity) = LeaseFileIdentity::of_file(&file) {
            observation.volume = Some(identity.volume);
            observation.inode = Some(identity.file);
        }
        observation.modified_at_ms = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64);
        let mut bytes = Vec::new();
        match file.read_to_end(&mut bytes) {
            Ok(_) => match serde_json::from_slice::<LockMetadata>(&bytes) {
                Ok(metadata) => observation.heartbeat_at_ms = Some(metadata.heartbeat_at_ms),
                Err(error) => observation.error = Some(error.to_string()),
            },
            Err(error) => observation.error = Some(error.to_string()),
        }
        observation
    }

    struct HeartbeatHarness {
        child: Child,
        input: Option<ChildStdin>,
        lines: mpsc::Receiver<String>,
        reader: Option<JoinHandle<()>>,
        transcript: Vec<String>,
    }

    impl HeartbeatHarness {
        fn read_observation(
            &mut self,
            prefix: &str,
            deadline: Instant,
        ) -> Result<HeartbeatObservation, String> {
            loop {
                let line = self
                    .lines
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|error| format!("observer response: {error}"))?;
                self.transcript.push(line.clone());
                if let Some(json) = line.strip_prefix(prefix) {
                    return serde_json::from_str(json)
                        .map_err(|error| format!("observer JSON: {error}"));
                }
            }
        }

        fn observe(
            &mut self,
            step: usize,
            deadline: Instant,
        ) -> Result<HeartbeatObservation, String> {
            let input = self.input.as_mut().expect("observer input is open");
            writeln!(input, "{step}").map_err(|error| error.to_string())?;
            input.flush().map_err(|error| error.to_string())?;
            self.read_observation("sample:", deadline)
        }

        fn diagnostics(&self, path: &Path, guard: &LockGuard, step: usize) -> String {
            let held_identity = guard.heartbeat_file.try_lock().ok().and_then(|mut slot| {
                slot.as_mut()
                    .and_then(|lease| LeaseFileIdentity::of_file(lease.file()))
            });
            format!(
                "parent={:?} held_identity={held_identity:?} stopped={} visible={}\nchild observations:\n{}",
                observe_heartbeat(path, step),
                guard.pump.heartbeat_stopped(),
                guard.heartbeat_is_visible(),
                self.transcript.join("\n")
            )
        }

        fn finish(&mut self) -> std::process::ExitStatus {
            self.input.take();
            let status = self.child.wait().expect("observer exit");
            self.reader.take().unwrap().join().expect("observer reader");
            self.transcript.extend(self.lines.try_iter());
            status
        }
    }

    impl Drop for HeartbeatHarness {
        fn drop(&mut self) {
            // An assertion can unwind before normal EOF/wait cleanup. Reap the
            // observer and join its stdout reader before the owner releases the
            // lease, so no child retains the test runner's inherited stderr.
            self.input.take();
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    /// Run the Rust test binary as an independent lease observer. Each request
    /// opens the lease afresh and returns its content timestamp and file identity.
    fn spawn_test_harness(mode: &str, path: &Path) -> HeartbeatHarness {
        let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
        command
            .args(["--exact", HARNESS_TEST, "--nocapture", "--test-threads=1"])
            .env("FS_LOCK_HARNESS_MODE", mode)
            .env("FS_LOCK_HARNESS_PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for name in [
            "AFT_TEST_REQUIRE_RUST_ANALYZER",
            "AFT_TEST_STORAGE_ROOT",
            "AFT_STORAGE_DIR",
        ] {
            command.env_remove(name);
        }
        let mut child = command.spawn().expect("spawn test harness process");
        let stdin = child.stdin.take().expect("harness stdin");
        let stdout = child.stdout.take().expect("harness stdout");
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        HeartbeatHarness {
            child,
            input: Some(stdin),
            lines: receiver,
            reader: Some(reader),
            transcript: Vec::new(),
        }
    }

    /// Another process must see background heartbeats through the kept handle.
    /// A beat interval is not a scheduling guarantee: allow progress within the
    /// configured stale bound, and never substitute a parent-side read for the
    /// observer's actual timestamp and inode.
    #[test]
    fn separate_process_sees_heartbeat_advance() {
        if std::env::var_os(HARNESS_TEST).is_some() {
            return;
        }
        assert_separate_process_heartbeat_advances(test_config());
    }

    #[test]
    fn separate_process_waits_for_heartbeat_within_stale_bound() {
        assert_separate_process_heartbeat_advances(LockConfig {
            heartbeat_interval_ms: 400,
            ..test_config()
        });
    }

    fn assert_separate_process_heartbeat_advances(config: LockConfig) {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, config).expect("acquire lock");
        guard.heartbeat_once().expect("initial beat");
        let mut harness = spawn_test_harness(HARNESS_MODE_INSPECT, &path);
        let ready = harness
            .read_observation("ready:", Instant::now() + Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "observer startup failed: {error}\n{}",
                    harness.diagnostics(&path, &guard, 0)
                )
            });
        let mut previous = ready.heartbeat_at_ms.unwrap_or_else(|| {
            panic!(
                "observer could not read initial lease\n{}",
                harness.diagnostics(&path, &guard, 0)
            )
        });
        for step in 1..=4 {
            let deadline = Instant::now() + Duration::from_millis(config.stale_heartbeat_ms);
            loop {
                let observation = harness.observe(step, deadline).unwrap_or_else(|error| {
                    panic!(
                        "observer step {step} failed: {error}\n{}",
                        harness.diagnostics(&path, &guard, step)
                    )
                });
                if let Some(seen) = observation.heartbeat_at_ms.filter(|seen| *seen > previous) {
                    eprintln!("heartbeat observer advanced: {observation:?}");
                    previous = seen;
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "observer heartbeat did not advance from {previous} within {}ms at step {step}\n{}",
                    config.stale_heartbeat_ms,
                    harness.diagnostics(&path, &guard, step)
                );
                thread::sleep(
                    Duration::from_millis(25)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        let status = harness.finish();
        assert!(
            status.success(),
            "observer failed: {status:?}\n{}",
            harness.diagnostics(&path, &guard, 4)
        );
    }

    #[cfg(unix)]
    #[test]
    fn separate_process_harness_drop_reaps_observer() {
        let (_dir, path) = test_lock_path();
        write_synthetic_lock(&path, &current_process_metadata());
        let mut harness = spawn_test_harness(HARNESS_MODE_INSPECT, &path);
        harness
            .read_observation("ready:", Instant::now() + Duration::from_secs(10))
            .expect("observer ready");
        let pid = harness.child.id() as libc::pid_t;
        drop(harness);
        let mut status = 0;
        // SAFETY: pid names the child this test just spawned; status is a valid
        // out pointer. A completed Drop must already have waited for this child.
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        let error = io::Error::last_os_error().raw_os_error();
        if waited == 0 {
            // SAFETY: only our still-running child can be waitable under pid.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
        }
        assert_eq!(waited, -1, "observer survived harness drop");
        assert_eq!(error, Some(libc::ECHILD), "observer was not reaped");
    }

    /// A replacement by another part of this process must leave the owner's
    /// heartbeat working: writers keep publishing through the same guard, and a
    /// heartbeat that stopped would let another process reclaim the lease after
    /// the stale window while the writer is still running. The test replaces the
    /// lease (which releases the published handle), then opens a fresh handle
    /// and publishes it, and requires the next beat to refresh through it.
    #[test]
    fn replacing_the_lease_keeps_the_next_beat_working_for_the_owner() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire");
        let mut restored = guard.metadata.clone();
        // Same digit count as the live heartbeat, so the serialized lease keeps
        // its byte length; the replacement itself is still an atomic rename.
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000);
        let fresh = {
            let bytes = lock_metadata_bytes(&restored).expect("serialize");
            with_lease_handles_closed(&path, || {
                replace_lock_bytes_unsynced(&path, &bytes).map(|()| open_lease_for_heartbeat(&path))
            })
            .expect("replace the lease")
            .expect("open the replacement lease")
        };
        // The owner keeps a reference, as `LockGuard` does, so the handle
        // survives until the guard is released.
        let replacement = Arc::new(Mutex::new(Some(fresh)));
        register_lease_slot(guard.pump.slot, &path, &replacement);

        let stale_at = restored.heartbeat_at_ms;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut advanced = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(
                test_config().heartbeat_interval_ms * 2,
            ));
            if read_lock_metadata(&path)
                .map(|meta| meta.heartbeat_at_ms > stale_at)
                .unwrap_or(false)
            {
                advanced = true;
                break;
            }
        }
        assert!(
            advanced,
            "the heartbeat did not refresh the lease through the replacement \
             handle: {:?}",
            read_lock_metadata(&path)
        );
        assert!(
            guard.verify_writer_epoch().expect("verify lease"),
            "writers must still verify their epoch after a replacement"
        );
    }

    /// Body of the separate-process harness this module's tests spawn.
    #[test]
    fn fs_lock_separate_process_harness() {
        let Some(mode) = std::env::var_os("FS_LOCK_HARNESS_MODE") else {
            return;
        };
        let path = PathBuf::from(std::env::var_os("FS_LOCK_HARNESS_PATH").expect("harness path"));
        println!("mode:{mode:?} path={}", path.display());
        match mode.to_string_lossy().as_ref() {
            HARNESS_MODE_INSPECT => {
                println!(
                    "ready:{}",
                    serde_json::to_string(&observe_heartbeat(&path, 0)).unwrap()
                );
                io::stdout().flush().unwrap();
                for line in io::stdin().lock().lines() {
                    let step = line.unwrap().parse().expect("observation step");
                    println!(
                        "sample:{}",
                        serde_json::to_string(&observe_heartbeat(&path, step)).unwrap()
                    );
                    io::stdout().flush().unwrap();
                }
            }
            other => panic!("unknown harness mode {other}"),
        }
    }

    /// A beat reads ownership through the descriptor the heartbeat keeps, not
    /// through the path. When another process reclaims the lease, the rename
    /// gives the path a new file while the kept descriptor still refers to the
    /// old, unlinked one. Reading through it would still see this owner's
    /// identity, so the beat would refresh the orphan and report success while
    /// the live lease went stale. The beat must instead see that the path is no
    /// longer the file it holds and stop.
    #[test]
    fn beat_stops_when_another_owner_replaces_the_lease_file() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire");
        guard
            .heartbeat_once()
            .expect("beat through the kept descriptor");

        // Another process's reclaim: it writes its own lease beside the file and
        // renames it over the path. It cannot see this process's open
        // descriptors, so the heartbeat keeps the old, now unlinked file.
        let mut foreign = current_process_metadata();
        foreign.pid = foreign.pid.wrapping_add(1);
        foreign.created_at_ms = now_ms().saturating_add(1_000);
        foreign.writer_epoch = format!("foreign-{}", now_ms());
        assert!(!lock_identity_matches(&foreign, &guard.metadata));
        let staging = path.with_extension("foreign");
        fs::write(
            &staging,
            lock_metadata_bytes(&foreign).expect("serialize foreign lease"),
        )
        .expect("write the foreign owner's lease");
        rename_over(&staging, &path).expect("foreign reclaim renames over the lease");

        assert!(
            matches!(guard.heartbeat_once(), Err(HeartbeatError::NotOwner)),
            "a beat must not accept the orphaned descriptor it still holds"
        );
        assert_eq!(
            read_lock_metadata(&path).expect("read foreign lease"),
            foreign,
            "a beat must never write over the foreign owner's lease"
        );
    }

    /// Replacing or removing another lease must not cost this lease its kept
    /// descriptor. Every replacement releases the handle it publishes for its
    /// own path; releasing every published handle instead would drop this one
    /// and the next beat would reopen the file, restoring the per-beat
    /// descriptor churn the kept handle exists to remove.
    #[test]
    fn unrelated_lease_churn_keeps_the_kept_descriptor() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mut guards = (0..2)
            .map(|index| {
                acquire_with_config(
                    &dir.path().join(format!("tenant-{index}.lease")),
                    Some(Duration::from_secs(2)),
                    test_config(),
                )
                .expect("acquire")
            })
            .collect::<Vec<_>>();
        let neighbour = dir.path().join("neighbour.lease");

        for _ in 0..3 {
            // Another tenant is acquired, replaced and released while this
            // lease keeps its own descriptor.
            let churn = acquire_with_config(&neighbour, None, test_config()).expect("churn lease");
            let mut restored = churn.metadata.clone();
            restored.heartbeat_at_ms = now_ms().saturating_sub(1_000);
            let bytes = lock_metadata_bytes(&restored).expect("serialize");
            with_lease_handles_closed(&neighbour, || {
                replace_lock_bytes_unsynced(&neighbour, &bytes)
            })
            .expect("replace the neighbour");
            drop(churn);
            let _ = io_ledger::take();
        }

        let _ = io_ledger::take();
        for guard in &guards {
            guard.heartbeat_once().expect("beat");
        }
        let cost = io_ledger::take();
        assert_beat_opens_no_descriptor(&cost);
        guards.clear();
    }

    /// The identity check runs on every beat, so it must not open or close the
    /// lease a second time. Unix needs a stat for it; on Windows the path lookup
    /// takes one short-lived attribute handle, which this ledger only counts if
    /// it went through the heartbeat's own open. What it pins is that the beat
    /// neither reopens the lease for reading nor closes the descriptor it
    /// keeps.
    #[test]
    fn descriptor_identity_check_opens_no_descriptor() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire");
        let _ = io_ledger::take();
        for _ in 0..3 {
            guard.heartbeat_once().expect("beat");
        }
        let ledger = io_ledger::take();
        assert_beat_opens_no_descriptor(&ledger);
    }

    #[test]
    fn stopped_heartbeat_releases_handle_without_unlocking_or_touching_replacement() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).expect("cache dir");
        let path = cache.join("writer.lease");
        let mut guard = acquire_with_config(&path, None, test_config()).expect("acquire lease");
        let _ = io_ledger::take();

        guard.stop_heartbeat_for_removal();
        assert_eq!(io_ledger::take().closes, 1, "close the kept handle now");
        assert!(guard.heartbeat.is_none(), "join before closing the handle");
        assert!(guard.heartbeat_file.lock().unwrap().is_none());
        assert!(!guard
            .verify_writer_epoch()
            .expect("stopped guard is not writable"));
        assert!(matches!(
            guard.pump.heartbeat_once(),
            Err(HeartbeatError::NotPumped)
        ));
        assert!(matches!(
            try_acquire_once(&path),
            Err(AcquireError::Timeout)
        ));
        assert!(lock_identity_matches(
            &read_lock_metadata(&path).expect("lease remains until removal"),
            &guard.metadata
        ));

        // Windows refuses this directory rename while the heartbeat handle is
        // open. Keep the lease on disk until the directory has moved so another
        // writer cannot enter the old cache during retirement.
        let retired = dir.path().join("retired");
        fs::rename(&cache, &retired).expect("retire with the lease guard still alive");
        fs::create_dir(&cache).expect("replacement cache");
        let replacement = acquire_with_config(&path, None, test_config()).expect("new owner");
        let canary = cache.join("canary");
        fs::write(&canary, b"replacement").expect("canary");
        drop(guard);
        assert!(replacement
            .verify_writer_epoch()
            .expect("replacement owns lease"));
        assert_eq!(
            fs::read(canary).expect("replacement preserved"),
            b"replacement"
        );
        fs::remove_dir_all(retired).expect("reap without a lingering lease handle");
    }

    #[test]
    fn drop_stops_heartbeat_thread() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        drop(guard);

        thread::sleep(Duration::from_millis(
            test_config().heartbeat_interval_ms * 3,
        ));
        assert!(
            !path.exists(),
            "heartbeat recreated or kept updating lockfile"
        );
    }

    #[test]
    fn heartbeat_error_classification_terminal_vs_transient() {
        // Terminal: the lock is provably no longer ours to refresh.
        assert!(heartbeat_error_is_terminal(&HeartbeatError::LockGone));
        assert!(heartbeat_error_is_terminal(&HeartbeatError::NotOwner));
        // Transient: a temporary I/O hiccup or a read that raced a concurrent
        // writer. These must NOT kill the heartbeat — it retries instead.
        assert!(!heartbeat_error_is_terminal(&HeartbeatError::Io(
            io::Error::other("disk blip")
        )));
        let malformed: serde_json::Error =
            serde_json::from_str::<LockMetadata>("not json").unwrap_err();
        assert!(!heartbeat_error_is_terminal(&HeartbeatError::Malformed(
            malformed
        )));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn released_lease_closes_all_descriptors_synchronously() {
        let (dir, path) = test_lock_path();
        let open_under_root = || {
            fs::read_dir("/proc/self/fd")
                .expect("descriptor census")
                .filter_map(Result::ok)
                .filter_map(|entry| fs::read_link(entry.path()).ok())
                .filter(|target| target.starts_with(dir.path()))
                .collect::<Vec<_>>()
        };
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lease");
        let handle = Arc::downgrade(&guard.heartbeat_file);
        assert!(!open_under_root().is_empty(), "live lease keeps its handle");
        drop(guard);
        assert!(
            handle.upgrade().is_none(),
            "heartbeat must release its handle"
        );
        assert!(open_under_root().is_empty(), "no descriptors after release");
    }

    #[test]
    fn malformed_beat_reopens_atomic_replacement_on_next_beat() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(
            &path,
            None,
            LockConfig {
                heartbeat_interval_ms: 60_000,
                ..test_config()
            },
        )
        .expect("acquire lease");
        let live = fs::read(&path).expect("read lease");
        fs::write(&path, vec![b'x'; live.len()]).expect("corrupt lease");

        assert!(matches!(
            guard.pump.heartbeat_once(),
            Err(HeartbeatError::Malformed(_))
        ));
        assert!(guard.heartbeat_file.lock().unwrap().is_none());
        assert!(guard.heartbeat_is_visible());

        let mut restored = guard.metadata.clone();
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000_000);
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, lock_metadata_bytes(&restored).unwrap()).unwrap();
        rename_over(&replacement, &path).expect("atomic recovery outside the lease registry");
        let _ = io_ledger::take();
        guard
            .pump
            .heartbeat_once()
            .expect("retry through live inode");
        let recovered = read_lock_metadata(&path).expect("independently read recovered lease");
        assert!(lock_identity_matches(&recovered, &guard.metadata));
        assert!(recovered.heartbeat_at_ms > restored.heartbeat_at_ms);
        let recovery = io_ledger::take();
        assert_eq!((recovery.opens, recovery.closes), (1, 0));

        guard.pump.heartbeat_once().expect("healthy next beat");
        assert_beat_opens_no_descriptor(&io_ledger::take());
    }

    #[test]
    fn valid_same_owner_replacement_refreshes_within_stale_bound() {
        let (_dir, path) = test_lock_path();
        let config = test_config();
        let guard = acquire_with_config(
            &path,
            None,
            LockConfig {
                heartbeat_interval_ms: 60_000,
                ..config
            },
        )
        .expect("acquire lease");
        let held_identity = LeaseFileIdentity::of_file(
            guard
                .heartbeat_file
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .file(),
        );
        let mut restored = guard.metadata.clone();
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000_000);
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, lock_metadata_bytes(&restored).unwrap()).unwrap();
        // Do not release or re-index the kept handle: an atomic replacement by
        // another process cannot notify this process's lease registry either.
        rename_over(&replacement, &path).expect("replace valid same-owner lease");
        let live_identity = LeaseFileIdentity::of_path(&path).expect("replacement identity");
        assert_ne!(held_identity, Some(live_identity));

        let _ = io_ledger::take();
        let started = Instant::now();
        let result = guard.pump.heartbeat_once();
        let elapsed = started.elapsed();
        let on_disk = read_lock_metadata(&path).expect("independent path read");
        let cost = io_ledger::take();
        eprintln!(
            "valid replacement beat: elapsed={elapsed:?} result={result:?} held={held_identity:?} live={live_identity:?} sentinel={} observed={} cost={cost:?}",
            restored.heartbeat_at_ms, on_disk.heartbeat_at_ms
        );
        assert!(
            elapsed < Duration::from_millis(config.stale_heartbeat_ms),
            "same-owner replacement delayed a beat beyond its stale bound: {elapsed:?}"
        );
        result.expect("refresh replacement through reopened handle");
        assert!(on_disk.heartbeat_at_ms > restored.heartbeat_at_ms);
        assert!(lock_identity_matches(&on_disk, &guard.metadata));
        assert_eq!((cost.opens, cost.closes), (1, 1));
        let reopened_identity = LeaseFileIdentity::of_file(
            guard
                .heartbeat_file
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .file(),
        );
        assert_eq!(reopened_identity, Some(live_identity));
        guard.pump.heartbeat_once().expect("healthy next beat");
        assert_beat_opens_no_descriptor(&io_ledger::take());
    }

    #[test]
    fn heartbeat_survives_transient_malformed_and_recovers() {
        // Regression: a single transient failure (e.g. a read that races a
        // concurrent writer and sees a momentarily-unparseable file) used to
        // permanently kill the heartbeat thread. The guard holder would then
        // run its critical section with a stale heartbeat_at_ms, letting
        // another process reclaim the lock after the stale window — concurrent
        // writers / split-brain. The heartbeat must instead retry and resume
        // refreshing once the file is readable again.
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let owner = guard.metadata.clone();

        // Corrupt the lockfile out from under the heartbeat (simulates a
        // concurrent-writer race producing a momentarily-unparseable read).
        // The heartbeat reads-then-writes, so it observes Malformed and, with
        // the fix, retries instead of dying. The corruption keeps the lease's
        // byte length: a length change takes the process-wide replacement path,
        // which releases the beat's kept handle and leaves the ownership check
        // to the next open, and this test is about retrying a read that failed.
        let live = fs::read(&path).expect("read live lease");
        fs::write(&path, vec![b'x'; live.len()]).expect("corrupt lockfile");

        // Give the heartbeat several intervals to observe the malformed file.
        // Pre-fix, the thread is dead by now.
        thread::sleep(Duration::from_millis(
            test_config().heartbeat_interval_ms * 4,
        ));

        // Restore the owner's own lease bytes with a clearly-stale heartbeat
        // timestamp. The replacement bytes keep the lease's byte length, so the
        // next beat takes its in-place path. The restore's timestamp only has to
        // be older than the beat that follows, so a value from before the
        // corruption is enough.
        let mut restored = owner.clone();
        restored.heartbeat_at_ms = now_ms().saturating_sub(1_000_000);
        let sentinel = restored.heartbeat_at_ms;
        // Use an atomic replacement to exercise reopening the lease, rather
        // than only rewriting the inode that the heartbeat previously held.
        {
            let bytes = lock_metadata_bytes(&restored).expect("serialize restored metadata");
            with_lease_handles_closed(&path, || replace_lock_bytes_unsynced(&path, &bytes))
                .expect("restore lock metadata");
        }

        // If the heartbeat thread is still alive (the fix), it will overwrite
        // heartbeat_at_ms with a current value. Poll for that recovery. The
        // window covers the beat that finds the restored lease but is still
        // failing on the earlier corrupt state, plus several retries after it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut recovered = false;
        while std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
            match read_lock_metadata(&path) {
                Ok(meta)
                    if meta.created_at_ms == owner.created_at_ms
                        && meta.heartbeat_at_ms > sentinel =>
                {
                    recovered = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(
            recovered,
            "heartbeat did not recover after a transient malformed read — thread likely died: \
             stopped={} visible={} on_disk={:?}",
            guard.pump.heartbeat_stopped(),
            guard.heartbeat_is_visible(),
            read_lock_metadata(&path)
        );
        drop(guard);
    }
}
