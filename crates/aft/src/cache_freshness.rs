pub use aft_base::cache_freshness::*;

#[cfg(test)]
#[allow(unused_imports)]
use rayon::prelude::*;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(debug_assertions)]
use std::cell::Cell;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeMap;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(debug_assertions)]
use std::collections::VecDeque;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(any(debug_assertions, test))]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
#[allow(unused_imports)]
#[cfg(any(debug_assertions, test))]
use std::sync::atomic::Ordering;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::OnceLock;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Instant;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::SystemTime;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::UNIX_EPOCH;

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unused_imports)]
    pub(crate) use aft_base::cache_freshness::tests::*;

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
}

#[cfg(test)]
mod warm_reload_tests {
    use super::*;
    #[allow(unused_imports)]
    pub(crate) use aft_base::cache_freshness::warm_reload_tests::*;

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
