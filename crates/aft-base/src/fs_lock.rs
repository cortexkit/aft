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
/// Successful refreshes need not rewrite every wake. Foreign hosts allow 75s
/// of silence, leaving 50s of scheduling/I/O headroom after a normal refresh.
/// Same-host readers use process identity, never heartbeat age, for takeover.
pub const HEARTBEAT_REFRESH_INTERVAL_MS: u64 = 25_000;
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
// compress::trust owns this one lock directly under the storage root.
// BackupStore scopes locks below each harness and session. Prefixes cover the
// client-specific MCP and fingerprint-specific federated harness directories.

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
    heartbeat_refresh_interval_ms: u64,
    stale_heartbeat_ms: u64,
    live_owner_warn_ms: u64,
    poll_interval_ms: u64,
}

impl LockConfig {
    fn cross_host_stale_heartbeat_ms(self) -> u64 {
        self.stale_heartbeat_ms.saturating_mul(5)
    }

    fn live_owner_warning_age_ms(self) -> u64 {
        // A healthy lease can now be older than the old warning window. Allow
        // one wake of jitter without changing the foreign-host takeover bound.
        self.stale_heartbeat_ms.max(
            self.heartbeat_refresh_interval_ms
                .saturating_add(self.heartbeat_interval_ms),
        )
    }
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: HEARTBEAT_INTERVAL_MS,
            heartbeat_refresh_interval_ms: HEARTBEAT_REFRESH_INTERVAL_MS,
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
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
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
        #[cfg(any(test, feature = "test-support"))]
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
                #[cfg(any(test, feature = "test-support"))]
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
                    slog_info!(
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

        if since_heartbeat > config.live_owner_warning_age_ms() && live_owner_log.first_stale() {
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
    // Anchor the first deadline before publication, not when the new thread
    // eventually gets CPU time. A late-starting thread must refresh immediately.
    let first_refresh =
        Instant::now() + Duration::from_millis(config.heartbeat_refresh_interval_ms);
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
                first_refresh,
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
    #[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static BEFORE_MALFORMED_RECLAIM: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_HEARTBEAT_SPAWN_FAILURE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
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
    #[cfg(any(test, feature = "test-support"))]
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
    mut next_refresh: Instant,
) {
    // Number of consecutive retry intervals before the warning window elapses.
    // Local takeover still requires proof of a dead or recycled process.
    // Beyond this point a sustained failure is genuinely dangerous, so we
    // escalate the log from warn to error — but we still keep retrying.
    let stale_intervals = config
        .stale_heartbeat_ms
        .checked_div(config.heartbeat_interval_ms.max(1))
        .unwrap_or(3)
        .max(1);
    let mut consecutive_transient_failures: u64 = 0;
    let refresh_interval = Duration::from_millis(config.heartbeat_refresh_interval_ms);

    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        match refresh_heartbeat_if_due(pump, &mut next_refresh, refresh_interval, Instant::now()) {
            Ok(false) => {}
            Ok(true) => {
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
                #[cfg(any(test, feature = "test-support"))]
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
        thread::park_timeout(Duration::from_millis(config.heartbeat_interval_ms));
    }
}

fn refresh_heartbeat_if_due(
    pump: &HeartbeatPump,
    next_refresh: &mut Instant,
    interval: Duration,
    now: Instant,
) -> Result<bool, HeartbeatError> {
    if now < *next_refresh {
        return Ok(false);
    }
    // A delayed wake refreshes immediately. Only success advances the deadline,
    // so transient failures retry at the short wake cadence, not another 25s.
    pump.heartbeat_once()?;
    *next_refresh = now + interval;
    Ok(true)
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
    #[cfg(any(test, feature = "test-support"))]
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
            #[cfg(any(test, feature = "test-support"))]
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
                #[cfg(any(test, feature = "test-support"))]
                stopped: Arc::new(AtomicBool::new(false)),
            },
            file,
        ))
    }

    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Whether the heartbeat thread ended by itself instead of at guard drop.
    #[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(windows, any(test, feature = "test-support")))]
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
    #[cfg(any(test, feature = "test-support"))]
    RECLAIM_TEST_LOGS.with(|logs| logs.borrow_mut().push(message.clone()));
    slog_warn!("{}", message);
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static RECLAIM_TEST_LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };

    // The observer is thread-local so concurrent lock tests cannot record one
    // another's retry decisions.
    static RETRY_SLEEP_OBSERVER: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicUsize>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct RetrySleepObserverGuard {
    previous: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for RetrySleepObserverGuard {
    fn drop(&mut self) {
        RETRY_SLEEP_OBSERVER.with(|observer| {
            *observer.borrow_mut() = self.previous.take();
        });
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn observe_retry_sleeps_for_test(
    observer: Arc<std::sync::atomic::AtomicUsize>,
) -> RetrySleepObserverGuard {
    let previous = RETRY_SLEEP_OBSERVER.with(|slot| slot.replace(Some(observer)));
    RetrySleepObserverGuard { previous }
}

#[cfg(any(test, feature = "test-support"))]
fn note_retry_sleep_for_test() {
    RETRY_SLEEP_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow().as_ref() {
            observer.fetch_add(1, Ordering::SeqCst);
        }
    });
}

pub(crate) fn acquisition_now() -> Instant {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(now) = retry_clock::now() {
        return now;
    }
    Instant::now()
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
    #[cfg(any(test, feature = "test-support"))]
    note_retry_sleep_for_test();
    #[cfg(any(test, feature = "test-support"))]
    if retry_clock::sleep(sleep_for) {
        return Ok(());
    }
    thread::sleep(sleep_for);
    Ok(())
}

/// Per-thread counters of successful acquisitions and lease I/O: file fsyncs,
/// directory fsyncs, newly created files (new inodes) and payload bytes.
/// Tests read them to prove which paths sync and how much a heartbeat
/// writes; outside tests every call compiles to nothing. The counters are
/// thread-local so tests running in parallel do not see each other's I/O.
pub(crate) mod io_ledger {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    #[cfg_attr(not(any(test, feature = "test-support")), allow(dead_code))]
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

    #[cfg(any(test, feature = "test-support"))]
    thread_local! {
        static LEDGER: std::cell::Cell<IoLedger> = std::cell::Cell::new(IoLedger::default());
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn record(update: impl FnOnce(&mut IoLedger)) {
        LEDGER.with(|cell| {
            let mut ledger = cell.get();
            update(&mut ledger);
            cell.set(ledger);
        });
    }

    #[cfg(not(any(test, feature = "test-support")))]
    #[inline(always)]
    pub(crate) fn record(_update: impl FnOnce(&mut IoLedger)) {}

    /// Return this thread's counters and reset them to zero.
    #[cfg(any(test, feature = "test-support"))]
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

    let identity = process_identity(metadata.pid);
    // An exited process cannot execute protected work, even while its parent
    // leaves it waitable. Missing or inconclusive identity still fails closed.
    if identity.as_ref().is_some_and(|identity| identity.exited) {
        return false;
    }
    let Some(recorded_start_time) = metadata.process_start_time else {
        return true;
    };
    let Some(current_identity) = identity else {
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
fn linux_process_stat_identity(stat: &str) -> Option<(u64, bool)> {
    // Fields 20 and 22 are num_threads and starttime. Split after the final ')'
    // because the command can contain spaces and parentheses. pthread_exit can
    // leave a zombie leader while other threads still execute protected work.
    // Z (zombie) or X (dead) proves exit only with one reported thread. Malformed
    // fields give no identity, so owner liveness remains conservative.
    let fields = stat.rsplit_once(") ")?.1.split_ascii_whitespace();
    let state = fields.clone().next()?;
    if state.len() != 1 {
        return None;
    }
    let num_threads = fields.clone().nth(17)?.parse::<u64>().ok()?;
    let start_time = fields.clone().nth(19)?.parse::<u64>().ok()?;
    Some((start_time, matches!(state, "Z" | "X") && num_threads == 1))
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (start_time, exited) = linux_process_stat_identity(&stat)?;
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
        exited,
    })
}

#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
const MACOS_KINFO_SIZE: usize = 648;
#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
pub(crate) const SZOMB: u8 = 5;
#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
pub(crate) const P_WEXIT: u32 = 0x0000_2000;

#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MacosExitState {
    NotExiting,
    Exiting,
    Zombie,
}

/// One kernel snapshot supplies both incarnation and exit evidence. libproc's
/// PROC_PIDTBSDINFO can fail for a zombie, which is not evidence of liveness.
#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
#[derive(Clone, Debug)]
pub(crate) struct MacosKinfo {
    pub(crate) comm: String,
    pub(crate) ruid: u32,
    pub(crate) pgid: i32,
    pub(crate) state: u8,
    pub(crate) flags: u32,
    pub(crate) started_at_us: Option<u64>,
}

#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
impl MacosKinfo {
    pub(crate) fn exit_state(&self) -> MacosExitState {
        if self.state == SZOMB {
            MacosExitState::Zombie
        } else if self.flags & P_WEXIT != 0 {
            MacosExitState::Exiting
        } else {
            MacosExitState::NotExiting
        }
    }

    /// The kernel exposes exit-started (P_WEXIT) before the process reaches the
    /// zombie state (SZOMB). Both count as exited for lease ownership and for
    /// deciding when a killed background task has finished.
    pub(crate) fn has_exited(&self) -> bool {
        self.exit_state() != MacosExitState::NotExiting
    }
}

#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
fn decode_macos_kinfo(bytes: &[u8]) -> Option<MacosKinfo> {
    // Darwin's kinfo_proc ABI is 648 bytes on arm64 and x86_64. libc does not
    // declare it for Apple targets; reject any other layout rather than guess.
    const P_FLAG_OFFSET: usize = 32;
    const P_STAT_OFFSET: usize = 36;
    const P_COMM_OFFSET: usize = 243;
    const P_COMM_LEN: usize = 17;
    const P_RUID_OFFSET: usize = 392;
    const E_PGID_OFFSET: usize = 564;
    if bytes.len() != MACOS_KINFO_SIZE {
        return None;
    }
    let read_u32 = |offset: usize| {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    };
    // extern_proc begins with p_un.p_starttime (timeval): 64-bit seconds and
    // 32-bit microseconds. Keep microsecond identity distinct from ms callers.
    let seconds = i64::from_ne_bytes(bytes[0..8].try_into().ok()?);
    let micros = i32::from_ne_bytes(bytes[8..12].try_into().ok()?);
    let started_at_us = if seconds > 0 && (0..1_000_000).contains(&micros) {
        (seconds as u64)
            .checked_mul(1_000_000)?
            .checked_add(micros as u64)
    } else {
        None
    };
    let comm = &bytes[P_COMM_OFFSET..P_COMM_OFFSET + P_COMM_LEN];
    let end = comm
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(comm.len());
    Some(MacosKinfo {
        comm: String::from_utf8_lossy(&comm[..end]).into_owned(),
        ruid: read_u32(P_RUID_OFFSET),
        pgid: read_u32(E_PGID_OFFSET) as i32,
        state: bytes[P_STAT_OFFSET],
        flags: read_u32(P_FLAG_OFFSET),
        started_at_us,
    })
}

/// kern.proc.pid also answers for other users and unreaped processes. A failed
/// or partial read is inconclusive, never a fabricated live/dead identity.
#[cfg(target_os = "macos")]
pub(crate) fn macos_kinfo(pid: u32) -> Option<MacosKinfo> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        pid as i32,
    ];
    let mut bytes = [0_u8; MACOS_KINFO_SIZE];
    let mut size = bytes.len();
    // SAFETY: sysctl receives the real buffer size and may write at most it.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size != bytes.len() {
        return None;
    }
    decode_macos_kinfo(&bytes)
}

#[cfg(any(target_os = "macos", any(test, feature = "test-support")))]
fn macos_process_identity(snapshot: Option<MacosKinfo>) -> Option<ProcessIdentity> {
    let info = snapshot?;
    Some(ProcessIdentity {
        start_time: info.started_at_us?,
        boot_id: None,
        exited: info.has_exited(),
    })
}

#[cfg(target_os = "macos")]
fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    macos_process_identity(macos_kinfo(pid))
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
    let boot = crate::volume::boot_id()?;
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

#[cfg(all(any(test, feature = "test-support"), target_os = "macos"))]
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
