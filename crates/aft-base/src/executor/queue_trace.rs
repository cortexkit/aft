use std::cell::Cell;
use std::fmt;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{ActorState, ExecutorContext, JobCancellation, JobClass, Lane};

#[derive(Clone, Copy)]
pub(super) enum QueueBlocker {
    Lane = 1,
    Writer = 2,
    WriterBarrier = 4,
    RootCapacity = 8,
    WorkerPool = 16,
    EpochReaders = 32,
    EpochGate = 64,
}

impl QueueBlocker {
    fn name(self) -> &'static str {
        match self {
            Self::Lane => "lane",
            Self::Writer => "writer",
            Self::WriterBarrier => "writer_barrier",
            Self::RootCapacity => "root_capacity",
            Self::WorkerPool => "worker_pool",
            Self::EpochReaders => "epoch_readers",
            Self::EpochGate => "epoch_gate",
        }
    }
}

pub(super) fn note(token: Option<&JobCancellation>, blocker: QueueBlocker) {
    if let Some(token) = token {
        token
            .inner
            .queue_blockers
            .fetch_or(blocker as u8, Ordering::Relaxed);
    }
}

pub(super) fn note_head<C: ExecutorContext>(
    actor: &ActorState<C>,
    class: JobClass,
    lane: Lane,
    blocker: QueueBlocker,
) {
    let token = actor
        .class_queues(class)
        .queue(lane)
        .front()
        .and_then(|job| job.cancellation.as_ref());
    note(token, blocker);
}

/// Admission blockers witnessed by the scheduler or worker, without job data.
#[derive(Debug, Clone, Copy)]
pub struct QueueObservation {
    lane: Lane,
    blockers: u8,
    epoch_wait: Option<Duration>,
}

impl fmt::Display for QueueObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lane = match self.lane {
            Lane::PureRead => "pure_read",
            Lane::SerialLspStatus => "serial_lsp_status",
            Lane::HeavyInit => "heavy_init",
            Lane::Mutating => "mutating",
            Lane::MaintenanceCommit => "maintenance_commit",
            Lane::ProviderMetadata => "provider_metadata",
            Lane::CallLedger => "call_ledger",
        };
        write!(f, "queue_lane={lane} queue_waited_on=")?;
        let mut separator = "";
        for blocker in [
            QueueBlocker::Lane,
            QueueBlocker::Writer,
            QueueBlocker::WriterBarrier,
            QueueBlocker::RootCapacity,
            QueueBlocker::WorkerPool,
            QueueBlocker::EpochReaders,
            QueueBlocker::EpochGate,
        ] {
            if self.blockers & (blocker as u8) != 0 {
                write!(f, "{separator}{}", blocker.name())?;
                separator = ",";
            }
        }
        if separator.is_empty() {
            f.write_str("none_observed")?;
        }
        if let Some(wait) = self.epoch_wait {
            write!(f, " queue_epoch_ms={:.3}", wait.as_secs_f64() * 1_000.0)?;
        }
        Ok(())
    }
}

thread_local! {
    static CURRENT: Cell<Option<QueueObservation>> = const { Cell::new(None) };
}

pub fn current_queue_observation() -> Option<QueueObservation> {
    CURRENT.get()
}

pub(super) struct Scope(Option<QueueObservation>);

pub(super) fn enter(lane: Lane, token: Option<&JobCancellation>) -> Scope {
    let blockers = token.map_or(0, |token| {
        token.inner.queue_blockers.load(Ordering::Relaxed)
    });
    Scope(CURRENT.replace(Some(QueueObservation {
        lane,
        blockers,
        epoch_wait: None,
    })))
}

pub(super) fn note_epoch_wait(wait: Duration) {
    if let Some(mut observation) = CURRENT.get() {
        // Report the full timing, but do not label ordinary lock acquisition
        // overhead as contention on every call.
        if wait >= Duration::from_millis(1) {
            observation.blockers |= QueueBlocker::EpochGate as u8;
        }
        observation.epoch_wait = Some(wait);
        CURRENT.set(Some(observation));
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.set(self.0);
    }
}
