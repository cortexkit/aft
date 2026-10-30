//! In-band views delivery acknowledgments, distinct from command-report flushes.
use crate::views::snapshot::WatcherState;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicU64, atomic::Ordering, mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PREFIX: &str = ".aft-cookie-";
static NEXT: AtomicU64 = AtomicU64::new(0);
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

impl CookieTicket {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// On timeout or an untrusted watcher, use strict reconciliation rather
    /// than relying on the cookie acknowledgment.
    pub fn wait_timeout(
        &self,
        timeout: Duration,
    ) -> Result<CookieObservation, mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

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

impl CookieBarrier {
    pub(crate) fn begin(&self, root: &Path) -> std::io::Result<CookieTicket> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = root.join(format!(
            "{PREFIX}{}-{nonce:x}{:016x}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let (tx, rx) = mpsc::channel();
        // Hold the registry lock across creation: even immediate delivery must
        // find the waiter, and create_new cannot overwrite a checkout file.
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        pending.insert(path.clone(), tx);
        Ok(CookieTicket {
            path,
            pending: Arc::clone(&self.pending),
            rx,
        })
    }

    pub(crate) fn observe(&self, path: &Path, observation: CookieObservation) {
        if let Some(tx) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path)
        {
            let _ = tx.send(observation);
        }
    }
}

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

    #[test]
    fn watcher_cookie_tickets_match_exact_names_and_cleanup_on_timeout() {
        let root = tempfile::tempdir().unwrap();
        let barrier = CookieBarrier::default();
        let ticket = barrier.begin(root.path()).unwrap();
        let path = ticket.path().to_path_buf();
        assert!(is_cookie_path(&path));
        let observation = CookieObservation {
            revision: 7,
            watcher: WatcherState::Healthy,
        };
        barrier.observe(&root.path().join(".aft-cookie-1-unregistered"), observation);
        assert_eq!(
            ticket.wait_timeout(Duration::ZERO),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        barrier.observe(&path, observation);
        assert_eq!(ticket.wait_timeout(Duration::ZERO), Ok(observation));
        drop(ticket);
        assert!(!path.exists());
        let ticket = barrier.begin(root.path()).unwrap();
        let path = ticket.path().to_path_buf();
        drop(ticket);
        assert!(!path.exists());
        assert!(barrier.pending.lock().unwrap().is_empty());
        for name in [
            ".aft-cookie",
            ".aft-cookie--nonce",
            ".aft-cookie-pid-nonce",
            ".aft-cookie-1-",
        ] {
            assert!(!is_cookie_path(Path::new(name)));
        }
    }
}
