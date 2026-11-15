//! Opt-in contention diagnostics for child-process tests. Pointer identities
//! refer to objects in the child, not to addresses in the test process.

use std::path::{Path, PathBuf};
use std::sync::{LockResult, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::TrackedConnection;

fn directory() -> Option<PathBuf> {
    std::env::var_os("AFT_TEST_MAINTENANCE_RETRY_GATE").map(PathBuf::from)
}

fn publish(directory: &Path, name: &str, evidence: &Value) {
    // Readers must never observe a partially written event, including on Windows.
    let temporary = directory.join(format!("{name}.tmp"));
    std::fs::write(&temporary, evidence.to_string()).expect("write contention evidence");
    std::fs::rename(temporary, directory.join(name)).expect("publish contention evidence");
}

/// Pause after a real SQLite BUSY result. Observe the actual mutex rather than
/// assuming the caller dropped its guard. The ceiling only bounds abandoned
/// fixtures; a test must finish its reads before releasing the retry.
pub(super) fn maintenance_retry_checkpoint(
    db: &Mutex<TrackedConnection>,
    connection_id: usize,
    attempt_elapsed: Duration,
) {
    let Some(directory) = directory() else {
        return;
    };
    // Multiple maintenance tasks share this gate. Only the first publishes its
    // observation; all of them stay paused until the test releases them.
    let first = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("owner"))
        .is_ok();
    let mut evidence = json!({
        "mutex_id": format!("{db:p}"),
        "connection_id": connection_id,
        "sqlite_busy": true,
        "attempt_elapsed_us": attempt_elapsed.as_micros(),
    });
    if first {
        publish(&directory, "busy", &evidence);
    }
    let mut observed_release = false;
    let ceiling = Instant::now() + Duration::from_secs(120);
    while !directory.join("release").exists() && Instant::now() < ceiling {
        if first && !observed_release {
            // A retention candidate read or another request may briefly own the
            // mutex after maintenance drops it. Observe a successful acquisition
            // during the pause, not a single scheduling-dependent sample.
            let observed_connection = db
                .try_lock()
                .ok()
                .map(|conn| &*conn as *const TrackedConnection as usize);
            if let Some(observed_connection) = observed_connection {
                evidence["observed_connection_id"] = json!(observed_connection);
                evidence["mutex_available"] = json!(true);
                crate::slog_debug!("maintenance write busy retry paused: {evidence}");
                publish(&directory, "paused", &evidence);
                observed_release = true;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if first {
        std::fs::remove_file(directory.join("paused")).ok();
        crate::slog_debug!("maintenance write busy retry resumed");
    }
}

/// Record the connection actually acquired by db_get_state and whether its
/// acquisition queued on the shared mutex. No gate means the normal lock path.
pub(crate) fn lock_state_read(
    db: &Mutex<TrackedConnection>,
) -> LockResult<MutexGuard<'_, TrackedConnection>> {
    let Some(directory) = directory() else {
        return db.lock();
    };
    let started = Instant::now();
    let requested = json!({
        "mutex_id": format!("{db:p}"),
        "maintenance_paused": directory.join("paused").exists(),
    });
    publish(&directory, "read-requested", &requested);
    let lock_started = Instant::now();
    let (result, waited) = match db.try_lock() {
        Err(TryLockError::WouldBlock) => {
            publish(&directory, "read-waiting", &requested);
            (db.lock(), true)
        }
        result => (
            result.map_err(|error| match error {
                TryLockError::Poisoned(error) => error,
                TryLockError::WouldBlock => unreachable!(),
            }),
            false,
        ),
    };
    let lock_elapsed = lock_started.elapsed();
    if let Ok(conn) = &result {
        let evidence = json!({
            "mutex_id": format!("{db:p}"),
            "connection_id": &**conn as *const TrackedConnection as usize,
            "waited_for_mutex": waited,
            "lock_elapsed_us": lock_elapsed.as_micros(),
            "probe_elapsed_us": started.elapsed().as_micros(),
            "maintenance_paused": directory.join("paused").exists(),
        });
        crate::slog_debug!("state database read acquired: {evidence}");
        publish(&directory, "read-acquired", &evidence);
    }
    result
}
