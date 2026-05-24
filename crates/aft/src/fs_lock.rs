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

use crate::{slog_error, slog_info, slog_warn};

pub const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const STALE_HEARTBEAT_MS: u64 = 15_000;
pub const LIVE_OWNER_WARN_MS: u64 = 600_000;
pub const POLL_INTERVAL_MS: u64 = 100;

#[derive(Clone, Copy, Debug)]
struct LockConfig {
    heartbeat_interval_ms: u64,
    stale_heartbeat_ms: u64,
    live_owner_warn_ms: u64,
    poll_interval_ms: u64,
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

pub struct LockGuard {
    path: PathBuf,
    metadata: LockMetadata,
    shutdown: Arc<AtomicBool>,
    heartbeat_done: mpsc::Receiver<()>,
    heartbeat: Option<JoinHandle<()>>,
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
            Ok(true) => slog_info!("released filesystem lock at {}", self.path.display()),
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

    loop {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                return Err(AcquireError::Timeout);
            }
        }

        match create_new_lock(path, &hostname, config) {
            Ok(guard) => return Ok(guard),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }

        let metadata = match read_lock_metadata(path) {
            Ok(metadata) => metadata,
            Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
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
                continue;
            }
        };

        if metadata.hostname != hostname {
            sleep_until_retry(deadline, config.poll_interval_ms)?;
            continue;
        }

        if !process_alive(metadata.pid) {
            slog_warn!(
                "removing filesystem lock at {} from dead PID {}",
                path.display(),
                metadata.pid
            );
            remove_lock_file(path)?;
            continue;
        }

        let now = now_ms();
        let since_heartbeat = now.saturating_sub(metadata.heartbeat_at_ms);
        if since_heartbeat > config.stale_heartbeat_ms {
            slog_warn!(
                "reclaiming filesystem lock at {}: PID {} is alive but heartbeat is stale ({}ms)",
                path.display(),
                metadata.pid,
                since_heartbeat
            );
            remove_lock_file(path)?;
            continue;
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
    };

    create_lock_file_atomically(path, &metadata)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    let heartbeat_path = path.to_path_buf();
    let heartbeat_metadata = metadata.clone();
    let heartbeat_shutdown = Arc::clone(&shutdown);
    let heartbeat = thread::Builder::new()
        .name("aft-fs-lock-heartbeat".to_string())
        .spawn(move || {
            run_heartbeat(
                heartbeat_path,
                heartbeat_metadata,
                heartbeat_shutdown,
                config,
            );
            let _ = done_tx.send(());
        })?;

    slog_info!("acquired filesystem lock at {}", path.display());

    Ok(LockGuard {
        path: path.to_path_buf(),
        metadata,
        shutdown,
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
    loop {
        thread::park_timeout(Duration::from_millis(config.heartbeat_interval_ms));
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        match heartbeat_once(&path, &owner) {
            Ok(()) => {}
            Err(HeartbeatError::LockGone) => {
                slog_error!(
                    "filesystem lock at {} disappeared; stopping heartbeat",
                    path.display()
                );
                return;
            }
            Err(HeartbeatError::NotOwner) => {
                slog_error!(
                    "filesystem lock at {} is no longer owned by this guard; stopping heartbeat",
                    path.display()
                );
                return;
            }
            Err(HeartbeatError::Malformed(error)) => {
                slog_error!(
                    "filesystem lock at {} became malformed: {}; stopping heartbeat",
                    path.display(),
                    error
                );
                return;
            }
            Err(HeartbeatError::Io(error)) => {
                slog_error!(
                    "failed to heartbeat filesystem lock at {}: {}; stopping heartbeat",
                    path.display(),
                    error
                );
                return;
            }
        }
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

    if metadata.pid != owner.pid
        || metadata.hostname != owner.hostname
        || metadata.created_at_ms != owner.created_at_ms
    {
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
        .mode(0o644)
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
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
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

#[cfg(windows)]
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    let _ = fs::remove_file(to);
    fs::rename(from, to)
}

#[cfg(not(windows))]
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
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

fn remove_lock_if_owned(path: &Path, owner: &LockMetadata) -> io::Result<bool> {
    let metadata = match read_lock_metadata(path) {
        Ok(metadata) => metadata,
        Err(ReadLockError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(ReadLockError::Io(error)) => return Err(error),
        Err(ReadLockError::Malformed(_)) => return Ok(false),
    };

    if metadata.pid == owner.pid
        && metadata.hostname == owner.hostname
        && metadata.created_at_ms == owner.created_at_ms
    {
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

fn sync_parent(path: &Path) {
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
fn process_alive(pid: u32) -> bool {
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
fn process_alive(pid: u32) -> bool {
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
fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

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
        }
    }

    fn current_process_metadata() -> LockMetadata {
        let now = now_ms();
        synthetic_metadata(std::process::id(), current_hostname(), now)
    }

    #[test]
    fn acquire_creates_lockfile_and_unlocks_on_drop() {
        let (_dir, path) = test_lock_path();

        let guard = acquire_with_config(&path, None, test_config()).expect("acquire lock");
        let metadata = read_lock_metadata(&path).expect("read lock metadata");
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(metadata.hostname, current_hostname());
        assert_eq!(metadata.created_at_ms, guard.metadata.created_at_ms);

        drop(guard);
        assert!(!path.exists());
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
        let deadline = std::time::Instant::now() + Duration::from_millis(2_000);
        let mut updated = initial;
        while std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            updated = read_lock_metadata(&path)
                .expect("read updated metadata")
                .heartbeat_at_ms;
            if updated > initial {
                break;
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
    fn stale_heartbeat_lock_is_reclaimed() {
        let (_dir, path) = test_lock_path();
        let mut metadata = current_process_metadata();
        metadata.created_at_ms = now_ms().saturating_sub(60_000);
        metadata.heartbeat_at_ms = now_ms().saturating_sub(60_000);
        write_synthetic_lock(&path, &metadata);

        let guard = acquire_with_config(&path, Some(Duration::from_secs(1)), test_config())
            .expect("reclaim stale heartbeat lock");
        let reclaimed = read_lock_metadata(&path).expect("read reclaimed lock");
        assert_ne!(reclaimed.created_at_ms, metadata.created_at_ms);
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
    fn cross_host_lock_is_not_stolen() {
        let (_dir, path) = test_lock_path();
        let now = now_ms().saturating_sub(60_000);
        let metadata = LockMetadata {
            pid: std::process::id(),
            hostname: format!("{}-other", current_hostname()),
            created_at_ms: now,
            heartbeat_at_ms: now,
        };
        write_synthetic_lock(&path, &metadata);

        let result = acquire_with_config(&path, Some(Duration::from_millis(80)), test_config());
        assert!(matches!(result, Err(AcquireError::Timeout)));
        assert_eq!(read_lock_metadata(&path).expect("read lock"), metadata);

        remove_lock_file(&path).expect("cleanup synthetic lock");
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
}
