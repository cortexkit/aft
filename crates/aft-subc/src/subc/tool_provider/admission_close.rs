use std::sync::{atomic::Ordering, Arc, Mutex};

use super::{LedgerEdge, LedgerWait};
use crate::db::{call_ledger as ledger, TrackedConnection};
use crate::subc::{DispatchPathMetrics, RouteChannel, SubcError, WriterSender};

pub(super) struct Undispatched {
    pub(super) db: Arc<Mutex<TrackedConnection>>,
    pub(super) key: ledger::Key,
    pub(super) event_id: String,
}

impl Undispatched {
    pub(super) fn settle(self) {
        tokio::task::spawn_blocking(move || {
            let result = (|| -> rusqlite::Result<()> {
                let conn = self.db.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
                if !ledger::get(&conn, &self.key)?.is_some_and(|row| row.event_id == self.event_id)
                {
                    return Ok(());
                }
                ledger::settle(
                    &conn,
                    &self.key,
                    &ledger::not_started(&self.key, "cancelled"),
                    "not_started",
                    Some("cancelled"),
                    ledger::now_ms(),
                )?;
                Ok(())
            })();
            if let Err(error) = result {
                log::warn!("call ledger pre-dispatch settlement failed: {error}");
            }
        });
    }
}

impl LedgerEdge {
    // Called with the waiting registry locked, excluding publication by the
    // admission task. A repeat never installs a call or owns the ledger row.
    pub(super) fn retire_wait(&self, key: (RouteChannel, u64), wait: &mut LedgerWait) -> bool {
        if wait.terminal_claim.swap(true, Ordering::SeqCst) {
            return false;
        }
        wait.cancel.cancel();
        if let Some(new) = wait.undispatched.take() {
            self.calls.lock().unwrap().remove(&key);
            new.settle();
        }
        true
    }

    pub(in crate::subc) async fn close_route(
        &self,
        route: RouteChannel,
        writer: &WriterSender,
        metrics: &DispatchPathMetrics,
    ) -> Result<(), SubcError> {
        let frames = self.release_route_admissions(route)?;
        for frame in frames {
            crate::subc::send_reliable_writer_frame(writer, metrics, frame, "released queued call")
                .await?;
        }
        Ok(())
    }

    pub(in crate::subc) fn release_route_admissions(
        &self,
        route: RouteChannel,
    ) -> Result<Vec<subc_protocol::Frame>, SubcError> {
        self.received
            .lock()
            .unwrap()
            .retain(|(key, _), _| *key != route);
        let mut waiting = self.waiting.lock().unwrap();
        let mut frames = Vec::new();
        waiting.retain(|key, wait| {
            if key.0 != route {
                return true;
            }
            if self.retire_wait(*key, wait) {
                frames.push(wait.admission.released_frame());
            }
            false
        });
        frames.into_iter().collect()
    }
}
