//! Last-use retention for shared, identity-keyed executable fixture directories.

#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[path = "image_cache_usage.rs"]
mod usage;

const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const PRESSURE_RETENTION: Duration = Duration::from_secs(30 * 60);
const LEASE_RETENTION: Duration = Duration::from_secs(3 * 60 * 60);
const FREE_FLOOR: u64 = 40 * 1024 * 1024 * 1024;
const KEEP_UNUSED: usize = 4;

/// Refresh under the publication lock so pruning cannot race a cache hit.
pub fn refresh(directory: &Path) -> io::Result<()> {
    let leases = directory.join(".leases");
    fs::create_dir_all(&leases)?;
    fs::write(leases.join(std::process::id().to_string()), [])?;
    filetime::set_file_mtime(directory, filetime::FileTime::now())
}

/// Prune only identity directories; the current lookup is always protected.
pub fn prune(root: &Path, current: &Path, now: SystemTime) -> io::Result<()> {
    prune_cache(root, current, now, false)
}

/// Include old Cargo/nextest cache roots left behind by older test binaries.
pub fn prune_real_images(root: &Path, current: &Path, now: SystemTime) -> io::Result<()> {
    prune_cache(root, current, now, true)
}

fn prune_cache(root: &Path, current: &Path, now: SystemTime, legacy: bool) -> io::Result<()> {
    let free = usage::free_bytes(root);
    let candidates = cache_entries(root, legacy)?;
    let pids = usage::Usage::default();
    let mut unused = Vec::new();
    for entry in &candidates {
        let live_lease = match leased(&entry.0, now, &pids) {
            Ok(active) => active,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if entry.0 != current && !live_lease && !entry.2.is_some_and(|pid| pids.pid_alive(pid)) {
            unused.push(entry);
        }
    }
    if unused.len() <= KEEP_UNUSED
        && !unused.iter().any(|entry| {
            let age = now.duration_since(entry.1).unwrap_or_default();
            age > RETENTION
                || (free.is_some_and(|free| free < FREE_FLOOR) && age > PRESSURE_RETENTION)
        })
    {
        return Ok(());
    }
    let Ok(usage) = usage::snapshot() else {
        // Incomplete process visibility cannot establish that an image is unused.
        return Ok(());
    };
    prune_with(root, current, now, legacy, &usage, |_| free)
}

fn identity(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn legacy_pid(name: &str) -> Option<Option<u32>> {
    if let Some(hash) = name.strip_prefix("aft-real-module-images-nextest-") {
        return identity(hash).then_some(None);
    }
    let pid = name.strip_prefix("aft-real-module-images-cargo-")?;
    let number = pid.parse::<u32>().ok()?;
    (number > 0 && number.to_string() == pid).then_some(Some(number))
}

fn last_used(directory: &Path) -> io::Result<SystemTime> {
    let mut modified = fs::metadata(directory)?.modified()?;
    for child in fs::read_dir(directory)? {
        let child = child?;
        if child.file_type()?.is_dir() && identity(&child.file_name().to_string_lossy()) {
            modified = modified.max(child.metadata()?.modified()?);
        }
    }
    Ok(modified)
}

fn leased(directory: &Path, now: SystemTime, usage: &usage::Usage) -> io::Result<bool> {
    let mut active = false;
    let children = match fs::read_dir(directory.join(".leases")) {
        Ok(children) => children,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for child in children {
        let child = child?;
        let name = child.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse() else {
            continue;
        };
        if !child.file_type()?.is_file() {
            continue;
        }
        let age = now
            .duration_since(child.metadata()?.modified()?)
            .unwrap_or_default();
        if age < LEASE_RETENTION && usage.pid_alive(pid) {
            active = true;
        } else {
            let _ = fs::remove_file(child.path());
        }
    }
    Ok(active)
}

fn protected(
    directory: &Path,
    pid: Option<u32>,
    now: SystemTime,
    usage: &usage::Usage,
) -> io::Result<bool> {
    Ok(pid.is_some_and(|pid| usage.pid_alive(pid))
        || leased(directory, now, usage)?
        || usage.in_use(directory)?)
}

fn cache_entries(root: &Path, legacy: bool) -> io::Result<Vec<(PathBuf, SystemTime, Option<u32>)>> {
    let mut entries: Vec<(PathBuf, SystemTime, Option<u32>)> = Vec::new();
    let mut add = |path: PathBuf, pid| -> io::Result<()> {
        match last_used(&path) {
            Ok(modified) => entries.push((path, modified, pid)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(())
    };
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if identity(&name) && entry.file_type()?.is_dir() {
            add(entry.path(), None)?;
        }
    }
    if legacy {
        for entry in fs::read_dir(root.parent().expect("image cache has a parent"))? {
            let entry = entry?;
            if let Some(pid) = legacy_pid(&entry.file_name().to_string_lossy()) {
                if entry.file_type()?.is_dir() {
                    add(entry.path(), pid)?;
                }
            }
        }
    }
    Ok(entries)
}

fn prune_with(
    root: &Path,
    current: &Path,
    now: SystemTime,
    legacy: bool,
    usage: &usage::Usage,
    free_bytes: impl FnOnce(&Path) -> Option<u64>,
) -> io::Result<()> {
    let mut entries = cache_entries(root, legacy)?;
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let low_disk = free_bytes(root).is_some_and(|free| free < FREE_FLOOR);
    let mut unused = 0;
    for (directory, modified, pid) in entries {
        if directory == current {
            continue;
        }
        match protected(&directory, pid, now, usage) {
            Ok(true) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
            Ok(false) => {}
        }
        let age = now.duration_since(modified).unwrap_or_default();
        let remove =
            unused >= KEEP_UNUSED || age > RETENTION || (low_disk && age > PRESSURE_RETENTION);
        unused += 1;
        if remove {
            prune_stale(&directory, modified, pid, now, usage, || {})?;
        }
    }
    Ok(())
}

fn prune_stale(
    directory: &Path,
    observed: SystemTime,
    pid: Option<u32>,
    now: SystemTime,
    usage: &usage::Usage,
    claimed: impl FnOnce(),
) -> io::Result<()> {
    let quarantine = tempfile::Builder::new()
        .prefix(".pruning-")
        .tempdir_in(directory.parent().expect("cache entry has a parent"))?;
    let removed = quarantine.path().join("image");
    // Claim the old pathname before the final age check. A concurrent lookup
    // either refreshed this directory already or will republish at its old path.
    // Windows may refuse to move an executing image; leave it for the next scan.
    if fs::rename(directory, &removed).is_err() {
        return Ok(());
    }
    claimed();
    let retain = (|| -> io::Result<bool> {
        Ok(last_used(&removed)? > observed || protected(&removed, pid, now, usage)?)
    })()
    .unwrap_or(true);
    if retain {
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

    fn entry(root: &Path, number: usize, age: Duration, now: SystemTime) -> PathBuf {
        let directory = root.join(format!("{number:064x}"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("image"), b"fixture").unwrap();
        filetime::set_file_mtime(&directory, filetime::FileTime::from_system_time(now - age))
            .unwrap();
        directory
    }

    fn lease(directory: &Path, pid: u32, modified: SystemTime) {
        let leases = directory.join(".leases");
        let previous = fs::metadata(directory).unwrap().modified().unwrap();
        fs::create_dir_all(&leases).unwrap();
        fs::write(leases.join(pid.to_string()), []).unwrap();
        filetime::set_file_mtime(
            leases.join(pid.to_string()),
            filetime::FileTime::from_system_time(modified),
        )
        .unwrap();
        filetime::set_file_mtime(directory, filetime::FileTime::from_system_time(previous))
            .unwrap();
    }

    #[test]
    fn image_cache_keeps_four_unused_plus_live_leases_and_open_images() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let current = entry(root.path(), 99, Duration::ZERO, now);
        let mut usage = usage::Usage::default();
        usage.live_pids = Some([123].into());
        let live = entry(root.path(), 20, Duration::from_secs(4 * 3600), now);
        lease(&live, 123, now);
        let expired = entry(root.path(), 21, Duration::from_secs(4 * 3600), now);
        lease(&expired, 123, now - LEASE_RETENTION);
        let open = entry(root.path(), 22, RETENTION + Duration::from_secs(1), now);
        usage.hold(&open.join("image"));
        let unused: Vec<_> = (1..=6)
            .map(|n| entry(root.path(), n, Duration::from_secs(n as u64), now))
            .collect();
        prune_with(root.path(), &current, now, false, &usage, |_| {
            Some(FREE_FLOOR)
        })
        .unwrap();
        assert!(live.exists(), "a fresh lease protects a delayed exec");
        assert!(open.exists(), "an open image outlives age and count limits");
        assert!(
            !expired.exists(),
            "a reused live PID cannot retain a three-hour-old lease"
        );
        for (index, directory) in unused.iter().enumerate() {
            assert_eq!(directory.exists(), index < KEEP_UNUSED);
        }
    }

    #[test]
    fn image_cache_low_disk_uses_thirty_minute_grace() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let current = entry(root.path(), 99, RETENTION + Duration::from_secs(1), now);
        let stale = entry(
            root.path(),
            1,
            PRESSURE_RETENTION + Duration::from_secs(1),
            now,
        );
        let boundary = entry(root.path(), 2, PRESSURE_RETENTION, now);
        let fresh = entry(
            root.path(),
            3,
            PRESSURE_RETENTION - Duration::from_secs(1),
            now,
        );
        let usage = usage::Usage::default();
        prune_with(root.path(), &current, now, false, &usage, |_| {
            Some(FREE_FLOOR)
        })
        .unwrap();
        assert!(stale.exists(), "exactly 40 GiB is not low disk");
        prune_with(root.path(), &current, now, false, &usage, |_| {
            Some(FREE_FLOOR - 1)
        })
        .unwrap();
        assert!(!stale.exists());
        assert!(boundary.exists());
        assert!(fresh.exists());
        assert!(current.exists());
    }

    #[test]
    fn image_cache_legacy_sweep_matches_exact_names_and_holds_live_cargo_pid() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("aft-real-module-images");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let current = entry(&root, 99, Duration::ZERO, now);
        for n in 1..=4 {
            entry(&root, n, Duration::from_secs(n as u64), now);
        }
        let names = [
            format!("aft-real-module-images-nextest-{}", "a".repeat(64)),
            "aft-real-module-images-cargo-123".into(),
            "aft-real-module-images-cargo-124".into(),
            "aft-real-module-images-nextest-not-a-hash".into(),
            format!("aft-real-module-images-nextest-{}-suffix", "a".repeat(64)),
            "aft-real-module-images-cargo-0124".into(),
            "aft-real-module-images-cargo-124-extra".into(),
        ];
        for name in &names {
            let directory = temp.path().join(name);
            fs::create_dir(&directory).unwrap();
            filetime::set_file_mtime(
                directory,
                filetime::FileTime::from_system_time(now - Duration::from_secs(3 * 3600)),
            )
            .unwrap();
        }
        let mut usage = usage::Usage::default();
        usage.live_pids = Some([123].into());
        prune_with(&root, &current, now, true, &usage, |_| Some(FREE_FLOOR)).unwrap();
        assert!(!temp.path().join(&names[0]).exists());
        assert!(temp.path().join(&names[1]).exists());
        assert!(!temp.path().join(&names[2]).exists());
        for name in &names[3..] {
            assert!(temp.path().join(name).exists(), "matched unrelated {name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn image_cache_live_probe_finds_open_image_and_hardlink_alias() {
        let root = tempfile::tempdir().unwrap();
        let directory = entry(root.path(), 1, Duration::ZERO, SystemTime::now());
        let alias = root.path().join("alias");
        fs::hard_link(directory.join("image"), &alias).unwrap();
        let _open = fs::File::open(alias).unwrap();
        assert!(usage::snapshot().unwrap().in_use(&directory).unwrap());
    }

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
        prune_with(
            root.path(),
            &current,
            now,
            false,
            &usage::Usage::default(),
            |_| Some(FREE_FLOOR),
        )
        .unwrap();
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
        let observed = last_used(&directory).unwrap();
        prune_stale(
            &directory,
            observed,
            None,
            now,
            &usage::Usage::default(),
            || {
                let quarantine = fs::read_dir(root.path())
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| entry.file_name().to_string_lossy().starts_with(".pruning-"))
                    .unwrap();
                refresh(&quarantine.path().join("image")).unwrap();
            },
        )
        .unwrap();
        assert_eq!(fs::read(directory.join("image")).unwrap(), b"fixture");
    }
}
