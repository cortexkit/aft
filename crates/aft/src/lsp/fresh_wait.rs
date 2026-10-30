use std::collections::HashMap;
use std::ops::DerefMut;
use std::path::Path;
use std::time::Instant;

use super::manager::{LspManager, PostEditWaitOutcome, PreEditSnapshot};
use super::roots::ServerKey;

/// Wait for authoritative diagnostics for the versions returned by
/// `notify_file_changed_versioned`, using the snapshot taken before that notify.
/// Older, cached, and unversioned reports cannot complete the wait.
///
/// The lock closure is used only for registration, polling, and collection;
/// channel waits never hold the manager mutex. The absolute deadline preserves
/// the caller's budget even if acquiring the manager takes time. Callers using
/// pull-only servers must request diagnostics before entering this helper.
pub fn wait_for_post_edit_diagnostics_unlocked<G>(
    lock: impl Fn() -> G,
    file_path: &Path,
    expected_versions: &[(ServerKey, i32)],
    pre_snapshot: &HashMap<ServerKey, PreEditSnapshot>,
    deadline: Instant,
) -> PostEditWaitOutcome
where
    G: DerefMut<Target = LspManager>,
{
    let mut lsp = lock();
    let remaining = deadline.saturating_duration_since(Instant::now());

    // Register under the lock so an event consumed by another drain still wakes
    // this waiter after that drain updates the diagnostic store.
    let mut wait =
        lsp.start_post_edit_diagnostics_wait(file_path, expected_versions, pre_snapshot, remaining);
    let mut complete = lsp.poll_post_edit_diagnostics_wait(&mut wait, None);
    drop(lsp);

    while !complete && !wait.deadline_reached() {
        #[cfg(all(test, unix))]
        PARK_PROBE.with(|probe| {
            if let Some(probe) = probe.borrow().as_ref() {
                probe();
            }
        });
        let event = wait.next_event();
        let mut lsp = lock();
        complete = lsp.poll_post_edit_diagnostics_wait(&mut wait, event);
    }

    lock().finish_post_edit_diagnostics_wait(wait)
}

#[cfg(all(test, unix))]
thread_local! {
    static PARK_PROBE: std::cell::RefCell<Option<Box<dyn Fn()>>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(all(test, unix))]
#[path = "fresh_wait_regression_tests.rs"]
mod tests;
