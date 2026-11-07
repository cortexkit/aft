//! In-band views delivery acknowledgments, distinct from command-report flushes.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

/// Whether the watcher can vouch for disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatcherState {
    Healthy,
    Overflowed,
    Stopped,
    /// A strict reconcile is running; the index must not prune.
    Reconciling,
}

const PREFIX: &str = ".aft-cookie-";
type Pending = Arc<Mutex<BTreeMap<PathBuf, mpsc::Sender<CookieObservation>>>>;

/// A driver delivery cut, not a verified-bind or durable-publication certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookieObservation {
    pub revision: u64,
    pub watcher: WatcherState,
}

/// An exclusive root cookie. Dropping a timed-out ticket cleans up its file.
pub struct CookieTicket {
    path: PathBuf,
    pending: Pending,
    rx: mpsc::Receiver<CookieObservation>,
}

impl CookieTicket {}

impl Drop for CookieTicket {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Default)]
pub(crate) struct CookieBarrier {
    pending: Pending,
}

impl CookieBarrier {}

pub(crate) fn is_cookie_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(PREFIX))
        .and_then(|suffix| suffix.split_once('-'))
        .is_some_and(|(pid, nonce)| {
            !pid.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && !nonce.is_empty()
                && nonce
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
}
