//! Completion channels and wake scopes shared by deferred producers and transports.

/// Completion-only wake shared by a subc connection. Generation accounting
/// coalesces bursts without losing a completion that races a registry poll.
#[derive(Clone, Default)]
pub(crate) struct DeferredResponseWake(std::sync::Arc<DeferredWakeState>);

#[derive(Default)]
struct DeferredWakeState {
    generation: std::sync::atomic::AtomicU64,
    notify: tokio::sync::Notify,
}

thread_local! {
    static DEFERRED_WAKE: std::cell::RefCell<Option<DeferredResponseWake>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct DeferredWakeScope(Option<DeferredResponseWake>);

impl Drop for DeferredWakeScope {
    fn drop(&mut self) {
        DEFERRED_WAKE.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

pub(crate) struct DeferredCompletionWake(Option<DeferredResponseWake>);

impl Drop for DeferredCompletionWake {
    fn drop(&mut self) {
        if let Some(wake) = &self.0 {
            wake.0
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            wake.0.notify.notify_one();
        }
    }
}

impl DeferredResponseWake {
    pub(crate) fn install(&self) -> DeferredWakeScope {
        DeferredWakeScope(DEFERRED_WAKE.with(|slot| slot.replace(Some(self.clone()))))
    }

    pub(crate) fn generation(&self) -> u64 {
        self.0.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) async fn notified(&self) {
        self.0.notify.notified().await;
    }
}

/// Capture before spawning, and keep the guard inside the producer until after
/// sending its response. Drop also wakes the transport when a producer panics.
pub(crate) fn deferred_completion_wake() -> DeferredCompletionWake {
    DeferredCompletionWake(DEFERRED_WAKE.with(|slot| slot.borrow().clone()))
}

/// A one-shot sender that wakes the admitted connection after its value is
/// queued, or after disconnecting if the producer returns without a value.
pub struct PendingResponseSender<T> {
    // Field drop order matters: disconnect before publishing the wake.
    sender: std::sync::mpsc::SyncSender<T>,
    _completion_wake: DeferredCompletionWake,
}

impl<T> PendingResponseSender<T> {
    pub fn send(self, value: T) -> Result<(), std::sync::mpsc::SendError<T>> {
        self.sender.send(value)
    }
}

pub struct PendingResponseReceiver<T> {
    receiver: std::sync::mpsc::Receiver<T>,
    pub(crate) wake_installed: bool,
}

impl<T> PendingResponseReceiver<T> {
    pub fn try_recv(&self) -> Result<T, std::sync::mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Block for the value at most `timeout`.
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<T, std::sync::mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

/// Capture the request's wake before transferring the sender to a producer.
/// The receiver's type certifies that completion cannot omit that signal.
pub fn pending_response_channel<T>() -> (PendingResponseSender<T>, PendingResponseReceiver<T>) {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let completion_wake = deferred_completion_wake();
    let wake_installed = completion_wake.0.is_some();
    (
        PendingResponseSender {
            sender,
            _completion_wake: completion_wake,
        },
        PendingResponseReceiver {
            receiver,
            wake_installed,
        },
    )
}
