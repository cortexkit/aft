//! Wall-clock time as milliseconds since the Unix epoch.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch; zero if the system clock reads earlier
/// than the epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
