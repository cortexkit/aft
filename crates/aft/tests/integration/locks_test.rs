use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aft::search_index::CacheLock;
use aft::semantic_index::SemanticIndexLock;
use aft::symbol_cache_disk::SymbolCacheLock;
use serde::Serialize;

#[derive(Serialize)]
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

fn write_synthetic_lock(lock_path: &Path, metadata: SyntheticLockMetadata) {
    fs::create_dir_all(lock_path.parent().expect("lock parent")).expect("create lock parent");
    let bytes = serde_json::to_vec(&metadata).expect("serialize synthetic lock");
    fs::write(lock_path, bytes).expect("write synthetic lock");
}

fn dead_owner_metadata() -> SyntheticLockMetadata {
    let now = now_ms();
    SyntheticLockMetadata {
        pid: 999_999_999,
        hostname: current_hostname(),
        created_at_ms: now,
        heartbeat_at_ms: now,
    }
}

fn cross_host_metadata() -> SyntheticLockMetadata {
    let now = now_ms();
    SyntheticLockMetadata {
        pid: std::process::id(),
        hostname: format!("{}-other", current_hostname()),
        created_at_ms: now,
        heartbeat_at_ms: now,
    }
}

fn assert_serializes_concurrent_callers<G, F>(acquire: F)
where
    G: Send + 'static,
    F: Fn() -> io::Result<G> + Send + Sync + 'static,
{
    let acquire = Arc::new(acquire);
    let inside = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let max_inside = Arc::new(AtomicUsize::new(0));
    let (first_entered_tx, first_entered_rx) = mpsc::channel();

    let first = {
        let acquire = Arc::clone(&acquire);
        let inside = Arc::clone(&inside);
        let entered = Arc::clone(&entered);
        let max_inside = Arc::clone(&max_inside);
        thread::spawn(move || {
            let guard = acquire().expect("acquire test lock");
            let previous = inside.fetch_add(1, Ordering::SeqCst);
            assert_eq!(previous, 0, "two lock holders overlapped");
            entered.fetch_add(1, Ordering::SeqCst);
            max_inside.fetch_max(previous + 1, Ordering::SeqCst);
            first_entered_tx.send(()).expect("signal first lock entry");
            thread::sleep(Duration::from_millis(150));
            inside.fetch_sub(1, Ordering::SeqCst);
            drop(guard);
        })
    };

    first_entered_rx.recv().expect("wait for first lock entry");

    let second = {
        let acquire = Arc::clone(&acquire);
        let inside = Arc::clone(&inside);
        let entered = Arc::clone(&entered);
        let max_inside = Arc::clone(&max_inside);
        thread::spawn(move || {
            let guard = acquire().expect("acquire test lock");
            let previous = inside.fetch_add(1, Ordering::SeqCst);
            assert_eq!(previous, 0, "two lock holders overlapped");
            entered.fetch_add(1, Ordering::SeqCst);
            max_inside.fetch_max(previous + 1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(25));
            inside.fetch_sub(1, Ordering::SeqCst);
            drop(guard);
        })
    };

    first.join().expect("join first lock worker");
    second.join().expect("join second lock worker");

    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(max_inside.load(Ordering::SeqCst), 1);
}

fn search_cache_dir(root: &Path) -> PathBuf {
    root.join("index").join("project")
}

fn symbol_lock_path(root: &Path, project_key: &str) -> PathBuf {
    root.join("symbols").join(project_key).join("symbols.lock")
}

fn semantic_lock_path(root: &Path, project_key: &str) -> PathBuf {
    root.join("semantic").join(project_key).join("cache.lock")
}

#[test]
fn search_lock_acquires_when_uncontended() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_dir = search_cache_dir(dir.path());
    let lock_path = cache_dir.join("cache.lock");

    let guard = CacheLock::acquire(&cache_dir).expect("acquire search lock");
    assert!(lock_path.exists());
    drop(guard);
    assert!(!lock_path.exists());
}

#[test]
fn search_lock_serializes_concurrent_callers() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_dir = Arc::new(search_cache_dir(dir.path()));
    let lock_path = cache_dir.join("cache.lock");

    assert_serializes_concurrent_callers({
        let cache_dir = Arc::clone(&cache_dir);
        move || CacheLock::acquire(&cache_dir)
    });

    assert!(!lock_path.exists());
}

#[test]
fn search_lock_reclaims_dead_owner() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_dir = search_cache_dir(dir.path());
    let lock_path = cache_dir.join("cache.lock");
    write_synthetic_lock(&lock_path, dead_owner_metadata());

    let guard = CacheLock::acquire(&cache_dir).expect("reclaim dead search owner");
    assert!(lock_path.exists());
    drop(guard);
    assert!(!lock_path.exists());
}

#[test]
fn search_lock_blocks_cross_host() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let cache_dir = search_cache_dir(dir.path());
    let lock_path = cache_dir.join("cache.lock");
    write_synthetic_lock(&lock_path, cross_host_metadata());

    let error = match CacheLock::acquire(&cache_dir) {
        Ok(_) => panic!("cross-host search lock should block"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("timed out acquiring search cache lock"));
    assert!(lock_path.exists());
}

#[test]
fn symbol_lock_acquires_when_uncontended() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let lock_path = symbol_lock_path(storage.path(), "project");

    let guard = SymbolCacheLock::acquire(storage.path(), "project").expect("acquire symbol lock");
    assert!(lock_path.exists());
    drop(guard);
    assert!(!lock_path.exists());
}

#[test]
fn symbol_lock_serializes_concurrent_callers() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let root = Arc::new(storage.path().to_path_buf());
    let lock_path = symbol_lock_path(&root, "project");

    assert_serializes_concurrent_callers({
        let root = Arc::clone(&root);
        move || SymbolCacheLock::acquire(&root, "project")
    });

    assert!(!lock_path.exists());
}

#[test]
fn symbol_lock_reclaims_dead_owner() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let lock_path = symbol_lock_path(storage.path(), "project");
    write_synthetic_lock(&lock_path, dead_owner_metadata());

    let guard =
        SymbolCacheLock::acquire(storage.path(), "project").expect("reclaim dead symbol owner");
    assert!(lock_path.exists());
    drop(guard);
    assert!(!lock_path.exists());
}

#[test]
fn symbol_lock_blocks_cross_host() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let lock_path = symbol_lock_path(storage.path(), "project");
    write_synthetic_lock(&lock_path, cross_host_metadata());

    let error = match SymbolCacheLock::acquire(storage.path(), "project") {
        Ok(_) => panic!("cross-host symbol lock should block"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("timed out acquiring symbol cache lock"));
    assert!(lock_path.exists());
}

#[test]
fn semantic_lock_acquires_when_uncontended() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let lock_path = semantic_lock_path(storage.path(), "project");

    let guard =
        SemanticIndexLock::acquire(storage.path(), "project").expect("acquire semantic lock");
    assert!(lock_path.exists());
    drop(guard);
    assert!(!lock_path.exists());
}

#[test]
fn semantic_lock_serializes_concurrent_callers() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let root = Arc::new(storage.path().to_path_buf());
    let lock_path = semantic_lock_path(&root, "project");

    assert_serializes_concurrent_callers({
        let root = Arc::clone(&root);
        move || SemanticIndexLock::acquire(&root, "project")
    });

    assert!(!lock_path.exists());
}

#[test]
fn semantic_lock_reclaims_dead_owner() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let lock_path = semantic_lock_path(storage.path(), "project");
    write_synthetic_lock(&lock_path, dead_owner_metadata());

    let guard =
        SemanticIndexLock::acquire(storage.path(), "project").expect("reclaim dead semantic owner");
    assert!(lock_path.exists());
    drop(guard);
    assert!(!lock_path.exists());
}

#[test]
fn semantic_lock_blocks_cross_host() {
    let storage = tempfile::tempdir().expect("create temp dir");
    let lock_path = semantic_lock_path(storage.path(), "project");
    write_synthetic_lock(&lock_path, cross_host_metadata());

    let error = match SemanticIndexLock::acquire(storage.path(), "project") {
        Ok(_) => panic!("cross-host semantic lock should block"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("timed out acquiring semantic cache lock"));
    assert!(lock_path.exists());
}
