//! Records the current holder's source line and acquisition time using atomics,
//! so watchdog diagnostics never need to lock the observed mutex.

use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) struct LockDiagnostics {
    site_file: &'static str,
    origin: Instant,
    held_since_ms_plus_one: AtomicU64,
    line: AtomicUsize,
}

impl LockDiagnostics {
    pub(crate) fn new(site_file: &'static str) -> Arc<Self> {
        Arc::new(Self {
            site_file,
            origin: Instant::now(),
            held_since_ms_plus_one: AtomicU64::new(0),
            line: AtomicUsize::new(0),
        })
    }

    #[track_caller]
    pub(crate) fn hold(&self) -> LockHold<'_> {
        self.line
            .store(Location::caller().line() as usize, Ordering::Relaxed);
        self.held_since_ms_plus_one
            .store(self.now_ms().saturating_add(1), Ordering::Release);
        LockHold(self)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn snapshot(&self) -> String {
        let since = self.held_since_ms_plus_one.load(Ordering::Acquire);
        if since == 0 {
            return "holder=none".to_string();
        }
        format!(
            "holder={}:{} held_for_ms={}",
            self.site_file,
            self.line.load(Ordering::Relaxed),
            self.now_ms().saturating_sub(since - 1),
        )
    }
}

pub(crate) struct LockHold<'a>(&'a LockDiagnostics);

impl Drop for LockHold<'_> {
    fn drop(&mut self) {
        self.0.held_since_ms_plus_one.store(0, Ordering::Release);
    }
}

/// Records every acquisition, including timed and nonblocking probes. The
/// evidence guard drops before the mutex guard unlocks, so a previous holder
/// cannot clear a subsequent holder's record.
pub(crate) struct TrackedMutex<T> {
    mutex: parking_lot::Mutex<T>,
    diagnostics: Arc<LockDiagnostics>,
}

impl<T> TrackedMutex<T> {
    pub(crate) fn new(value: T, site_file: &'static str) -> Self {
        Self {
            mutex: parking_lot::Mutex::new(value),
            diagnostics: LockDiagnostics::new(site_file),
        }
    }

    pub(crate) fn diagnostics(&self) -> Arc<LockDiagnostics> {
        Arc::clone(&self.diagnostics)
    }

    #[track_caller]
    pub(crate) fn lock(&self) -> TrackedGuard<'_, T> {
        let guard = self.mutex.lock();
        TrackedGuard {
            _hold: self.diagnostics.hold(),
            guard,
        }
    }

    #[track_caller]
    pub(crate) fn try_lock(&self) -> Option<TrackedGuard<'_, T>> {
        let guard = self.mutex.try_lock()?;
        Some(TrackedGuard {
            _hold: self.diagnostics.hold(),
            guard,
        })
    }

    #[track_caller]
    pub(crate) fn try_lock_for(&self, timeout: Duration) -> Option<TrackedGuard<'_, T>> {
        let guard = self.mutex.try_lock_for(timeout)?;
        Some(TrackedGuard {
            _hold: self.diagnostics.hold(),
            guard,
        })
    }
}

pub(crate) struct TrackedGuard<'a, T> {
    _hold: LockHold<'a>,
    guard: parking_lot::MutexGuard<'a, T>,
}

impl<T> Deref for TrackedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_evidence_survives_failed_probe_and_clears_on_release() {
        let mutex = TrackedMutex::new(0, file!());
        let diagnostics = mutex.diagnostics();
        assert_eq!(diagnostics.snapshot(), "holder=none");
        let line = line!() + 1;
        let held = mutex.lock();
        assert!(diagnostics
            .snapshot()
            .contains(&format!("holder={}:{line} held_for_ms=", file!())));
        assert!(mutex.try_lock().is_none());
        assert!(diagnostics
            .snapshot()
            .contains(&format!("holder={}:{line} held_for_ms=", file!())));
        drop(held);
        assert_eq!(diagnostics.snapshot(), "holder=none");
        let _held = mutex.try_lock_for(Duration::from_millis(1)).unwrap();
        assert!(diagnostics.snapshot().contains("held_for_ms="));
    }
}
