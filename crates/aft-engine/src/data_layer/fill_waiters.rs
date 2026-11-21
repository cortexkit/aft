//! Fill rounds waiting for another fill of their repository.
//!
//! A fill round that finds its repository's fill lock held by another fill,
//! in this process or another, ends at once instead of waiting in one of the
//! fill pool's few places. Each waiting checkout leaves a wake-up here. One
//! thread per repository, outside every pool and holding no AFT lock, blocks
//! on the lock file until the other fill lets it go, then runs every wake-up
//! left for that repository. That thread may stay blocked for as long as the
//! other fill holds the lock, which a slow embedding model can make minutes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What a waiting checkout runs once the lock is free.
pub(super) type Wake = Box<dyn FnOnce() + Send>;

fn waiting() -> &'static parking_lot::Mutex<HashMap<PathBuf, Vec<Wake>>> {
    static WAITING: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Vec<Wake>>>> = OnceLock::new();
    WAITING.get_or_init(Default::default)
}

#[cfg(test)]
static THREADS_STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Runs `wake` once no fill holds the fill lock of the family in
/// `family_dir`. A family's first waiter starts its waiting thread; later
/// ones join it, so a family never has two. Returns false, dropping `wake`
/// unrun, when the thread cannot be started; the caller then retries the
/// fill on its own schedule.
pub(super) fn wake_when_free(family_dir: &Path, wake: Wake) -> bool {
    let mut families = waiting().lock();
    if let Some(wakes) = families.get_mut(family_dir) {
        wakes.push(wake);
        return true;
    }
    families.insert(family_dir.to_path_buf(), vec![wake]);
    let dir = family_dir.to_path_buf();
    let started = std::thread::Builder::new()
        .name("aft-data-fill-wait".to_string())
        .spawn(move || {
            if let Err(error) = super::vectors::wait_for_other_fill(&dir) {
                // The woken fills meet the same error and report it.
                crate::slog_warn!(
                    "data layer could not wait for the fill lock of {}: {error}",
                    dir.display()
                );
            }
            // Taken out before any wake runs, so a wake that finds the lock
            // held again starts a new waiting thread.
            let wakes = waiting().lock().remove(&dir).unwrap_or_default();
            for wake in wakes {
                wake();
            }
        });
    match started {
        Ok(_) => {
            #[cfg(test)]
            THREADS_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            true
        }
        Err(error) => {
            families.remove(family_dir);
            crate::slog_warn!("data layer could not start a fill-lock waiter: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Two checkouts of one repository wait on one thread, and neither is
    /// woken while the other fill still holds the lock.
    #[test]
    fn waiters_of_one_family_share_one_thread_and_wake_when_the_lock_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(super::super::vectors::FILL_LOCK);
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        holder.lock().unwrap();

        let (woken_tx, woken_rx) = mpsc::channel();
        let before = THREADS_STARTED.load(Ordering::SeqCst);
        for checkout in ["first", "second"] {
            let woken = woken_tx.clone();
            assert!(wake_when_free(
                dir.path(),
                Box::new(move || woken.send(checkout).unwrap())
            ));
        }
        assert_eq!(
            THREADS_STARTED.load(Ordering::SeqCst) - before,
            1,
            "a second waiter of the family started its own thread"
        );
        assert_eq!(waiting().lock()[dir.path()].len(), 2);
        // Nothing wakes while the lock is held: the thread is blocked on it.
        assert!(woken_rx.try_recv().is_err());

        drop(holder);
        let mut woken = (0..2)
            .map(|_| woken_rx.recv_timeout(Duration::from_secs(30)).unwrap())
            .collect::<Vec<_>>();
        woken.sort_unstable();
        assert_eq!(woken, ["first", "second"]);
        assert!(!waiting().lock().contains_key(dir.path()));
    }
}
