//! Durable per-minute storage for the process write ledger.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::db::TrackedConnection;

/// Fold at most once per minute without delaying a request loop on the shared DB.
pub fn maybe_spawn_fold(db: Option<Arc<Mutex<TrackedConnection>>>) {
    static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static LAST: std::sync::OnceLock<Mutex<Option<Instant>>> = std::sync::OnceLock::new();
    use std::sync::atomic::Ordering;

    let Some(db) = db else {
        return;
    };
    let mut last = LAST
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.is_some_and(|value| value.elapsed() < Duration::from_secs(60))
        || IN_FLIGHT.swap(true, Ordering::AcqRel)
    {
        return;
    }
    *last = Some(Instant::now());
    drop(last);

    let spawn = std::thread::Builder::new()
        .name("aft-write-ledger-fold".to_owned())
        .spawn(move || {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0);
            match db.try_lock() {
                Ok(mut conn) => {
                    if let Err(error) = crate::write_ledger::fold_minute(&mut conn, now_ms) {
                        log::warn!("write ledger minute fold failed: {error}");
                    }
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    log::warn!("write ledger minute fold skipped: database mutex poisoned");
                }
            }
            IN_FLIGHT.store(false, Ordering::Release);
        });
    if let Err(error) = spawn {
        IN_FLIGHT.store(false, Ordering::Release);
        log::warn!("write ledger minute fold could not start: {error}");
    }
}
