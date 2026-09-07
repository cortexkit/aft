use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex, OnceLock,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{slog_debug, slog_error, slog_info, slog_warn};

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

/// Try one lock acquisition attempt, then check once whether an existing stale
/// lock can be taken over.
///
/// Read-only cache openers use this to switch to writer mode without waiting
/// behind another process that is still building the cache.
pub fn try_acquire_once(path: &Path) -> Result<LockGuard, AcquireError> {
    try_acquire(path, Duration::ZERO)
}

pub struct LockGuard {
    path: PathBuf,
    metadata: LockMetadata,
    shutdown: Arc<AtomicBool>,
    heartbeat_failed: Arc<AtomicBool>,
    heartbeat_done: mpsc::Receiver<()>,
    heartbeat: Option<JoinHandle<()>>,
}

impl LockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn writer_epoch(&self) -> &str {
        &self.metadata.writer_epoch
    }

    /// Re-read the lock file and confirm that this guard still owns the writer
    /// token. Writers call this right before saving published data or starting
    /// SQLite writes so they stop if another process has taken over the lock.
    pub fn verify_writer_epoch(&self) -> io::Result<bool> {
        if self.heartbeat_failed.load(Ordering::Acquire) {
            return Ok(false);
        }
        match read_lock_metadata(&self.path) {
            Ok(metadata) => Ok(lock_identity_matches(&metadata, &self.metadata)),
            Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(ReadLockError::Io(error)) => Err(error),
            Err(ReadLockError::Malformed(_)) => Ok(false),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Signal shutdown then unconditionally join the heartbeat thread
        // BEFORE removing the lockfile. The earlier `recv_timeout(100ms)`
        // implementation could let `remove_lock_if_owned` race with a
        // still-alive heartbeat:
        //
        //   1. Drop signals shutdown, ack times out under CI load.
        //   2. Drop calls `remove_lock_if_owned` → file removed.
        //   3. Another caller acquires the lock → writes its metadata.
        //   4. Our heartbeat (still alive, mid-`atomic_write_lock_metadata`
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
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let hostname = current_hostname();
    let mut warned_live_owner = false;
    let mut warned_stale_live_owner = false;
    let mut transient_create_failures: u32 = 0;
    let mut attempted_once = false;
    let mut dead_reclaim_blocked_attempts = 0_u32;
    // A zero-timeout acquire still gets one immediate retry after it removes a
    // stale lock; otherwise it would reap the dead owner and report Timeout.
    let mut immediate_retry_budget = 0_u8;

    loop {
        if attempted_once {
            if immediate_retry_budget > 0 {
                immediate_retry_budget -= 1;
            } else if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Err(AcquireError::Timeout);
                }
            }
        }
        attempted_once = true;

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

        let metadata = match read_lock_metadata(path) {
            Ok(metadata) => metadata,
            Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                immediate_retry_budget = 1;
                continue;
            }
            Err(ReadLockError::Io(error)) => return Err(error.into()),
            Err(ReadLockError::Malformed(error)) => {
                // A just-created O_EXCL file is visible before its owner has
                // finished writing JSON. Give that transient creation window
                // one poll interval before treating malformed contents as stale.
                sleep_until_retry(deadline, config.poll_interval_ms)?;
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
                remove_lock_file(path)?;
                immediate_retry_budget = 1;
                continue;
            }
        };

        let now = now_ms();
        let since_heartbeat = now.saturating_sub(metadata.heartbeat_at_ms);

        if metadata.hostname != hostname {
            dead_reclaim_blocked_attempts = 0;
            let cross_host_stale_ms = config.cross_host_stale_heartbeat_ms();
            if since_heartbeat > cross_host_stale_ms {
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

        if !lock_owner_is_alive(&metadata) {
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

        if since_heartbeat > config.stale_heartbeat_ms && !warned_stale_live_owner {
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
            warned_stale_live_owner = true;
        }

        let held_for = now.saturating_sub(metadata.created_at_ms);
        if held_for > config.live_owner_warn_ms && !warned_live_owner {
            slog_warn!(
                "filesystem lock at {} held >10min by live heartbeating PID {}; NOT breaking",
                path.display(),
                metadata.pid
            );
            warned_live_owner = true;
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

    let shutdown = Arc::new(AtomicBool::new(false));
    let heartbeat_failed = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    let heartbeat_path = path.to_path_buf();
    let heartbeat_metadata = metadata.clone();
    let heartbeat_shutdown = Arc::clone(&shutdown);
    let heartbeat_failed_for_thread = Arc::clone(&heartbeat_failed);
    let heartbeat = thread::Builder::new()
        .name("aft-fs-lock-heartbeat".to_string())
        .spawn(move || {
            let heartbeat_shutdown_for_run = Arc::clone(&heartbeat_shutdown);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_heartbeat(
                    heartbeat_path,
                    heartbeat_metadata,
                    heartbeat_shutdown_for_run,
                    config,
                );
            }));
            if result.is_err() || !heartbeat_shutdown.load(Ordering::Acquire) {
                heartbeat_failed_for_thread.store(true, Ordering::Release);
            }
            let _ = done_tx.send(());
        })?;

    slog_debug!("acquired filesystem lock at {}", path.display());

    Ok(LockGuard {
        path: path.to_path_buf(),
        metadata,
        shutdown,
        heartbeat_failed,
        heartbeat_done: done_rx,
        heartbeat: Some(heartbeat),
    })
}

fn run_heartbeat(
    path: PathBuf,
    owner: LockMetadata,
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

        match heartbeat_once(&path, &owner) {
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
    matches!(error, HeartbeatError::LockGone | HeartbeatError::NotOwner)
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

fn heartbeat_once(path: &Path, owner: &LockMetadata) -> Result<(), HeartbeatError> {
    let mut metadata = match read_lock_metadata(path) {
        Ok(metadata) => metadata,
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Err(HeartbeatError::LockGone);
        }
        Err(ReadLockError::Io(error)) => return Err(HeartbeatError::Io(error)),
        Err(ReadLockError::Malformed(error)) => return Err(HeartbeatError::Malformed(error)),
    };

    if !lock_identity_matches(&metadata, owner) {
        return Err(HeartbeatError::NotOwner);
    }

    metadata.heartbeat_at_ms = now_ms();
    atomic_write_lock_metadata(path, &metadata).map_err(HeartbeatError::Io)
}

#[derive(Debug)]
enum HeartbeatError {
    Io(io::Error),
    LockGone,
    Malformed(serde_json::Error),
    NotOwner,
}

#[derive(Debug)]
enum ReadLockError {
    Io(io::Error),
    Malformed(serde_json::Error),
}

fn read_lock_metadata(path: &Path) -> Result<LockMetadata, ReadLockError> {
    let bytes = fs::read(path).map_err(ReadLockError::Io)?;
    serde_json::from_slice(&bytes).map_err(ReadLockError::Malformed)
}

#[cfg(unix)]
fn open_new_lock_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_new_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn write_lock_metadata_to_file(file: &mut File, metadata: &LockMetadata) -> io::Result<()> {
    serde_json::to_writer(&mut *file, metadata).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn create_lock_file_atomically(path: &Path, metadata: &LockMetadata) -> io::Result<()> {
    let tmp_path = temp_path_for_lock(path);
    let result = (|| {
        let mut file = open_new_lock_file(&tmp_path)?;
        write_lock_metadata_to_file(&mut file, metadata)?;
        drop(file);

        fs::hard_link(&tmp_path, path)?;
        sync_parent(path);
        Ok(())
    })();

    let _ = fs::remove_file(&tmp_path);
    result
}

fn atomic_write_lock_metadata(path: &Path, metadata: &LockMetadata) -> io::Result<()> {
    let tmp_path = temp_path_for_lock(path);
    let write_result = (|| {
        let mut file = open_new_lock_file(&tmp_path)?;
        write_lock_metadata_to_file(&mut file, metadata)?;
        drop(file);

        rename_over(&tmp_path, path)?;
        sync_parent(path);
        Ok(())
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
    match fs::remove_file(path) {
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

struct ReclaimTokenGuard {
    path: PathBuf,
}

impl Drop for ReclaimTokenGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        sync_parent(&self.path);
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
    sync_parent(token_path);
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
        Ok(()) => {
            sync_parent(token_path);
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Remove abandoned reclaim tokens from the bounded set of storage domains that
/// own persistent artifact locks. The first maintenance pass in a process runs;
/// later passes are suppressed for 24 hours.
pub(crate) fn sweep_stale_reclaim_tokens(root: &Path) -> io::Result<Option<usize>> {
    let last_run = RECLAIM_TOKEN_SWEEP_LAST_RUN.get_or_init(|| Mutex::new(None));
    sweep_stale_reclaim_tokens_at(root, Instant::now(), last_run)
}

fn sweep_stale_reclaim_tokens_at(
    root: &Path,
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
    for relative in ROOT_RECLAIM_TOKEN_PATHS {
        removed =
            removed.saturating_add(remove_stale_reclaim_token(&root.join(relative))? as usize);
    }
    for domain in RECLAIM_TOKEN_SWEEP_DOMAINS {
        removed = removed.saturating_add(sweep_reclaim_token_domain(&root.join(domain))?);
    }
    removed = removed.saturating_add(sweep_backup_reclaim_tokens(root)?);
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
    if removed {
        sync_parent(path);
    }
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
struct RetrySleepObserverGuard {
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
fn observe_retry_sleeps_for_test(
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

fn sleep_until_retry(deadline: Option<Instant>, poll_interval_ms: u64) -> Result<(), AcquireError> {
    let poll = Duration::from_millis(poll_interval_ms);
    let sleep_for = match deadline {
        Some(deadline) => {
            let now = Instant::now();
            if now >= deadline {
                return Err(AcquireError::Timeout);
            }
            poll.min(deadline.saturating_duration_since(now))
        }
        None => poll,
    };
    #[cfg(test)]
    note_retry_sleep_for_test();
    thread::sleep(sleep_for);
    Ok(())
}

pub(crate) fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
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
fn current_hostname() -> String {
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
fn current_hostname() -> String {
    crate::environment::non_empty_var("COMPUTERNAME")
        .or_else(|| crate::environment::non_empty_var("HOSTNAME"))
        .unwrap_or_else(|| "unknown-host".to_string())
}

#[cfg(not(any(unix, windows)))]
fn current_hostname() -> String {
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
    })
}

#[cfg(windows)]
fn process_identity(_pid: u32) -> Option<ProcessIdentity> {
    // Windows keeps PID-only liveness for now. Querying creation time requires a
    // process handle and new FFI/error policy; Linux PID namespaces are the
    // environment where reused PIDs otherwise persistently deadlock leases.
    None
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
    if pid == 0 {
        return false;
    }
    let filter = format!("PID eq {pid}");
    let Ok(output) = std::process::Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
    else {
        return true;
    };

    if !output.status.success() {
        return true;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // `tasklist /NH /FO CSV` emits a single line per matching process with
    // every field quoted, e.g. `"image","7420","Console","1","12,345 K"`.
    // When the filter matches nothing, the literal text
    // `INFO: No tasks are running which match the specified criteria.`
    // is written to stdout. The previous matcher was too strict — it looked
    // for `","{pid}",` patterns mid-line, which works on most Windows builds
    // but missed Windows runners that emit slightly different quoting (e.g.
    // a trailing CRLF leaves the pid token at end-of-line as `"7420"\r\n`).
    // The robust check: confirm the "no tasks" sentinel is absent AND any
    // PID-quoted form is present.
    if stdout.contains("No tasks are running") {
        return false;
    }
    stdout.contains(&format!("\"{pid}\""))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Barrier};

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

        let removed = sweep_stale_reclaim_tokens_at(root.path(), Instant::now(), &cadence)
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
            sweep_stale_reclaim_tokens_at(root.path(), start, &cadence).expect("first sweep"),
            Some(1)
        );
        assert!(!first_token.exists());

        let second_token = write_reclaim_token(&cache_dir.join("cache.lock"), &dead);
        assert_eq!(
            sweep_stale_reclaim_tokens_at(root.path(), start + Duration::from_secs(60), &cadence,)
                .expect("suppressed sweep"),
            None
        );
        assert!(second_token.exists());
        assert_eq!(
            sweep_stale_reclaim_tokens_at(
                root.path(),
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
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
        // the fix, retries instead of dying.
        fs::write(&path, b"{ not valid json").expect("corrupt lockfile");

        // Give the heartbeat several intervals to observe the malformed file.
        // Pre-fix, the thread is dead by now.
        thread::sleep(Duration::from_millis(
            test_config().heartbeat_interval_ms * 4,
        ));

        // Restore valid owner metadata with a clearly-stale heartbeat sentinel.
        // Ownership fields must match `owner` exactly so heartbeat_once passes
        // its ownership check and writes a fresh timestamp.
        //
        // Use the atomic temp-write+rename path rather than remove-then-recreate:
        // a remove followed by a separate create leaves a window where the file
        // does not exist, and a heartbeat poll landing in that window reads
        // NotFound -> LockGone (terminal) and kills the thread, failing this test
        // spuriously under runner load (observed on macOS CI). The atomic replace
        // overwrites the corrupt file in place with no no-file window on Unix.
        let sentinel = now_ms().saturating_sub(1_000_000);
        let mut restored = owner.clone();
        restored.heartbeat_at_ms = sentinel;
        atomic_write_lock_metadata(&path, &restored).expect("atomically restore lock metadata");

        // If the heartbeat thread is still alive (the fix), it will overwrite
        // heartbeat_at_ms with a current value. Poll for that recovery.
        let deadline = std::time::Instant::now() + Duration::from_millis(3_000);
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
            "heartbeat did not recover after a transient malformed read — thread likely died"
        );
        drop(guard);
    }
}
