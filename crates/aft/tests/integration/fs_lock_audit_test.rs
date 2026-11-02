use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aft::fs_lock::{self, AcquireError, STALE_HEARTBEAT_MS};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
struct SyntheticLockMetadata {
    pid: u32,
    hostname: String,
    created_at_ms: u64,
    heartbeat_at_ms: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
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

fn write_synthetic_lock(path: &Path, metadata: &SyntheticLockMetadata) {
    fs::create_dir_all(path.parent().expect("lock parent")).expect("create lock parent");
    fs::write(path, serde_json::to_vec(metadata).expect("serialize lock"))
        .expect("write synthetic lock");
}

#[test]
fn live_same_host_pid_with_stale_heartbeat_is_not_reclaimed() {
    let dir = crate::test_helpers::tempdir();
    let lock_path = dir.path().join("audit.lock");
    let stale_at = now_ms().saturating_sub(STALE_HEARTBEAT_MS + 60_000);
    let metadata = SyntheticLockMetadata {
        pid: std::process::id(),
        hostname: current_hostname(),
        created_at_ms: stale_at,
        heartbeat_at_ms: stale_at,
    };
    write_synthetic_lock(&lock_path, &metadata);

    let result = fs_lock::try_acquire(&lock_path, Duration::from_millis(150));
    assert!(matches!(result, Err(AcquireError::Timeout)));

    let retained: SyntheticLockMetadata =
        serde_json::from_slice(&fs::read(&lock_path).expect("read retained lock"))
            .expect("parse retained lock");
    assert_eq!(retained.pid, metadata.pid);
    assert_eq!(retained.hostname, metadata.hostname);
}

#[test]
fn stale_cross_host_lock_is_reclaimed_after_extended_heartbeat_timeout() {
    let dir = crate::test_helpers::tempdir();
    let lock_path = dir.path().join("audit.lock");
    let stale_at = now_ms().saturating_sub(STALE_HEARTBEAT_MS.saturating_mul(5) + 1_000);
    let metadata = SyntheticLockMetadata {
        pid: std::process::id(),
        hostname: format!("{}-other", current_hostname()),
        created_at_ms: stale_at,
        heartbeat_at_ms: stale_at,
    };
    write_synthetic_lock(&lock_path, &metadata);

    let guard = fs_lock::try_acquire(&lock_path, Duration::from_secs(2))
        .expect("reclaim stale cross-host lock");
    drop(guard);

    assert!(!lock_path.exists());
}
