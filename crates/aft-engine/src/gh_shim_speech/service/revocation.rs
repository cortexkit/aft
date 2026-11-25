use super::super::store::{StorageError, Store};
use super::{Refusal, Service};
use std::collections::HashMap;
use std::sync::{Arc, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct PendingTasks {
    // An entry with no reason is still in its first, synchronous write.
    entries: HashMap<String, Option<String>>,
    retrying: bool,
}

impl PendingTasks {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn contains(&self, incarnation: &str) -> bool {
        self.entries.contains_key(incarnation)
    }
}

trait RevocationIo: Send + 'static {
    fn write(&self, store: &Store, incarnation: &str, reason: &str) -> Result<(), StorageError>;
    fn sleep(&self, delay: Duration);
    fn now(&self) -> Instant;
}

struct StoreIo;

impl RevocationIo for StoreIo {
    fn write(&self, store: &Store, incarnation: &str, reason: &str) -> Result<(), StorageError> {
        store.write_revocation(incarnation, reason)
    }

    fn sleep(&self, delay: Duration) {
        thread::sleep(delay);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

impl Service {
    fn lock_pending_tasks(&self) -> MutexGuard<'_, PendingTasks> {
        self.pending_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn revoke(self: &Arc<Self>, incarnation: &str, reason: &str) -> Result<(), Refusal> {
        self.revoke_using(incarnation, reason, StoreIo).0
    }

    fn revoke_using(
        self: &Arc<Self>,
        incarnation: &str,
        reason: &str,
        io: impl RevocationIo,
    ) -> (Result<(), Refusal>, Option<JoinHandle<()>>) {
        let work = self.track_finalization();
        let _gate = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.lock_pending_tasks()
            .entries
            .entry(incarnation.into())
            .or_default();
        let result = io.write(&self.store, incarnation, reason);
        let mut pending = self.lock_pending_tasks();
        let Err(error) = result else {
            pending.entries.remove(incarnation);
            return (Ok(()), None);
        };
        let refusal = Err(error.refusal());
        let Some(entry) = pending.entries.get_mut(incarnation) else {
            // The retry worker already wrote the row.
            return (refusal, None);
        };
        entry.get_or_insert_with(|| reason.to_string());
        if pending.retrying {
            return (refusal, None);
        }
        pending.retrying = true;
        drop(pending);

        let failed_at = io.now();
        crate::slog_warn!("could not persist gh speech revocation; retrying: {error}");
        let service = Arc::clone(self);
        let worker = thread::spawn(move || {
            // An unpersisted revocation keeps retirement blocked on purpose.
            // The serving lease is the only fence while the store cannot record
            // the revocation: a process that takes over the lease cannot see
            // this process's in-memory refusal.
            let _work = work;
            service.retry_revocations(io, failed_at);
        });
        (refusal, Some(worker))
    }

    fn retry_revocations(&self, io: impl RevocationIo, mut last_warning: Instant) {
        let mut delay = Duration::from_millis(100);
        let mut attempts = 0u64;
        loop {
            io.sleep(delay);
            let batch: Vec<_> = self
                .lock_pending_tasks()
                .entries
                .iter()
                .filter_map(|(inc, reason)| Some((inc.clone(), reason.as_ref()?.clone())))
                .collect();
            for (inc, reason) in batch {
                attempts = attempts.saturating_add(1);
                if io.write(&self.store, &inc, &reason).is_ok() {
                    self.lock_pending_tasks().entries.remove(&inc);
                }
            }

            let mut pending = self.lock_pending_tasks();
            let remaining = pending
                .entries
                .values()
                .filter(|entry| entry.is_some())
                .count();
            if remaining == 0 {
                pending.retrying = false;
                drop(pending);
                crate::slog_info!(
                    "gh speech revocation queue drained after {attempts} retry attempts"
                );
                return;
            }
            drop(pending);
            let now = io.now();
            if now.duration_since(last_warning) >= Duration::from_secs(60) {
                crate::slog_warn!(
                    "gh speech revocation still pending: {remaining} unpersisted, {attempts} retry attempts"
                );
                last_warning = now;
            }
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests;
