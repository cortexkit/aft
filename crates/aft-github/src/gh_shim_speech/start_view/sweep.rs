use std::collections::HashMap;
use std::fs::{self, ReadDir};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

const MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_ENTRIES: usize = 4096;
const MAX_PASS: Duration = Duration::from_millis(100);

type Pending = HashMap<PathBuf, Option<ReadDir>>;
static PENDING: OnceLock<Mutex<Pending>> = OnceLock::new();

pub(crate) fn queue_sweep(storage: &Path) {
    let root = crate::environment::non_empty_var("AFT_GH_SHIM_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| storage.join("gh-shim"));
    queue_root(root);
}

pub(super) fn queue_root(root: PathBuf) {
    PENDING
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(root)
        .or_default();
}

/// Called only by the detached watchdog cleanup worker. Retaining the iterator
/// between bounded passes makes progress even through a large fresh backlog.
pub(crate) fn sweep_queued() {
    let Some(pending) = PENDING.get() else { return };
    // Never hold this mutex during filesystem work: startup and launch only
    // enqueue a path and must not wait behind a sweep's directory iteration.
    let queued = std::mem::take(&mut *pending.lock().unwrap_or_else(|error| error.into_inner()));
    for (root, entries) in queued {
        let mut entries = match entries.map(Ok).unwrap_or_else(|| fs::read_dir(&root)) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("speech start sweep {}: {error}", root.display());
                }
                continue;
            }
        };
        let stats = sweep_entries(
            &mut entries,
            SystemTime::now(),
            MAX_ENTRIES,
            Instant::now() + MAX_PASS,
        );
        log::info!(
            "speech start sweep root={} examined={} removed={} bounded={}",
            root.display(),
            stats.examined,
            stats.removed,
            !stats.exhausted
        );
        if !stats.exhausted {
            pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(root, Some(entries));
        }
    }
}

#[derive(Default, Debug)]
struct Stats {
    examined: usize,
    removed: usize,
    exhausted: bool,
}

fn sweep_entries(
    entries: &mut impl Iterator<Item = std::io::Result<fs::DirEntry>>,
    now: SystemTime,
    max_entries: usize,
    deadline: Instant,
) -> Stats {
    let mut stats = Stats::default();
    // Bound before advancing the iterator, not after collecting the directory.
    while stats.examined < max_entries && Instant::now() < deadline {
        let Some(entry) = entries.next() else {
            stats.exhausted = true;
            break;
        };
        stats.examined += 1;
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("speech-start-")
            || !name.ends_with(".json")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let path = entry.path();
        let Ok(file) = crate::private_storage::options()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        let old = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= MIN_AGE);
        // The default command limit is 30 minutes, but explicit limits and
        // renewed worker waits have no finite maximum. Age alone is not proof
        // of quiescence: live new views hold a shared flock/LockFileEx lock.
        // Legacy digest-only views have no lock protocol; after 24 hours they
        // are assumed abandoned so pre-upgrade crash backlogs can be reclaimed.
        if !old || file.try_lock().is_err() {
            continue;
        }
        let _ = file.unlock();
        drop(file);
        // Names are never reused by new launches. Release the probe immediately
        // (also allowing Windows deletion), rather than holding it past the check.
        if fs::remove_file(path).is_ok() {
            stats.removed += 1;
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_file(root: &Path, name: &str, now: SystemTime) -> PathBuf {
        let path = root.join(name);
        let file = File::create(&path).unwrap();
        file.set_modified(now - MIN_AGE - Duration::from_secs(1))
            .unwrap();
        path
    }

    use std::fs::File;

    #[test]
    fn sweep_removes_old_views_but_keeps_fresh_unrelated_and_live_views() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = old_file(root.path(), "speech-start-legacy-digest.json", now);
        let unrelated = old_file(root.path(), "other.json", now);
        let fresh = root.path().join("speech-start-fresh.json");
        fs::write(&fresh, b"fresh").unwrap();
        let live = old_file(root.path(), "speech-start-live.json", now);
        let file = File::options().read(true).write(true).open(&live).unwrap();
        file.try_lock_shared().unwrap();
        // Reading and closing a second descriptor must not release the guard's
        // lock (POSIX record locks would incorrectly allow that).
        assert!(fs::read(&live).is_ok());
        let mut entries = fs::read_dir(root.path()).unwrap();
        let stats = sweep_entries(
            &mut entries,
            now,
            100,
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(stats.examined, 4);
        assert_eq!(stats.removed, 1);
        assert!(stats.exhausted);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(unrelated.exists());
        assert!(live.exists());
        file.unlock().unwrap();
        drop(file);
        let stats = sweep_entries(
            &mut fs::read_dir(root.path()).unwrap(),
            now,
            100,
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(stats.removed, 1);
        assert!(!live.exists());
    }

    #[test]
    fn sweep_bounds_iterator_advances_and_resumes_after_count_or_deadline() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for index in 0..8 {
            old_file(root.path(), &format!("speech-start-{index}.json"), now);
        }
        let mut advances = 0;
        let mut entries = fs::read_dir(root.path())
            .unwrap()
            .inspect(|_| advances += 1);
        let stats = sweep_entries(
            &mut entries,
            now,
            3,
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(
            (stats.examined, stats.removed, stats.exhausted),
            (3, 3, false)
        );
        let expired = sweep_entries(&mut entries, now, 100, Instant::now());
        assert_eq!((expired.examined, expired.removed), (0, 0));
        let resumed = sweep_entries(
            &mut entries,
            now,
            100,
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(
            (resumed.examined, resumed.removed, resumed.exhausted),
            (5, 5, true)
        );
        assert_eq!(advances, 8);
    }
}
