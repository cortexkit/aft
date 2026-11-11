//! Last-use retention for shared, identity-keyed executable fixture directories.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Refresh under the publication lock so pruning cannot race a cache hit.
pub fn refresh(directory: &Path) -> io::Result<()> {
    filetime::set_file_mtime(directory, filetime::FileTime::now())
}

/// Prune only identity directories; the current lookup is always protected.
pub fn prune(root: &Path, current: &Path, now: SystemTime) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.path() == current
            || name.len() != 64
            || !name.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        let modified = match entry.metadata() {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if now.duration_since(modified).unwrap_or_default() > RETENTION {
            prune_stale(root, &entry.path(), now, || {})?;
        }
    }
    Ok(())
}

fn prune_stale(
    root: &Path,
    directory: &Path,
    now: SystemTime,
    claimed: impl FnOnce(),
) -> io::Result<()> {
    let quarantine = tempfile::Builder::new()
        .prefix(".pruning-")
        .tempdir_in(root)?;
    let removed = quarantine.path().join("image");
    // Claim the old pathname before the final age check. A concurrent lookup
    // either refreshed this directory already or will republish at its old path.
    // Windows may refuse to move an executing image; leave it for the next scan.
    if fs::rename(directory, &removed).is_err() {
        return Ok(());
    }
    claimed();
    let modified = fs::metadata(&removed)?.modified()?;
    if now.duration_since(modified).unwrap_or_default() <= RETENTION {
        match fs::rename(&removed, directory) {
            Ok(()) => {}
            // A concurrent publisher may already have restored the identity.
            Err(_) if directory.is_dir() => {}
            Err(error) => {
                // Keep a refreshed image even when restoring it was refused.
                let _ = quarantine.keep();
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_cache_prunes_only_stale_siblings() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let directory = |digit: &str, age| {
            let path = root.path().join(digit.repeat(64));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("image"), b"fixture").unwrap();
            filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(now - age))
                .unwrap();
            path
        };
        let stale = directory("a", RETENTION + Duration::from_secs(1));
        let fresh = directory("b", RETENTION - Duration::from_secs(1));
        let boundary = directory("c", RETENTION);
        let current = directory("d", RETENTION + Duration::from_secs(1));
        let refreshed = directory("e", RETENTION + Duration::from_secs(1));
        refresh(&refreshed).unwrap();
        let pending = root.path().join(".publishing-unrelated");
        fs::create_dir(&pending).unwrap();
        filetime::set_file_mtime(&pending, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        prune(root.path(), &current, now).unwrap();
        assert!(!stale.exists());
        for retained in [fresh, boundary, current, refreshed, pending] {
            assert!(retained.exists(), "removed {}", retained.display());
        }
    }

    #[test]
    fn image_cache_restores_a_directory_refreshed_during_pruning() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("a".repeat(64));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("image"), b"fixture").unwrap();
        let now = SystemTime::now();
        filetime::set_file_mtime(
            &directory,
            filetime::FileTime::from_system_time(now - RETENTION - Duration::from_secs(1)),
        )
        .unwrap();
        prune_stale(root.path(), &directory, now, || {
            let quarantine = fs::read_dir(root.path())
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| entry.file_name().to_string_lossy().starts_with(".pruning-"))
                .unwrap();
            refresh(&quarantine.path().join("image")).unwrap();
        })
        .unwrap();
        assert_eq!(fs::read(directory.join("image")).unwrap(), b"fixture");
    }
}
