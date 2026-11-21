use std::sync::mpsc;
use std::time::Instant;

#[cfg(target_os = "macos")]
type Events = Vec<notify::Result<notify::Event>>;
#[cfg(target_os = "linux")]
type Events = notify::Result<notify::Event>;

pub(super) enum Message {
    Events(Events),
    Wake,
}

pub(super) fn receive(
    rx: &mpsc::Receiver<Message>,
    deadline: Option<Instant>,
) -> Result<Message, mpsc::RecvTimeoutError> {
    if let Some(deadline) = deadline {
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    } else {
        rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
    }
}

#[cfg(target_os = "macos")]
pub(super) fn next_deadline(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    first.into_iter().chain(second).min()
}
