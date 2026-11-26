//! Writes to the cache of bot logins' numeric GitHub user IDs
//! (`numeric-ids.json`), which co-author trailers are built from.
//!
//! Several `gh` and git-hook processes can look up different logins at once.
//! Each adds its entry under an exclusive lock on `numeric-ids.lock`, reloading
//! the file after taking the lock, so one process's entry is never replaced by
//! another's older copy of the map. The new map is written to a temporary file
//! of this process's own and renamed into place.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::sync::atomic::{AtomicU64, Ordering};

use super::StatePaths;

/// Add `login`'s numeric ID to the cache. Failures are silent: the cache only
/// saves a later lookup.
pub(super) fn record(paths: &StatePaths, login: &str, numeric_id: u64) {
    if crate::private_storage::open_root(&paths.root).is_err() {
        return;
    }
    let Ok(_lock) = lock(paths, login) else {
        return;
    };
    let mut ids = super::cached_numeric_ids(paths);
    #[cfg(test)]
    test_hook::reached(paths, login, test_hook::Point::Loaded);
    ids.insert(login.to_string(), numeric_id);
    write(paths, &ids);
}

/// The exclusive lock, released when the returned file is closed.
fn lock(paths: &StatePaths, _login: &str) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(paths.root.join("numeric-ids.lock"))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            #[cfg(test)]
            test_hook::reached(paths, _login, test_hook::Point::Waiting);
            file.lock()?;
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }
    Ok(file)
}

fn write(paths: &StatePaths, ids: &BTreeMap<String, u64>) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let Ok(bytes) = serde_json::to_vec(ids) else {
        return;
    };
    let temporary = paths.root.join(format!(
        "numeric-ids.json.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if crate::private_storage::write(&temporary, bytes).is_ok() {
        #[cfg(windows)]
        let _ = fs::remove_file(&paths.numeric_ids);
        if fs::rename(&temporary, &paths.numeric_ids).is_err() {
            let _ = fs::remove_file(&temporary);
        }
    }
}

/// Lets a test stop one writer at a chosen point to force an interleaving.
#[cfg(test)]
pub(super) mod test_hook {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::StatePaths;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum Point {
        /// Another writer holds the lock; this one is about to wait for it.
        Waiting,
        /// This writer has the lock and has reloaded the map.
        Loaded,
    }

    type Hook = Arc<dyn Fn(&str, Point) + Send + Sync>;
    static HOOK: Mutex<Option<(PathBuf, Hook)>> = Mutex::new(None);

    pub(crate) fn install(paths: &StatePaths, hook: impl Fn(&str, Point) + Send + Sync + 'static) {
        *HOOK.lock().unwrap() = Some((paths.root.clone(), Arc::new(hook)));
    }

    pub(crate) fn reached(paths: &StatePaths, login: &str, point: Point) {
        let hook = HOOK
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(root, _)| root == &paths.root)
            .map(|(_, hook)| Arc::clone(hook));
        if let Some(hook) = hook {
            hook(login, point);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use super::test_hook::Point;
    use super::*;

    #[test]
    fn concurrent_fills_for_different_logins_keep_both_entries() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Arc::new(StatePaths::from_root(temp.path().join("state")));
        // Writer "a" stops after loading the map until writer "b" has either
        // started waiting for the lock or (without the lock) loaded the same
        // map; then "a" writes. Every interleaving the lock allows must end
        // with both entries.
        let (events, observed) = mpsc::channel::<(String, Point)>();
        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        test_hook::install(&paths, move |login, point| {
            events.send((login.to_string(), point)).unwrap();
            if login == "a" && point == Point::Loaded {
                gate.lock().unwrap().recv().unwrap();
            }
        });

        let first = {
            let paths = Arc::clone(&paths);
            std::thread::spawn(move || record(&paths, "a", 1))
        };
        assert_eq!(observed.recv().unwrap(), ("a".to_string(), Point::Loaded));
        let second = {
            let paths = Arc::clone(&paths);
            std::thread::spawn(move || record(&paths, "b", 2))
        };
        let (login, _) = observed.recv().unwrap();
        assert_eq!(login, "b");
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();

        let ids = crate::gh_shim::cached_numeric_ids(&paths);
        assert_eq!(ids.get("a"), Some(&1));
        assert_eq!(ids.get("b"), Some(&2));
    }
}
