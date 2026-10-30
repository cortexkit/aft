//! Ordered watcher acknowledgments for command-completion change reports.
use notify::Event;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// How long a finished command waits for the watcher to acknowledge its flush marker.
///
/// Native Windows file notifications are slower and less prompt than inotify or
/// FSEvents: on a busy machine building the marker file itself can take most of a
/// second, so a 300 ms cap reports changes as incomplete even though every file was
/// seen. The wait still ends as soon as the acknowledgement arrives; the larger
/// Windows cap only bounds the worst case where it never does.
#[cfg(windows)]
pub(crate) const BASH_FLUSH_LIMIT: Duration = Duration::from_millis(1500);
#[cfg(not(windows))]
pub(crate) const BASH_FLUSH_LIMIT: Duration = Duration::from_millis(300);
const MIN_MARKER_BUDGET: Duration = Duration::from_millis(100);
const MARKER_PREFIX: &str = ".aft-flush-";
const GIT_MARKER_PREFIX: &str = "aft-flush-";

#[derive(Debug)]
struct Pending {
    marker: PathBuf,
    ack: mpsc::Sender<bool>,
}

#[derive(Debug, Default)]
pub(crate) struct FlushControl {
    next: AtomicU64,
    pending: Mutex<BTreeMap<u64, Pending>>,
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    marker_directory: Mutex<Option<PathBuf>>,
    #[cfg(target_os = "macos")]
    native: Mutex<Option<mpsc::Sender<FlushRequest>>>,
    #[cfg(test)]
    pub(crate) force_timeout: std::sync::atomic::AtomicBool,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(super) struct FlushRequest {
    pub(super) marker_id: Option<u64>,
    pub(super) deadline: Instant,
    #[cfg(test)]
    pub(super) flushed: Option<mpsc::Sender<()>>,
}

impl FlushControl {
    /// Test-only comparison with the native flush, without waiting for a marker event.
    #[cfg(all(test, target_os = "macos"))]
    #[allow(dead_code)]
    pub(crate) fn native_flush_only(&self, deadline: Instant) -> bool {
        let (tx, rx) = mpsc::channel();
        self.native
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|native| {
                native
                    .send(FlushRequest {
                        deadline,
                        flushed: Some(tx),
                        marker_id: None,
                    })
                    .is_ok()
            })
            && rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .is_ok()
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    pub(super) fn set_marker_directory(&self, path: PathBuf) {
        *self
            .marker_directory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }
    #[cfg(target_os = "macos")]
    pub(super) fn register_native(&self) -> mpsc::Receiver<FlushRequest> {
        let (tx, rx) = mpsc::channel();
        *self
            .native
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tx);
        rx
    }
    #[cfg(target_os = "macos")]
    pub(super) fn is_pending(&self, id: u64) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id)
    }

    pub(crate) fn wait(&self, root: &Path, deadline: Instant) -> bool {
        // Do not create a file when the caller has too little time to remove it.
        if deadline.saturating_duration_since(Instant::now()) < MIN_MARKER_BUDGET {
            return false;
        }
        #[cfg(test)]
        if self.force_timeout.load(Ordering::Acquire) {
            return false;
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("AFT_TEST_BASH_CHANGED_FILES_FLUSH_TIMEOUT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            return false;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let marker = {
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            let directory = self
                .marker_directory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_else(|| root.to_path_buf());
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let directory = root.to_path_buf();
            let prefix = if directory == root.join(".git") {
                GIT_MARKER_PREFIX
            } else {
                MARKER_PREFIX
            };
            directory.join(format!("{prefix}{}-{}", std::process::id(), id))
        };
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                Pending {
                    marker: marker.clone(),
                    ack: tx.clone(),
                },
            );
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let (cleaned_tx, cleaned_rx) = mpsc::channel();
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let event_deadline = deadline
            - Duration::from_millis(100)
                .min(deadline.saturating_duration_since(Instant::now()) / 2);
        #[cfg(target_os = "macos")]
        let native = self
            .native
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let started = {
            // Use .git only when the backend confirmed it is watched; otherwise
            // the root stays watched even when excluded subdirectories are skipped.
            // Creating the empty marker exclusively cannot overwrite a project
            // file. Its native event follows earlier writes on the
            // same native event stream. FSEvents also flushes its pending batch,
            // but success still requires observing the marker itself.
            let marker = marker.clone();
            std::thread::Builder::new()
                .name("aft-watcher-marker".into())
                .spawn(move || {
                    if std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&marker)
                        .is_err()
                    {
                        let _ = tx.send(false);
                        let _ = cleaned_tx.send(false);
                        return;
                    }
                    #[cfg(target_os = "macos")]
                    if let Some(native) = native {
                        let _ = native.send(FlushRequest {
                            marker_id: Some(id),
                            deadline,
                            #[cfg(test)]
                            flushed: None,
                        });
                    }
                    let _ = cleanup_rx
                        .recv_timeout(event_deadline.saturating_duration_since(Instant::now()));
                    let removed = std::fs::remove_file(&marker)
                        .or_else(|error| {
                            if error.kind() == std::io::ErrorKind::NotFound {
                                Ok(())
                            } else {
                                std::fs::remove_file(&marker)
                            }
                        })
                        .is_ok();
                    let _ = cleaned_tx.send(removed);
                })
                .is_ok()
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        let started = false;
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let wait_deadline = event_deadline;
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let wait_deadline = deadline;
        let caught_up = started
            && rx
                .recv_timeout(wait_deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(false);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let _ = cleanup_tx.send(());
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        let caught_up = started
            && cleaned_rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or(false)
            && caught_up;
        caught_up
    }

    /// Called on the raw intake, after preceding changes entered the journal.
    /// Marker creation/removal never becomes a user file change.
    pub(crate) fn observe(&self, event: &Event) -> bool {
        // A control filename must never hide a native overflow notification.
        if event.need_rescan() {
            return false;
        }
        let marker_event = event.paths.iter().any(|path| is_flush_marker_path(path));
        if !marker_event {
            return false;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids = pending
            .iter()
            .filter_map(|(id, wait)| event.paths.contains(&wait.marker).then_some(*id))
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(wait) = pending.remove(&id) {
                let _ = wait.ack.send(true);
            }
        }
        true
    }
}

pub(crate) fn is_marker_path(path: &Path) -> bool {
    // Corpus admission excludes both protocols, but command-report intake must
    // not consume a views cookie before it reaches the driver's ordered queue.
    super::cookie::is_cookie_path(path) || is_flush_marker_path(path)
}

fn is_flush_marker_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let suffix = name.strip_prefix(MARKER_PREFIX).or_else(|| {
        path.parent()
            .and_then(Path::file_name)
            .filter(|parent| *parent == ".git")
            .and_then(|_| name.strip_prefix(GIT_MARKER_PREFIX))
    });
    suffix
        .and_then(|suffix| suffix.split_once('-'))
        .is_some_and(|(pid, number)| {
            !pid.is_empty()
                && !number.is_empty()
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    /// A flush control whose first marker acknowledgement arrives only after a
    /// delay, standing in for a watcher that acknowledges once the file system
    /// has caught up rather than immediately.
    fn control_with_ack_delay(root: &Path, delay: Duration) -> Arc<FlushControl> {
        let control = Arc::new(FlushControl::default());
        let acknowledged = control.clone();
        std::thread::spawn(move || {
            // Only the first wait is held back; a later wait in the same test
            // would otherwise pay the delay again.
            let mut delay = Some(delay);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let acks = {
                    let pending = acknowledged.pending.lock().unwrap();
                    if pending.is_empty() {
                        if Instant::now() >= deadline {
                            return;
                        }
                        drop(pending);
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    drop(pending);
                    if let Some(delay) = delay.take() {
                        std::thread::sleep(delay);
                    }
                    std::mem::take(&mut *acknowledged.pending.lock().unwrap())
                };
                for (_, wait) in acks {
                    let _ = wait.ack.send(true);
                }
            }
        });
        control.set_marker_directory(root.to_path_buf());
        control
    }

    #[cfg(windows)]
    #[test]
    fn bash_changed_files_flush_limit_is_1_5s_on_windows() {
        // Native Windows notification delivery can take most of a second on a
        // loaded runner, so the Windows cap leaves room for it.
        assert_eq!(BASH_FLUSH_LIMIT, Duration::from_millis(1500));
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_changed_files_flush_limit_stays_300ms_off_windows() {
        assert_eq!(BASH_FLUSH_LIMIT, Duration::from_millis(300));
    }

    #[cfg(windows)]
    #[test]
    fn bash_changed_files_flush_waits_for_late_acknowledgement() {
        // Both delays are past the 300 ms the other platforms allow but inside
        // the Windows cap, so only Windows has to keep waiting for them. The
        // control acknowledges without a real watcher, which keeps this test's
        // own timer off the shared machine load, but the kernel still needs
        // room to schedule its threads, so give the runs a cap with headroom.
        for delay in [Duration::from_millis(400), Duration::from_millis(900)] {
            let root = tempfile::tempdir().unwrap();
            let control = control_with_ack_delay(root.path(), delay);
            let budget = delay + Duration::from_secs(5);
            assert!(
                control.wait(root.path(), Instant::now() + budget),
                "a {delay:?} acknowledgement arrives well inside {budget:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn bash_changed_files_flush_limit_leaves_room_for_a_late_acknowledgement() {
        // The Windows cap has to outlast the waits measured on a busy machine:
        // about 900 ms for the marker file to be created and its event delivered.
        assert!(BASH_FLUSH_LIMIT > Duration::from_millis(900));
    }

    #[test]
    fn bash_changed_files_flush_reports_incomplete_past_the_limit() {
        // The delay outlasts the cap, so the pending wait must give up and the
        // late acknowledgement must not turn that into a complete report.
        let delay = BASH_FLUSH_LIMIT + Duration::from_millis(600);
        let root = tempfile::tempdir().unwrap();
        let control = control_with_ack_delay(root.path(), delay);
        let started = Instant::now();
        assert!(!control.wait(root.path(), started + BASH_FLUSH_LIMIT));
        assert!(started.elapsed() < delay);
    }

    #[test]
    fn bash_changed_files_flush_acknowledges_only_observed_marker() {
        let control = FlushControl::default();
        let marker = PathBuf::from(".aft-flush-123-1");
        let (tx, rx) = mpsc::channel();
        control.pending.lock().unwrap().insert(
            1,
            Pending {
                marker: marker.clone(),
                ack: tx,
            },
        );
        assert!(!control
            .observe(&Event::new(notify::EventKind::Any).add_path(PathBuf::from("changed.txt"))));
        assert!(control.observe(
            &Event::new(notify::EventKind::Any).add_path(PathBuf::from(".aft-flush-123-2"))
        ));
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert!(control.observe(&Event::new(notify::EventKind::Any).add_path(marker.clone())));
        assert_eq!(rx.try_recv(), Ok(true));
        assert!(control.pending.lock().unwrap().is_empty());
        assert!(control.observe(&Event::new(notify::EventKind::Any).add_path(marker)));
        assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn bash_changed_files_flush_marker_does_not_hide_overflow() {
        let event = Event::new(notify::EventKind::Other)
            .add_path(PathBuf::from(".aft-flush-123-1"))
            .set_flag(notify::event::Flag::Rescan);
        assert!(!FlushControl::default().observe(&event));
    }

    #[test]
    fn bash_changed_files_flush_marker_collision_preserves_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let marker = root
            .path()
            .join(format!(".aft-flush-{}-0", std::process::id()));
        std::fs::write(&marker, "pre-existing file").unwrap();
        let control = FlushControl::default();
        assert!(!control.wait(root.path(), Instant::now() + BASH_FLUSH_LIMIT));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "pre-existing file"
        );
    }
}
