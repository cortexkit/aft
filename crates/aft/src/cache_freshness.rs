use rayon::prelude::*;
#[cfg(debug_assertions)]
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CONTENT_HASH_SIZE_CAP: u64 = 4 * 1024 * 1024;

#[cfg(debug_assertions)]
static STRICT_VERIFY_FILE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(debug_assertions)]
thread_local! {
    static HASH_FILE_IF_SMALL_CALLS: Cell<usize> = const { Cell::new(0) };
}
#[cfg(any(debug_assertions, test))]
static WATCHED_HASH_FILE: OnceLock<Mutex<Option<(PathBuf, usize)>>> = OnceLock::new();

const VERIFY_MEMO_TTL: Duration = Duration::from_secs(10 * 60);
static VERIFY_MEMO: OnceLock<Mutex<VerifyMemo>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum VerifyArtifact {
    Search,
    Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WarmVerifyPlan {
    Skip,
    StatFirst,
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifyStrategy {
    StatFirst,
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtifactGeneration {
    size: u64,
    modified_nanos: Option<u128>,
}

#[derive(Debug, Clone)]
struct VerifyMemoEntry {
    verify_completed_at: Instant,
    generation: ArtifactGeneration,
    invalidated: bool,
}

#[derive(Default)]
struct VerifyMemo {
    entries: BTreeMap<(PathBuf, VerifyArtifact), VerifyMemoEntry>,
    invalidation_tickets: BTreeMap<PathBuf, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFreshness {
    pub mtime: SystemTime,
    pub size: u64,
    pub content_hash: blake3::Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessVerdict {
    HotFresh,
    ContentFresh {
        new_mtime: SystemTime,
        new_size: u64,
    },
    Stale,
    Deleted,
}

pub fn hash_bytes(bytes: &[u8]) -> blake3::Hash {
    blake3::hash(bytes)
}

pub fn hash_file_if_small(path: &Path, size: u64) -> std::io::Result<Option<blake3::Hash>> {
    if size > CONTENT_HASH_SIZE_CAP {
        return Ok(None);
    }
    #[cfg(debug_assertions)]
    HASH_FILE_IF_SMALL_CALLS.with(|calls| calls.set(calls.get() + 1));
    #[cfg(any(debug_assertions, test))]
    {
        let mut watched = WATCHED_HASH_FILE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((watched_path, count)) = watched.as_mut() {
            if watched_path == path {
                *count += 1;
            }
        }
    }
    fs::read(path).map(|bytes| Some(hash_bytes(&bytes)))
}

pub fn metadata_matches(path: &Path, cached: &FileFreshness) -> std::io::Result<bool> {
    let metadata = fs::metadata(path)?;
    let new_size = metadata.len();
    let new_mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    Ok(new_size == cached.size && new_mtime == cached.mtime)
}

pub fn zero_hash() -> blake3::Hash {
    blake3::Hash::from_bytes([0u8; 32])
}

pub(crate) fn artifact_generation(path: &Path) -> Option<ArtifactGeneration> {
    let metadata = fs::metadata(path).ok()?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Some(ArtifactGeneration {
        size: metadata.len(),
        modified_nanos,
    })
}

fn verify_memo() -> &'static Mutex<VerifyMemo> {
    VERIFY_MEMO.get_or_init(|| Mutex::new(VerifyMemo::default()))
}

pub(crate) fn warm_verify_plan(
    root: &Path,
    artifact: VerifyArtifact,
    generation: Option<ArtifactGeneration>,
) -> WarmVerifyPlan {
    let Some(generation) = generation else {
        return WarmVerifyPlan::Strict;
    };
    let memo = verify_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = memo.entries.get(&(root.to_path_buf(), artifact)) else {
        return WarmVerifyPlan::Strict;
    };
    if entry.generation != generation {
        return WarmVerifyPlan::Strict;
    }
    if entry.invalidated || entry.verify_completed_at.elapsed() >= VERIFY_MEMO_TTL {
        WarmVerifyPlan::StatFirst
    } else {
        WarmVerifyPlan::Skip
    }
}

pub(crate) fn capture_verify_ticket(root: &Path) -> u64 {
    verify_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .invalidation_tickets
        .get(root)
        .copied()
        .unwrap_or(0)
}

pub(crate) fn record_verify_completed_if_unchanged(
    root: &Path,
    artifact: VerifyArtifact,
    generation: Option<ArtifactGeneration>,
    ticket: u64,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    let mut memo = verify_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current_ticket = memo.invalidation_tickets.get(root).copied().unwrap_or(0);
    if current_ticket != ticket {
        return false;
    }
    memo.entries.insert(
        (root.to_path_buf(), artifact),
        VerifyMemoEntry {
            verify_completed_at: Instant::now(),
            generation,
            invalidated: false,
        },
    );
    true
}

/// Seed the search-index verification memo for integration lifecycle tests.
#[doc(hidden)]
pub fn seed_search_verify_memo_for_test(root: &Path, artifact_path: &Path) -> bool {
    let ticket = capture_verify_ticket(root);
    record_verify_completed_if_unchanged(
        root,
        VerifyArtifact::Search,
        artifact_generation(artifact_path),
        ticket,
    )
}

/// Return the current search warm-verify plan as a stable test label.
#[doc(hidden)]
pub fn search_warm_verify_plan_for_test(root: &Path, artifact_path: &Path) -> &'static str {
    match warm_verify_plan(
        root,
        VerifyArtifact::Search,
        artifact_generation(artifact_path),
    ) {
        WarmVerifyPlan::Skip => "skip",
        WarmVerifyPlan::StatFirst => "stat_first",
        WarmVerifyPlan::Strict => "strict",
    }
}

#[cfg(test)]
pub(crate) fn record_verify_completed(
    root: &Path,
    artifact: VerifyArtifact,
    generation: Option<ArtifactGeneration>,
) {
    let ticket = capture_verify_ticket(root);
    let _ = record_verify_completed_if_unchanged(root, artifact, generation, ticket);
}

pub(crate) fn invalidate_verify_memo(root: &Path) {
    let mut memo = verify_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ticket = memo
        .invalidation_tickets
        .entry(root.to_path_buf())
        .or_insert(0);
    *ticket = ticket.wrapping_add(1);
    for ((memo_root, _), entry) in &mut memo.entries {
        if memo_root == root {
            entry.invalidated = true;
        }
    }
}

/// Forget all completed verification for a root after an interval in which its
/// filesystem watcher was stopped. A normal watcher invalidation can use
/// stat-first verification, but an unobserved interval must hash content because
/// size and mtime may both have been preserved by an external writer.
pub(crate) fn invalidate_verify_memo_strict(root: &Path) {
    let mut memo = verify_memo()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ticket = memo
        .invalidation_tickets
        .entry(root.to_path_buf())
        .or_insert(0);
    *ticket = ticket.wrapping_add(1);
    memo.entries.retain(|(memo_root, _), _| memo_root != root);
}

pub fn collect(path: &Path) -> std::io::Result<FileFreshness> {
    let metadata = fs::metadata(path)?;
    let mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    let size = metadata.len();
    let content_hash = hash_file_if_small(path, size)?.unwrap_or_else(zero_hash);
    Ok(FileFreshness {
        mtime,
        size,
        content_hash,
    })
}

pub fn verify_file(path: &Path, cached: &FileFreshness) -> FreshnessVerdict {
    verify_file_inner(path, cached, false)
}

pub fn verify_file_strict(path: &Path, cached: &FileFreshness) -> FreshnessVerdict {
    #[cfg(debug_assertions)]
    {
        STRICT_VERIFY_FILE_CALLS.fetch_add(1, Ordering::Relaxed);
        strict_verify_paths_for_debug()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(path.to_path_buf());
    }
    verify_file_inner(path, cached, true)
}

/// Verify semantic cache file freshness in a private bounded Rayon pool.
///
/// Do not use the global pool here: load-time strict verification can hash every
/// indexed file, and the semantic load/build already runs beside the bridge's
/// single dispatch thread. Match the half-cores/cap-8 policy used by the search
/// and callgraph cold-build pools.
pub(crate) fn verify_files_strict_bounded<K: Send>(
    files: Vec<(K, PathBuf, FileFreshness)>,
) -> Vec<(K, PathBuf, FreshnessVerdict)> {
    verify_files_bounded(files, VerifyStrategy::Strict)
}

pub(crate) fn verify_files_bounded<K: Send>(
    files: Vec<(K, PathBuf, FileFreshness)>,
    strategy: VerifyStrategy,
) -> Vec<(K, PathBuf, FreshnessVerdict)> {
    fn verify_one<K>(
        (key, path, cached): (K, PathBuf, FileFreshness),
        strategy: VerifyStrategy,
    ) -> (K, PathBuf, FreshnessVerdict) {
        let verdict = match strategy {
            VerifyStrategy::StatFirst => verify_file(&path, &cached),
            VerifyStrategy::Strict => verify_file_strict(&path, &cached),
        };
        (key, path, verdict)
    }

    if files.len() <= 1 {
        return files
            .into_iter()
            .map(|file| verify_one(file, strategy))
            .collect();
    }

    match strict_verify_pool() {
        Some(pool) => pool.install(|| {
            files
                .into_par_iter()
                .map(|file| verify_one(file, strategy))
                .collect()
        }),
        None => files
            .into_iter()
            .map(|file| verify_one(file, strategy))
            .collect(),
    }
}

fn strict_verify_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(build_strict_verify_pool).as_ref()
}

fn build_strict_verify_pool() -> Option<rayon::ThreadPool> {
    #[cfg(test)]
    STRICT_VERIFY_POOL_BUILDS.fetch_add(1, Ordering::Relaxed);
    rayon::ThreadPoolBuilder::new()
        .num_threads(strict_verify_pool_size())
        .thread_name(|index| format!("aft-semantic-verify-{index}"))
        .build()
        .ok()
}

pub(crate) fn strict_verify_pool_size() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get())
        .unwrap_or(1)
        .div_ceil(2)
        .clamp(1, 8)
}

#[cfg(debug_assertions)]
fn strict_verify_paths_for_debug() -> &'static std::sync::Mutex<Vec<PathBuf>> {
    static STRICT_VERIFY_FILE_PATHS: OnceLock<std::sync::Mutex<Vec<PathBuf>>> = OnceLock::new();
    STRICT_VERIFY_FILE_PATHS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_verify_file_strict_count_for_debug() {
    STRICT_VERIFY_FILE_CALLS.store(0, Ordering::Relaxed);
    strict_verify_paths_for_debug()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn verify_file_strict_count_for_debug() -> usize {
    STRICT_VERIFY_FILE_CALLS.load(Ordering::Relaxed)
}

/// Strict-verification count attributed to one root. The bare process-global
/// count is meaningless under parallel tests (any concurrent semantic or
/// search load increments it); tests asserting "MY operation ran no strict
/// verification" must count only paths under their own fixture root.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn verify_file_strict_count_under_for_debug(root: &Path) -> usize {
    strict_verify_paths_for_debug()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|path| path.starts_with(root))
        .count()
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_hash_file_if_small_count_for_debug() {
    HASH_FILE_IF_SMALL_CALLS.with(|calls| calls.set(0));
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn hash_file_if_small_count_for_debug() -> usize {
    HASH_FILE_IF_SMALL_CALLS.with(Cell::get)
}

#[cfg(any(debug_assertions, test))]
#[doc(hidden)]
pub fn watch_hash_file_for_debug(path: &Path) {
    *WATCHED_HASH_FILE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path.to_path_buf(), 0));
}

#[cfg(any(debug_assertions, test))]
#[doc(hidden)]
pub fn watched_hash_file_count_for_debug() -> usize {
    WATCHED_HASH_FILE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or(0, |(_, count)| *count)
}

#[cfg(test)]
static WATCHED_VERIFY_STATS: OnceLock<Mutex<Option<VerifyStatWatch>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct VerifyStatWatch {
    root: PathBuf,
    pub(crate) calls: Vec<(PathBuf, std::thread::ThreadId)>,
}

#[cfg(test)]
pub(crate) fn watch_verify_stats(root: &Path) {
    *WATCHED_VERIFY_STATS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some(VerifyStatWatch {
        root: root.to_path_buf(),
        calls: Vec::new(),
    });
}

#[cfg(test)]
pub(crate) fn take_verify_stats() -> VerifyStatWatch {
    WATCHED_VERIFY_STATS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take()
        .unwrap()
}

#[cfg(test)]
pub(crate) fn record_metadata_call(path: &Path) {
    if let Some(watch) = WATCHED_VERIFY_STATS.get() {
        let mut watch = watch.lock().unwrap();
        if let Some(watch) = watch.as_mut().filter(|watch| path.starts_with(&watch.root)) {
            watch
                .calls
                .push((path.to_path_buf(), std::thread::current().id()));
        }
    }
}

fn verify_file_inner(
    path: &Path,
    cached: &FileFreshness,
    hash_matching_metadata: bool,
) -> FreshnessVerdict {
    #[cfg(test)]
    record_metadata_call(path);
    let Ok(metadata) = fs::metadata(path) else {
        return FreshnessVerdict::Deleted;
    };
    let new_size = metadata.len();
    let new_mtime = metadata.modified().unwrap_or(UNIX_EPOCH);
    if new_size == cached.size && new_mtime == cached.mtime {
        if hash_matching_metadata {
            if new_size > CONTENT_HASH_SIZE_CAP || cached.content_hash == zero_hash() {
                return FreshnessVerdict::Stale;
            }
            return match hash_file_if_small(path, new_size) {
                Ok(Some(hash)) if hash == cached.content_hash => FreshnessVerdict::HotFresh,
                _ => FreshnessVerdict::Stale,
            };
        }
        return FreshnessVerdict::HotFresh;
    }
    if new_size != cached.size || new_size > CONTENT_HASH_SIZE_CAP {
        return FreshnessVerdict::Stale;
    }
    match hash_file_if_small(path, new_size) {
        Ok(Some(hash)) if hash == cached.content_hash => FreshnessVerdict::ContentFresh {
            new_mtime,
            new_size,
        },
        _ => FreshnessVerdict::Stale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        file.sync_all().unwrap();
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
