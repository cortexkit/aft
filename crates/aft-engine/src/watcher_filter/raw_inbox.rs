//! The bounded inbox between a native watcher backend and its filter thread.
//!
//! A backend must never block on its consumer, so a full inbox drops the
//! event and raises an overflow flag instead. The filter treats the flag as a
//! user-space event drop: it takes the same rescan path as an operating
//! system queue overflow. The inbox also carries a wake signal that does not
//! count as a sender, so a shutdown request can wake an idle filter thread
//! while a backend thread that dies alone still disconnects the inbox.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};

/// Events the inbox holds before it drops further ones and flags an overflow.
pub const RAW_INBOX_CAPACITY: usize = 16 * super::WATCHER_MAX_BATCH_PATHS;

/// The event info that marks an inbox overflow, in the vocabulary
/// [`super::RescanReason::from_event_info`] already reads.
pub(super) const RAW_INBOX_OVERFLOW_INFO: &str = "rescan: user dropped";

type RawEvent = notify::Result<notify::Event>;

/// A backend's end of the inbox. Sending never blocks.
///
/// A backend can also feed a plain unbounded channel (the config-file
/// watchers and backend tests do), through `From<mpsc::Sender<_>>`.
#[derive(Clone)]
pub struct RawEventSender(SenderKind);

#[derive(Clone)]
enum SenderKind {
    Inbox {
        events: Sender<RawEvent>,
        overflowed: Arc<AtomicBool>,
    },
    Channel(mpsc::Sender<RawEvent>),
}

impl From<mpsc::Sender<RawEvent>> for RawEventSender {
    fn from(sender: mpsc::Sender<RawEvent>) -> Self {
        Self(SenderKind::Channel(sender))
    }
}

impl RawEventSender {
    /// Queues `event`, or drops it and flags an overflow when the inbox is
    /// full. Fails only once the receiver is gone, like `mpsc::Sender::send`.
    ///
    /// # Errors
    ///
    /// The receiver was dropped; the event is handed back.
    pub fn send(&self, event: RawEvent) -> Result<(), mpsc::SendError<RawEvent>> {
        let (events, overflowed) = match &self.0 {
            SenderKind::Inbox { events, overflowed } => (events, overflowed),
            SenderKind::Channel(sender) => return sender.send(event),
        };
        match events.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                overflowed.store(true, Ordering::Release);
                Ok(())
            }
            Err(TrySendError::Disconnected(event)) => Err(mpsc::SendError(event)),
        }
    }
}

/// The filter thread's end of the inbox.
pub struct RawEventReceiver {
    events: Receiver<RawEvent>,
    overflowed: Arc<AtomicBool>,
    wake_rx: Receiver<()>,
    /// Kept so the wake channel never disconnects: a disconnected channel
    /// would make every wait return at once.
    wake_tx: Sender<()>,
}

/// Wakes a filter thread waiting on its inbox. Holding one does not keep the
/// inbox connected.
#[derive(Clone)]
pub struct RawInboxWaker(Sender<()>);

impl RawInboxWaker {
    pub fn wake(&self) {
        // A full wake channel already holds a wake.
        let _ = self.0.try_send(());
    }
}

/// An inbox of [`RAW_INBOX_CAPACITY`] events.
#[must_use]
pub fn raw_event_channel() -> (RawEventSender, RawEventReceiver) {
    raw_event_channel_with_capacity(RAW_INBOX_CAPACITY)
}

#[must_use]
pub(crate) fn raw_event_channel_with_capacity(
    capacity: usize,
) -> (RawEventSender, RawEventReceiver) {
    let (events_tx, events_rx) = crossbeam_channel::bounded(capacity);
    let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
    let overflowed = Arc::new(AtomicBool::new(false));
    (
        RawEventSender(SenderKind::Inbox {
            events: events_tx,
            overflowed: Arc::clone(&overflowed),
        }),
        RawEventReceiver {
            events: events_rx,
            overflowed,
            wake_rx,
            wake_tx,
        },
    )
}

impl RawEventReceiver {
    #[must_use]
    pub fn waker(&self) -> RawInboxWaker {
        RawInboxWaker(self.wake_tx.clone())
    }

    /// Whether an event was dropped since the last call, clearing the flag.
    pub fn take_overflow(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    /// The next event, waiting up to `timeout`. A wake ends the wait early
    /// and reports `Timeout`, so the caller re-checks its own state.
    ///
    /// # Errors
    ///
    /// `Timeout` after `timeout` or a wake; `Disconnected` once every
    /// sender is gone and the inbox is empty.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<RawEvent, mpsc::RecvTimeoutError> {
        crossbeam_channel::select! {
            recv(self.events) -> event => event.map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            recv(self.wake_rx) -> _ => Err(mpsc::RecvTimeoutError::Timeout),
            default(timeout) => Err(mpsc::RecvTimeoutError::Timeout),
        }
    }

    /// The next event, waiting for one; wakes are ignored.
    ///
    /// # Errors
    ///
    /// Every sender is gone and the inbox is empty.
    pub fn recv(&self) -> Result<RawEvent, mpsc::RecvError> {
        self.events.recv().map_err(|_| mpsc::RecvError)
    }

    /// The next queued event, if any.
    ///
    /// # Errors
    ///
    /// `Empty` with nothing queued; `Disconnected` once every sender is gone
    /// and the inbox is empty.
    pub fn try_recv(&self) -> Result<RawEvent, mpsc::TryRecvError> {
        self.events.try_recv().map_err(|error| match error {
            crossbeam_channel::TryRecvError::Empty => mpsc::TryRecvError::Empty,
            crossbeam_channel::TryRecvError::Disconnected => mpsc::TryRecvError::Disconnected,
        })
    }

    /// Events queued now, for tests and measurements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}
