pub use aft_base::cache_freshness::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn audit10_freshness_hash_streams_bounded_buffers() {
        struct MeasuredReader<'a> {
            bytes: std::io::Cursor<Vec<u8>>,
            largest: &'a mut usize,
            consumed: &'a mut usize,
        }
        impl std::io::Read for MeasuredReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                *self.largest = (*self.largest).max(buffer.len());
                let read = self.bytes.read(buffer)?;
                *self.consumed += read;
                Ok(read)
            }
        }
        let data = vec![b'x'; 64 * 1024];
        let mut largest = 0;
        let mut consumed = 0;
        let (hash, bytes_read) = hash_small_reader(MeasuredReader {
            bytes: std::io::Cursor::new(data.clone()),
            largest: &mut largest,
            consumed: &mut consumed,
        })
        .unwrap();
        assert_eq!(hash, Some(blake3::hash(&data)));
        assert_eq!(bytes_read, data.len() as u64);
        assert_eq!(consumed, data.len());
        assert!(largest <= 8192, "read buffer grew to {largest}");
        let data = vec![b'x'; CONTENT_HASH_SIZE_CAP as usize + 16384];
        let mut largest = 0;
        let mut consumed = 0;
        let (hash, _) = hash_small_reader(MeasuredReader {
            bytes: std::io::Cursor::new(data),
            largest: &mut largest,
            consumed: &mut consumed,
        })
        .unwrap();
        assert!(hash.is_none());
        assert_eq!(consumed as u64, CONTENT_HASH_SIZE_CAP + 1);
    }

    #[test]
    fn audit10_file_growing_past_the_cap_has_no_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grown.rs");
        fs::write(&path, vec![b'x'; CONTENT_HASH_SIZE_CAP as usize + 1]).unwrap();
        // The caller's stat said the file was small; it grew afterwards.
        assert!(hash_file_if_small(&path, 1).unwrap().is_none());
        fs::write(&path, b"small").unwrap();
        assert_eq!(
            hash_file_if_small(&path, 5).unwrap(),
            Some(blake3::hash(b"small"))
        );
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    /// Phase-3 gating benchmark: stat-all (non-strict verify_file) vs hash-all
    /// (strict verify_file_strict) cost across a real repo's file set, with NO
    /// file changed (the steady-state warm freshness pass). Decides Option B:
    /// if stat-all is cheap even at large file counts, replace the per-edit
    /// hash-all with stat-diff-first.
    ///
    ///   AFT_BENCH_REPO=/path/to/repo cargo test -p agent-file-tools --lib \
    ///     --release -- --ignored --nocapture --test-threads=1 \
    ///     freshness_stat_vs_hash_benchmark
    #[test]
    #[ignore = "manual benchmark; needs AFT_BENCH_REPO"]
    fn freshness_stat_vs_hash_benchmark() {
        use std::time::Instant;
        let Ok(repo) = std::env::var("AFT_BENCH_REPO") else {
            eprintln!("AFT_BENCH_REPO unset; skipping");
            return;
        };
        let root = std::path::PathBuf::from(&repo);
        let files: Vec<std::path::PathBuf> = crate::callgraph::walk_project_files(&root).collect();

        // Cold pass: collect freshness records (this is the cold-build cost, not
        // what we're optimizing — just needed to seed the warm comparison).
        let records: Vec<(std::path::PathBuf, FileFreshness)> = files
            .iter()
            .filter_map(|p| collect(p).ok().map(|f| (p.clone(), f)))
            .collect();

        eprintln!(
            "\n=== freshness stat-vs-hash benchmark ===\nrepo: {}\nfiles walked: {}  freshness records: {}",
            root.display(),
            files.len(),
            records.len()
        );

        // 3 iterations each, report medians. Interleave to share cache effects.
        let mut stat_ms = Vec::new();
        let mut hash_ms = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            let mut stat_hot = 0usize;
            for (path, cached) in &records {
                // Non-strict: stat only; hashes ONLY if (mtime,size) differ.
                // With no file changed, this is pure stat — the Option B cost.
                if matches!(verify_file(path, cached), FreshnessVerdict::HotFresh) {
                    stat_hot += 1;
                }
            }
            stat_ms.push(t.elapsed().as_micros());

            let t = Instant::now();
            let mut hash_hot = 0usize;
            for (path, cached) in &records {
                // Strict: stat + content-hash every file (today's per-edit cost).
                if matches!(verify_file_strict(path, cached), FreshnessVerdict::HotFresh) {
                    hash_hot += 1;
                }
            }
            hash_ms.push(t.elapsed().as_micros());

            eprintln!("  iter: stat_hot={stat_hot} hash_hot={hash_hot}");
        }
        stat_ms.sort_unstable();
        hash_ms.sort_unstable();
        let stat_med = stat_ms[1] as f64 / 1000.0;
        let hash_med = hash_ms[1] as f64 / 1000.0;
        eprintln!(
            "SUMMARY  files={}  stat_all_median={:.2}ms  hash_all_median={:.2}ms  speedup={:.1}x",
            records.len(),
            stat_med,
            hash_med,
            hash_med / stat_med.max(0.001)
        );
    }

    #[test]
    fn hot_fresh_when_mtime_size_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::HotFresh);
    }

    #[test]
    fn strict_hashes_small_file_when_metadata_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let original_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        write(&path, b"alpha");
        filetime::set_file_mtime(&path, original_mtime).unwrap();
        let fresh = collect(&path).unwrap();

        assert_eq!(
            verify_file_strict(&path, &fresh),
            FreshnessVerdict::HotFresh
        );

        write(&path, b"bravo");
        filetime::set_file_mtime(&path, original_mtime).unwrap();

        // Stat-diff freshness intentionally treats same-size, same-mtime edits as
        // fresh; Tier-2's staleness ceiling heals this accepted residual with a
        // periodic strict pass instead of hashing every file on each edit.
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::HotFresh);
        assert_eq!(verify_file_strict(&path, &fresh), FreshnessVerdict::Stale);
    }

    #[test]
    fn strict_stale_when_large_file_hash_was_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let original_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        let file = fs::File::create(&path).unwrap();
        file.set_len(CONTENT_HASH_SIZE_CAP + 1).unwrap();
        filetime::set_file_mtime(&path, original_mtime).unwrap();
        let fresh = collect(&path).unwrap();

        assert_eq!(fresh.size, CONTENT_HASH_SIZE_CAP + 1);
        assert_eq!(fresh.content_hash, zero_hash());
        // Non-strict stat-diff trusts unchanged metadata for over-cap files and
        // avoids strict's needless rescan of large unchanged files.
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::HotFresh);
        assert_eq!(verify_file_strict(&path, &fresh), FreshnessVerdict::Stale);
    }

    #[test]
    fn content_fresh_when_only_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"").unwrap();
        aft_store::sync::sync_all(&file, &path).unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        assert!(matches!(
            verify_file(&path, &fresh),
            FreshnessVerdict::ContentFresh { .. }
        ));
    }

    #[test]
    fn stale_when_size_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        write(&path, b"different");
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::Stale);
    }

    #[test]
    fn deleted_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        write(&path, b"same");
        let fresh = collect(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(verify_file(&path, &fresh), FreshnessVerdict::Deleted);
    }

    #[test]
    fn over_cap_hash_is_not_computed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        fs::write(&path, vec![0u8; CONTENT_HASH_SIZE_CAP as usize + 1]).unwrap();
        assert!(hash_file_if_small(&path, CONTENT_HASH_SIZE_CAP + 1)
            .unwrap()
            .is_none());
    }

    #[test]
    fn audit_growth_repeated_bulk_verification_builds_only_one_pool() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for index in 0..2 {
            let path = dir.path().join(format!("pool-{index}.rs"));
            write(&path, b"fn pool() {}\n");
            files.push((index, path.clone(), collect(&path).unwrap()));
        }
        let before = STRICT_VERIFY_POOL_BUILDS.load(Ordering::Relaxed);
        for _ in 0..2 {
            let results = verify_files_bounded(files.clone(), VerifyStrategy::StatFirst);
            assert_eq!(results.len(), 2);
        }
        let builds = STRICT_VERIFY_POOL_BUILDS.load(Ordering::Relaxed) - before;
        assert_eq!(builds, 1, "two calls share one Rayon pool construction");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn audit_growth_debug_strict_verify_path_history_is_bounded() {
        const MAX_PATH_HISTORY: usize = 4096;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug-history.rs");
        write(&path, b"");
        let freshness = collect(&path).unwrap();
        for _ in 0..(MAX_PATH_HISTORY + 8) {
            let _ = verify_file_strict(&path, &freshness);
        }
        assert!(
            verify_file_strict_count_under_for_debug(dir.path()) <= MAX_PATH_HISTORY,
            "debug path history exceeded {MAX_PATH_HISTORY} entries"
        );
    }
}

#[cfg(test)]
mod warm_reload_tests {
    use super::*;

    #[test]
    fn stat_first_hashes_only_files_with_changed_metadata_while_cold_verify_is_strict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("warm.rs");
        fs::write(&path, b"fn warm() {}\n").unwrap();
        let original_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(&path, original_mtime).unwrap();
        let freshness = collect(&path).unwrap();

        watch_hash_file_for_debug(&path);
        let unchanged = verify_files_bounded(
            vec![((), path.clone(), freshness)],
            VerifyStrategy::StatFirst,
        );
        assert_eq!(unchanged[0].2, FreshnessVerdict::HotFresh);
        assert_eq!(watched_hash_file_count_for_debug(), 0);

        watch_hash_file_for_debug(&path);
        let cold = verify_files_strict_bounded(vec![((), path.clone(), freshness)]);
        assert_eq!(cold[0].2, FreshnessVerdict::HotFresh);
        assert_eq!(watched_hash_file_count_for_debug(), 1);

        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        watch_hash_file_for_debug(&path);
        let changed_stat = verify_files_bounded(
            vec![((), path.clone(), freshness)],
            VerifyStrategy::StatFirst,
        );
        assert!(matches!(
            changed_stat[0].2,
            FreshnessVerdict::ContentFresh { .. }
        ));
        assert_eq!(watched_hash_file_count_for_debug(), 1);
    }

    #[test]
    fn verify_memo_skips_hits_and_invalidates_on_watcher_or_generation_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let artifact = dir.path().join("cache.bin");
        fs::create_dir(&root).unwrap();
        fs::write(&artifact, b"generation-one").unwrap();
        let generation_one = artifact_generation(&artifact).unwrap();

        assert_eq!(
            warm_verify_plan(&root, VerifyArtifact::Search, Some(generation_one)),
            WarmVerifyPlan::Strict
        );
        record_verify_completed(&root, VerifyArtifact::Search, Some(generation_one));
        assert_eq!(
            warm_verify_plan(&root, VerifyArtifact::Search, Some(generation_one)),
            WarmVerifyPlan::Skip
        );

        invalidate_verify_memo(&root);
        assert_eq!(
            warm_verify_plan(&root, VerifyArtifact::Search, Some(generation_one)),
            WarmVerifyPlan::StatFirst
        );

        fs::write(&artifact, b"generation-two-is-different").unwrap();
        let generation_two = artifact_generation(&artifact).unwrap();
        assert_ne!(generation_one, generation_two);
        assert_eq!(
            warm_verify_plan(&root, VerifyArtifact::Search, Some(generation_two)),
            WarmVerifyPlan::Strict
        );
    }

    #[test]
    fn invalidation_between_verify_and_record_prevents_false_fresh_memo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let artifact = dir.path().join("cache.bin");
        fs::create_dir(&root).unwrap();
        fs::write(&artifact, b"stable-generation").unwrap();
        let generation = artifact_generation(&artifact).unwrap();

        let stale_ticket = capture_verify_ticket(&root);
        invalidate_verify_memo(&root);
        assert!(!record_verify_completed_if_unchanged(
            &root,
            VerifyArtifact::Search,
            Some(generation),
            stale_ticket,
        ));
        assert_eq!(
            warm_verify_plan(&root, VerifyArtifact::Search, Some(generation)),
            WarmVerifyPlan::Strict
        );

        let current_ticket = capture_verify_ticket(&root);
        assert!(record_verify_completed_if_unchanged(
            &root,
            VerifyArtifact::Search,
            Some(generation),
            current_ticket,
        ));
        assert_eq!(
            warm_verify_plan(&root, VerifyArtifact::Search, Some(generation)),
            WarmVerifyPlan::Skip
        );
    }

    #[cfg(unix)]
    fn process_cpu_time() -> Duration {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        assert_eq!(result, 0, "getrusage should report process CPU time");
        let usage = unsafe { usage.assume_init() };
        let seconds = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64;
        let micros = (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as u64;
        Duration::from_secs(seconds) + Duration::from_micros(micros)
    }

    /// Measures the configure -> eviction -> rebind freshness component against
    /// a real repository. The first pass models a cold strict verification; the
    /// second models the unchanged-generation memo hit after in-process eviction.
    #[cfg(unix)]
    #[test]
    #[ignore = "manual CPU measurement; needs AFT_BENCH_REPO"]
    fn configure_eviction_rebind_cpu_measurement() {
        let root = PathBuf::from(
            std::env::var("AFT_BENCH_REPO").expect("AFT_BENCH_REPO must name a repository"),
        );
        let records = crate::callgraph::walk_project_files(&root)
            .filter_map(|path| collect(&path).ok().map(|freshness| (path, freshness)))
            .collect::<Vec<_>>();
        let artifact_dir = tempfile::tempdir().unwrap();
        let artifact = artifact_dir.path().join("cache.bin");
        fs::write(&artifact, b"stable-generation").unwrap();
        let generation = artifact_generation(&artifact).unwrap();

        let strict_inputs = records
            .iter()
            .enumerate()
            .map(|(index, (path, freshness))| (index, path.clone(), *freshness))
            .collect();
        let before = process_cpu_time();
        let _ = verify_files_strict_bounded(strict_inputs);
        let strict_cpu = process_cpu_time().saturating_sub(before);
        record_verify_completed(&root, VerifyArtifact::Search, Some(generation));

        let before = process_cpu_time();
        if warm_verify_plan(&root, VerifyArtifact::Search, Some(generation)) != WarmVerifyPlan::Skip
        {
            panic!("unchanged artifact generation should skip rebind verification");
        }
        let memo_hit_cpu = process_cpu_time().saturating_sub(before);

        invalidate_verify_memo(&root);
        let stat_inputs = records
            .iter()
            .enumerate()
            .map(|(index, (path, freshness))| (index, path.clone(), *freshness))
            .collect();
        let before = process_cpu_time();
        let _ = verify_files_bounded(stat_inputs, VerifyStrategy::StatFirst);
        let stat_first_cpu = process_cpu_time().saturating_sub(before);

        eprintln!(
            "configure-eviction-rebind CPU: files={} cold_strict={:?} memo_rebind={:?} invalidated_stat_first={:?}",
            records.len(), strict_cpu, memo_hit_cpu, stat_first_cpu
        );
    }
}
