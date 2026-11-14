pub use aft_base::executable_cache::image_cache::*;

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
        #[cfg(target_os = "linux")]
        let child = usage::non_dumpable_child();
        #[cfg(target_os = "linux")]
        let child_path = PathBuf::from(format!("/proc/{}", child.pid));
        #[cfg(target_os = "linux")]
        let snapshot = usage::snapshot_process_paths(
            [
                PathBuf::from(format!("/proc/{}", std::process::id())),
                child_path.clone(),
            ],
            &fs::read_to_string("/proc/self/mountinfo").unwrap(),
        );
        #[cfg(not(target_os = "linux"))]
        let snapshot = usage::snapshot().unwrap();
        #[cfg(target_os = "linux")]
        {
            let unreadable = snapshot
                .unreadable
                .iter()
                .find(|process| process.pid == child.pid)
                .expect("the non-dumpable child must be counted as unreadable");
            assert!(unreadable
                .paths
                .iter()
                .any(|path| path == &child_path || path.ends_with("fd") || path.ends_with("maps")));
        }
        assert_eq!(snapshot.in_use(&directory).unwrap(), usage::ImageUse::Found);
    }

    #[test]
    fn image_cache_unknown_usage_keeps_an_unobserved_image() {
        let root = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let current = entry(root.path(), 99, Duration::ZERO, now);
        let old = entry(root.path(), 1, RETENTION + Duration::from_secs(1), now);
        let mut incomplete = usage::Usage::default();
        incomplete.unreadable.push(usage::UnreadableProcess {
            pid: 123,
            paths: vec![PathBuf::from("/proc/123/maps")],
        });
        assert_eq!(incomplete.in_use(&old).unwrap(), usage::ImageUse::Unknown);
        prune_with(root.path(), &current, now, false, &incomplete, |_| Some(0)).unwrap();
        assert!(
            old.exists(),
            "unknown ownership must defeat age and low-disk eviction"
        );
        let complete = usage::Usage::default();
        assert_eq!(complete.in_use(&old).unwrap(), usage::ImageUse::Absent);
        prune_with(root.path(), &current, now, false, &complete, |_| Some(0)).unwrap();
        assert!(!old.exists(), "known-absent ownership permits eviction");
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

#[cfg(test)]
#[path = "image_cache_usage.rs"]
mod usage;
