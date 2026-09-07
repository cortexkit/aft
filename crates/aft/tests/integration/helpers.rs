// Re-exported for both the `integration` and `watcher_integration` binaries,
// which share this module via `#[path]`. Each binary uses a different subset,
// so some re-exports are unused in one or the other.
#[allow(unused_imports)]
pub use crate::test_helpers::{
    canonicalize_like_product, cargo_manifest_dir, disable_in_process_file_watcher, fixture_path,
    user_config, user_config_tier, warm_executable, AftProcess, ReleaseOnDrop,
};

pub fn json_string(value: &impl std::fmt::Display) -> String {
    serde_json::to_string(&value.to_string()).unwrap()
}

/// Serializes the real-watcher tests within the `watcher_integration` binary so
/// at most one live `AftProcess` watcher exists at a time.
///
/// These tests live in their own test binary (cargo runs test binaries
/// sequentially, so it runs alone, with no concurrent `aft`-process load from
/// the ~1150-test `integration` binary). Under that concurrent load the macOS
/// `fseventsd` daemon was swamped and the watcher `watch()` call probabilistically
/// hung for ~1-in-3 watcher processes, so events were never delivered and the
/// tests timed out. Binary isolation removes that load; this lock is the
/// belt-and-suspenders within the isolated binary. Acquire it at the top of every
/// real-watcher test.
///
/// `#[allow(dead_code)]` because this module is `#[path]`-shared with the
/// `integration` binary, which no longer contains any watcher test.
#[allow(dead_code)]
pub fn watcher_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
