use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct LockMetadata {
    pid: u32,
    hostname: String,
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
            let cross_host_stale_ms = config.cross_host_stale_heartbeat_ms();
            if since_heartbeat > cross_host_stale_ms {
                slog_warn!(
                    "reclaiming cross-host filesystem lock at {} from host {} after stale heartbeat ({}ms > {}ms)",
                    path.display(),
                    metadata.hostname,
                    since_heartbeat,
                    cross_host_stale_ms
                );
                // Compare-and-delete: only remove if it's still the SAME stale
                // owner (a fresh owner may have acquired it in the gap).
                if reclaim_lock_file(path, &metadata)? {
                    immediate_retry_budget = 1;
                }
                continue;
            }
            sleep_until_retry(deadline, config.poll_interval_ms)?;
            continue;
        }

        if !process_alive(metadata.pid) {
            slog_warn!(
                "removing filesystem lock at {} from dead PID {}",
                path.display(),
                metadata.pid
            );
            // Compare-and-delete: only remove if it's still this dead owner's
            // lock. A fresh owner could have written a new lock (with a recycled
            // or different PID) between our liveness check and the unlink.
            if reclaim_lock_file(path, &metadata)? {
                immediate_retry_budget = 1;
            }
            continue;
        }

        if since_heartbeat > config.stale_heartbeat_ms && !warned_stale_live_owner {
            // Same-host PID liveness is authoritative. A SIGSTOP'd process,
            // suspended VM, or sleeping laptop can miss heartbeats and later
            // resume inside the critical section. Breaking that lock would allow
            // split-brain writers, so a paused live owner blocks acquirers until
            // it resumes and releases the lock or the PID dies.
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
    let metadata = LockMetadata {
        pid: std::process::id(),
        hostname: hostname.to_string(),
        created_at_ms: now,
        heartbeat_at_ms: now,
        writer_epoch: format!("{}-{}", std::process::id(), now_nanos()),
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
/// writers. Re-read immediately before the unlink and bail if the identity
/// (pid, hostname, created_at_ms) changed or the file vanished. POSIX has no
/// atomic compare-and-unlink, so a microscopic residual race remains, but this
/// shrinks the window from the whole judgment/poll duration to a couple of
/// syscalls — the standard mitigation. Returns true if we removed it.
fn reclaim_lock_file(path: &Path, judged: &LockMetadata) -> io::Result<bool> {
    let Some(_token) = acquire_reclaim_token(path)? else {
        return Ok(false);
    };
    match read_lock_metadata(path) {
        Ok(current) => {
            if lock_identity_matches(&current, judged) {
                remove_lock_file(path)?;
                Ok(true)
            } else {
                // A different owner acquired it in the gap — do NOT delete.
                Ok(false)
            }
        }
        // Already gone (released/reclaimed by someone else) — nothing to do.
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        // Malformed now (mid-write by a new owner) — don't delete; retry next poll.
        Err(ReadLockError::Malformed(_)) => Ok(false),
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

fn acquire_reclaim_token(lock_path: &Path) -> io::Result<Option<ReclaimTokenGuard>> {
    let token_path = reclaim_token_path(lock_path);
    let metadata = LockMetadata {
        pid: std::process::id(),
        hostname: current_hostname(),
        created_at_ms: now_ms(),
        heartbeat_at_ms: now_ms(),
        writer_epoch: format!("reclaim-{}-{}", std::process::id(), now_nanos()),
    };
    let mut file = match open_new_lock_file(&token_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => return Err(error),
    };
    if let Err(error) = write_lock_metadata_to_file(&mut file, &metadata) {
        let _ = fs::remove_file(&token_path);
        return Err(error);
    }
    sync_parent(&token_path);
    Ok(Some(ReclaimTokenGuard { path: token_path }))
}

fn reclaim_token_path(lock_path: &Path) -> PathBuf {
    let file_name = lock_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lock");
    lock_path.with_file_name(format!(".{file_name}.reclaim"))
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

    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string())
}

#[cfg(windows)]
fn current_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}

#[cfg(not(any(unix, windows)))]
fn current_hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string())
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

    fn synthetic_metadata(pid: u32, hostname: String, created_at_ms: u64) -> LockMetadata {
        LockMetadata {
            pid,
            hostname,
            created_at_ms,
            heartbeat_at_ms: created_at_ms,
            writer_epoch: format!("synthetic-{pid}-{created_at_ms}"),
        }
    }

    fn current_process_metadata() -> LockMetadata {
        let now = now_ms();
        synthetic_metadata(std::process::id(), current_hostname(), now)
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
        let removed = reclaim_lock_file(&path, &judged_a).expect("reclaim");
        assert!(!removed, "must not remove a different owner's lock");
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
        let removed = reclaim_lock_file(&path, &owner).expect("reclaim");
        assert!(removed, "matching-identity stale lock should be removed");
        assert!(!path.exists());

        // Reclaiming a now-absent lock is a no-op, not an error.
        assert!(!reclaim_lock_file(&path, &owner).expect("reclaim missing"));
    }

    #[test]
    fn try_acquire_once_never_waits_behind_live_owner() {
        let (_dir, path) = test_lock_path();
        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let contender_path = path.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let contender = std::thread::spawn(move || {
            let _ = started_tx.send(());
            let _ = result_tx.send(try_acquire_once(&contender_path));
        });

        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("contender should start");
        let result = match result_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                contender.join().expect("contender should not panic");
                panic!("contender exited without reporting a result");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drop(guard);
                match result_rx.recv_timeout(Duration::from_secs(1)) {
                    Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = contender.join();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                panic!("try-acquire blocked behind the live owner");
            }
        };

        contender.join().expect("contender should exit");
        assert!(matches!(result, Err(AcquireError::Timeout)));
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
        let initial = read_lock_metadata(&path)
            .expect("read initial metadata")
            .heartbeat_at_ms;

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
    fn stale_heartbeat_from_live_pid_blocks() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        metadata.created_at_ms = now_ms().saturating_sub(60_000);
        metadata.heartbeat_at_ms = now_ms().saturating_sub(60_000);
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
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
        let metadata = LockMetadata {
            pid: std::process::id(),
            hostname: format!("{}-other", current_hostname()),
            created_at_ms: now,
            heartbeat_at_ms: now,
            writer_epoch: format!("cross-host-{now}"),
        };
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
        let metadata = LockMetadata {
            pid: std::process::id(),
            hostname: format!("{}-other", current_hostname()),
            created_at_ms: stale_at,
            heartbeat_at_ms: stale_at,
            writer_epoch: format!("cross-host-{stale_at}"),
        };
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
