use std::path::{Path, PathBuf};
use std::time::Duration;

// These openers borrow cache artifacts that may be owned by a different AFT
// session. They therefore only read and verify the opened snapshot; any repair,
// migration, deletion, or rebuild must be left to the session that owns writes
// for that project.
use crate::cache_freshness::{artifact_generation, ArtifactGeneration};
use crate::search_index::{
    artifact_cache_key_read_only, resolve_cache_dir_with_key, BorrowedIndexLoad,
    BorrowedIndexLoadStop, SearchIndex,
};
use crate::semantic_index::SemanticIndex;

#[derive(Clone, Debug)]
pub(crate) enum ReadOnlyArtifact<T> {
    Fresh(T),
    Stale(ReadOnlyStale<T>),
    Degraded(ReadOnlyDegradation),
    Cancelled,
    Absent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadOnlyDegradation {
    pub reason: &'static str,
}

pub(crate) const BORROWED_SEARCH_LOAD_DEGRADATION: ReadOnlyDegradation = ReadOnlyDegradation {
    reason: "borrowed_search_index_load_budget",
};
/// The shared snapshot covers a different part of the checkout than the
/// borrowing root (for example it was written by a session opened in a
/// subfolder), so adopting it would hide files without saying so.
pub(crate) const BORROWED_SEARCH_COVERAGE_DEGRADATION: ReadOnlyDegradation = ReadOnlyDegradation {
    reason: "borrowed_search_index_coverage_mismatch",
};
pub(crate) const BORROWED_SEMANTIC_LOAD_DEGRADATION: ReadOnlyDegradation = ReadOnlyDegradation {
    reason: "borrowed_semantic_index_load_budget",
};
const BORROWED_SEMANTIC_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(test)]
#[derive(Default)]
pub(crate) struct BorrowedSearchLoadProbe {
    pub(crate) starts: std::sync::atomic::AtomicUsize,
    pub(crate) held: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
pub(crate) static BORROWED_SEARCH_LOAD_PROBES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<BorrowedSearchLoadProbe>>>,
> = std::sync::LazyLock::new(Default::default);

#[cfg(test)]
thread_local! {
    static BORROWED_SEARCH_LOAD_LIMITS_FOR_TEST: std::cell::Cell<Option<(usize, Duration)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn borrowed_search_load_limits() -> (usize, Duration) {
    #[cfg(test)]
    if let Some(limits) = BORROWED_SEARCH_LOAD_LIMITS_FOR_TEST.with(std::cell::Cell::get) {
        return limits;
    }
    (
        BORROWED_SEARCH_BACKGROUND_MAX_RECORDS,
        crate::search_index::BORROWED_INDEX_LOAD_BUDGET,
    )
}

pub(crate) const BORROWED_SEARCH_BACKGROUND_MAX_RECORDS: usize = 1_000_000;

pub(crate) fn borrowed_search_background_limits() -> (usize, Duration) {
    #[cfg(test)]
    if let Some(limits) = BORROWED_SEARCH_LOAD_LIMITS_FOR_TEST.with(std::cell::Cell::get) {
        return limits;
    }
    (
        BORROWED_SEARCH_BACKGROUND_MAX_RECORDS,
        crate::search_index::BORROWED_INDEX_LOAD_BUDGET,
    )
}

#[cfg(test)]
pub(crate) fn with_borrowed_search_load_limits_for_test<R>(
    max_records: usize,
    duration: Duration,
    run: impl FnOnce() -> R,
) -> R {
    BORROWED_SEARCH_LOAD_LIMITS_FOR_TEST.with(|slot| {
        let previous = slot.replace(Some((max_records, duration)));
        let result = run();
        slot.set(previous);
        result
    })
}

#[derive(Clone, Debug)]
pub(crate) struct ReadOnlyStale<T> {
    pub index: T,
    pub drift_count: usize,
    pub ignore_rules_differ: bool,
}

impl<T> ReadOnlyArtifact<T> {
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> ReadOnlyArtifact<U> {
        match self {
            Self::Fresh(index) => ReadOnlyArtifact::Fresh(map(index)),
            Self::Stale(stale) => ReadOnlyArtifact::Stale(ReadOnlyStale {
                index: map(stale.index),
                drift_count: stale.drift_count,
                ignore_rules_differ: stale.ignore_rules_differ,
            }),
            Self::Degraded(degradation) => ReadOnlyArtifact::Degraded(degradation),
            Self::Cancelled => ReadOnlyArtifact::Cancelled,
            Self::Absent => ReadOnlyArtifact::Absent,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BorrowedArtifactGeneration {
    pub path: PathBuf,
    pub generation: ArtifactGeneration,
}

#[derive(Debug)]
pub(crate) enum GitRootResolutionError {
    PathNotFound(PathBuf),
    NotAGitRoot,
    ProtectedScanRoot,
    Other(String),
}

pub(crate) fn resolve_git_root_from_user_path(
    project_root: &Path,
    raw_path: &str,
    scan_root_is_protected: impl Fn(&Path) -> bool,
) -> Result<PathBuf, GitRootResolutionError> {
    let account_home = crate::commands::configure::resolve_account_home_dir();
    let environment_home = home_dir();
    resolve_git_root_from_user_path_with_homes(
        project_root,
        raw_path,
        account_home.as_deref(),
        environment_home.as_deref(),
        scan_root_is_protected,
    )
}

fn resolve_git_root_from_user_path_with_homes(
    project_root: &Path,
    raw_path: &str,
    account_home: Option<&Path>,
    environment_home: Option<&Path>,
    scan_root_is_protected: impl Fn(&Path) -> bool,
) -> Result<PathBuf, GitRootResolutionError> {
    let expanded = expand_tilde_with_home(raw_path, environment_home);
    let requested = if expanded.is_absolute() {
        expanded
    } else {
        project_root.join(expanded)
    };
    if !requested.exists() {
        return Err(GitRootResolutionError::PathNotFound(requested));
    }

    let existing = nearest_existing_parent(&requested)
        .ok_or_else(|| GitRootResolutionError::PathNotFound(requested.clone()))?;

    // `nearest_existing_parent` returns an `fs::canonicalize` result, which on
    // Windows is a verbatim (`\\?\C:\...`) path. Normalize before comparing the
    // requested directory with the configured root so aliases such as `.` and
    // symlinks remain local without requiring the workspace itself to be Git.
    let existing = crate::inspect::job::canonicalize_normalized(&existing);
    if existing == crate::inspect::job::canonicalize_normalized(project_root) {
        return Ok(project_root.to_path_buf());
    }

    let git_base = if existing.is_file() {
        existing.parent().unwrap_or(&existing).to_path_buf()
    } else {
        existing
    };
    match git_toplevel(&git_base) {
        Ok(root) => Ok(root),
        Err(error) if error == "not_a_git_root" => {
            let account_home = account_home.or(environment_home);
            // A scan must use the same narrow-root policy as a watcher, even
            // though it never starts a watcher or builds an index. Exact-file
            // authorization must not widen into a protected containing folder.
            if scan_root_is_protected(&git_base) {
                return Err(GitRootResolutionError::ProtectedScanRoot);
            }
            if (!crate::developer_tools::git_usable()
                && crate::git_probe::repository_marker_or_uncertainty_in_directory(&git_base))
                || plain_scan_root_is_too_broad(&git_base, account_home, environment_home)
            {
                return Err(GitRootResolutionError::NotAGitRoot);
            }
            Ok(git_base)
        }
        Err(error) => Err(GitRootResolutionError::Other(error)),
    }
}

fn plain_scan_root_is_too_broad(
    root: &Path,
    account_home: Option<&Path>,
    environment_home: Option<&Path>,
) -> bool {
    // HOME can differ from the OS account home, so a scan root must be narrow
    // relative to both homes when both are available.
    match (account_home, environment_home) {
        (None, None) => crate::commands::configure::project_root_is_too_broad(root, false, None),
        (account_home, environment_home) => [account_home, environment_home]
            .into_iter()
            .flatten()
            .any(|home| {
                crate::commands::configure::project_root_is_too_broad(root, false, Some(home))
            }),
    }
}

pub(crate) fn search_index_artifact_generation(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> Option<BorrowedArtifactGeneration> {
    let cache_dir = search_index_cache_dir(project_root, storage_dir)?;
    search_artifact_generation_from_cache_dir(cache_dir)
}

pub(crate) fn search_index_artifact_generation_with_key(
    project_key: &str,
    storage_dir: Option<&Path>,
) -> Option<BorrowedArtifactGeneration> {
    search_artifact_generation_from_cache_dir(resolve_cache_dir_with_key(project_key, storage_dir))
}

fn search_artifact_generation_from_cache_dir(
    cache_dir: PathBuf,
) -> Option<BorrowedArtifactGeneration> {
    let path = cache_dir.join("cache.bin");
    let generation = artifact_generation(&path)?;
    Some(BorrowedArtifactGeneration { path, generation })
}

#[cfg(test)]
pub(crate) fn open_search_index_read_only(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> ReadOnlyArtifact<SearchIndex> {
    let Some(cache_dir) = search_index_cache_dir(project_root, storage_dir) else {
        return ReadOnlyArtifact::Absent;
    };
    open_search_index_from_cache_dir(project_root, cache_dir)
}

#[cfg(test)]
fn open_search_index_from_cache_dir(
    project_root: &Path,
    cache_dir: PathBuf,
) -> ReadOnlyArtifact<SearchIndex> {
    let (max_records, duration) = borrowed_search_load_limits();
    open_search_index_from_cache_dir_with_budget(project_root, cache_dir, max_records, duration)
}

static ACTIVE_BACKGROUND_SEARCH_LOADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

struct BackgroundSearchPermit;
impl Drop for BackgroundSearchPermit {
    fn drop(&mut self) {
        ACTIVE_BACKGROUND_SEARCH_LOADS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[allow(deprecated)] // fetch_update became try_update in Rust 1.99; the MSRV (1.92) lacks try_update
pub(crate) fn open_search_index_background_with_limit(
    project_root: &Path,
    cache_dir: PathBuf,
    max_records: usize,
    keep_going: &dyn Fn() -> bool,
) -> ReadOnlyArtifact<SearchIndex> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        if !keep_going() {
            return ReadOnlyArtifact::Cancelled;
        }
        if std::time::Instant::now() >= deadline {
            return ReadOnlyArtifact::Degraded(BORROWED_SEARCH_LOAD_DEGRADATION);
        }
        if ACTIVE_BACKGROUND_SEARCH_LOADS
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |active| (active < 2).then_some(active + 1),
            )
            .is_ok()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _permit = BackgroundSearchPermit;
    open_search_index_cancellable(
        project_root,
        cache_dir,
        max_records,
        deadline.saturating_duration_since(std::time::Instant::now()),
        keep_going,
    )
}

pub(crate) fn open_search_index_background(
    project_root: &Path,
    cache_dir: PathBuf,
    keep_going: &dyn Fn() -> bool,
) -> ReadOnlyArtifact<SearchIndex> {
    // Background loading is not an interactive request. Bound its record count
    // separately: large valid snapshots can exceed the interactive parse cap.
    open_search_index_background_with_limit(
        project_root,
        cache_dir,
        BORROWED_SEARCH_BACKGROUND_MAX_RECORDS,
        keep_going,
    )
}

#[cfg(test)]
fn open_search_index_from_cache_dir_with_budget(
    project_root: &Path,
    cache_dir: PathBuf,
    max_records: usize,
    duration: Duration,
) -> ReadOnlyArtifact<SearchIndex> {
    open_search_index_cancellable(project_root, cache_dir, max_records, duration, &|| true)
}

/// Parse another session's saved search index for `project_root`.
///
/// `Fresh` here means the snapshot parsed, matches this root's coverage and
/// ignore rules; it does not mean its records match the files on disk, which
/// this opener never reads. The result is cached across requests, so the disk
/// can change after it loads. Callers that answer queries from it must compare
/// it with the disk themselves: the external `aft_search` path does so on every
/// query (`semantic_search::external_disk_check`), and a read-only session
/// reconciles its own borrowed snapshot before publishing it
/// (`SearchIndex::reconcile_borrowed_snapshot_with_disk`).
pub(crate) fn open_search_index_cancellable(
    project_root: &Path,
    cache_dir: PathBuf,
    max_records: usize,
    duration: Duration,
    keep_going: &dyn Fn() -> bool,
) -> ReadOnlyArtifact<SearchIndex> {
    if crate::root_cache::check_cache_access(&cache_dir).is_err() {
        return ReadOnlyArtifact::Degraded(ReadOnlyDegradation {
            reason: crate::root_cache::CACHE_DIR_UNAVAILABLE,
        });
    }
    #[cfg(test)]
    let probe = BORROWED_SEARCH_LOAD_PROBES
        .lock()
        .unwrap()
        .get(project_root)
        .cloned();
    #[cfg(test)]
    if let Some(probe) = probe {
        probe
            .starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while probe.held.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    if !cache_dir.join("cache.bin").is_file() {
        return ReadOnlyArtifact::Absent;
    }

    let (mut index, ignore_rules_differ) =
        match SearchIndex::read_from_disk_borrow_tolerant_cancellable(
            &cache_dir,
            project_root,
            max_records,
            duration,
            keep_going,
        ) {
            BorrowedIndexLoad::Loaded(index, ignore_rules_differ) => (index, ignore_rules_differ),
            BorrowedIndexLoad::Stopped(BorrowedIndexLoadStop::BudgetExceeded) => {
                return ReadOnlyArtifact::Degraded(BORROWED_SEARCH_LOAD_DEGRADATION);
            }
            BorrowedIndexLoad::Stopped(BorrowedIndexLoadStop::CoverageMismatch) => {
                return ReadOnlyArtifact::Degraded(BORROWED_SEARCH_COVERAGE_DEGRADATION);
            }
            BorrowedIndexLoad::Stopped(BorrowedIndexLoadStop::Cancelled) => {
                return ReadOnlyArtifact::Cancelled;
            }
            BorrowedIndexLoad::Invalid => return ReadOnlyArtifact::Absent,
        };

    index.set_ready(true);
    if ignore_rules_differ {
        ReadOnlyArtifact::Stale(ReadOnlyStale {
            index,
            drift_count: 0,
            ignore_rules_differ,
        })
    } else {
        ReadOnlyArtifact::Fresh(index)
    }
}

fn search_index_cache_dir(project_root: &Path, storage_dir: Option<&Path>) -> Option<PathBuf> {
    match storage_dir {
        Some(storage_dir) => match artifact_cache_key_read_only(project_root, storage_dir) {
            Ok(project_key) => Some(resolve_cache_dir_with_key(&project_key, Some(storage_dir))),
            Err(error) => {
                crate::slog_warn!("read-only search index unavailable: {}", error);
                None
            }
        },
        None => search_index_cache_dir(
            project_root,
            Some(&crate::bash_background::storage_dir(None)),
        ),
    }
}

#[cfg(test)]
pub(crate) fn semantic_index_artifact_generation(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> Option<BorrowedArtifactGeneration> {
    let (data_path, _) = semantic_index_location(project_root, storage_dir)?;
    borrowed_artifact_generation(data_path)
}

pub(crate) fn semantic_index_artifact_generation_with_key(
    project_key: &str,
    storage_dir: Option<&Path>,
) -> Option<BorrowedArtifactGeneration> {
    let storage_dir = storage_dir?;
    borrowed_artifact_generation(
        storage_dir
            .join("semantic")
            .join(project_key)
            .join("semantic.bin"),
    )
}

fn borrowed_artifact_generation(path: PathBuf) -> Option<BorrowedArtifactGeneration> {
    let generation = artifact_generation(&path)?;
    Some(BorrowedArtifactGeneration { path, generation })
}

#[cfg(test)]
pub(crate) fn open_semantic_index_read_only(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> ReadOnlyArtifact<SemanticIndex> {
    let Some((data_path, project_key)) = semantic_index_location(project_root, storage_dir) else {
        return ReadOnlyArtifact::Absent;
    };
    open_semantic_index_from_location(project_root, storage_dir, data_path, &project_key)
}

pub(crate) fn open_semantic_index_read_only_with_key(
    project_root: &Path,
    storage_dir: Option<&Path>,
    project_key: &str,
) -> ReadOnlyArtifact<SemanticIndex> {
    let Some(storage_dir) = storage_dir else {
        return ReadOnlyArtifact::Absent;
    };
    let data_path = storage_dir
        .join("semantic")
        .join(project_key)
        .join("semantic.bin");
    open_semantic_index_from_location(project_root, Some(storage_dir), data_path, project_key)
}

fn open_semantic_index_from_location(
    project_root: &Path,
    storage_dir: Option<&Path>,
    data_path: PathBuf,
    project_key: &str,
) -> ReadOnlyArtifact<SemanticIndex> {
    if crate::root_cache::check_cache_access(&data_path).is_err() {
        return ReadOnlyArtifact::Degraded(ReadOnlyDegradation {
            reason: crate::root_cache::CACHE_DIR_UNAVAILABLE,
        });
    }
    let Some(storage_dir) = storage_dir else {
        return ReadOnlyArtifact::Absent;
    };
    let Ok(metadata) = data_path.metadata() else {
        return ReadOnlyArtifact::Absent;
    };
    if metadata.len() > BORROWED_SEMANTIC_MAX_BYTES {
        return ReadOnlyArtifact::Degraded(BORROWED_SEMANTIC_LOAD_DEGRADATION);
    }
    if search_cancelled() {
        return ReadOnlyArtifact::Cancelled;
    }

    let opened =
        SemanticIndex::read_from_disk_borrow_tolerant(storage_dir, project_key, project_root)
            .map(ReadOnlyArtifact::Fresh)
            .unwrap_or(ReadOnlyArtifact::Absent);
    if search_cancelled() {
        ReadOnlyArtifact::Cancelled
    } else {
        opened
    }
}

fn search_cancelled() -> bool {
    crate::executor::current_job_cancelled()
}

/// Test-only: production openers take the key configure resolved, because
/// this derivation falls back to a path-based key when the Git probe fails.
#[cfg(test)]
fn semantic_index_location(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> Option<(PathBuf, String)> {
    let storage_dir = storage_dir?;
    let project_key = match artifact_cache_key_read_only(project_root, storage_dir) {
        Ok(project_key) => project_key,
        Err(error) => {
            crate::slog_warn!("read-only semantic index unavailable: {}", error);
            return None;
        }
    };
    let data_path = storage_dir
        .join("semantic")
        .join(&project_key)
        .join("semantic.bin");
    Some((data_path, project_key))
}

pub(crate) fn expand_tilde(raw: &str) -> PathBuf {
    let home = home_dir();
    expand_tilde_with_home(raw, home.as_deref())
}

fn expand_tilde_with_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
}

fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return std::fs::canonicalize(&current).ok().or(Some(current));
        }
        if !current.pop() {
            return None;
        }
    }
}

fn git_toplevel(base_dir: &Path) -> Result<PathBuf, String> {
    let stdout = match crate::git_probe::toplevel_in_directory(base_dir) {
        Ok(stdout) => stdout,
        Err(crate::git_probe::ProbeError::Spawn(error))
            if error.kind() == std::io::ErrorKind::NotFound
                && !crate::git_probe::repository_marker_or_uncertainty_in_directory(base_dir) =>
        {
            return Err("not_a_git_root".to_string());
        }
        Err(crate::git_probe::ProbeError::Spawn(error)) => {
            return Err(format!("failed to run git: {error}"));
        }
        Err(crate::git_probe::ProbeError::Failed { kind, output }) => {
            if kind == crate::git_probe::FailureKind::NotRepository {
                return Err("not_a_git_root".to_string());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git rev-parse failed: {}", stderr.trim()));
        }
    };

    let toplevel = String::from_utf8_lossy(&stdout).trim().to_string();
    if toplevel.is_empty() {
        return Err("git rev-parse returned an empty toplevel".to_string());
    }
    let toplevel = PathBuf::from(toplevel);
    Ok(std::fs::canonicalize(&toplevel).unwrap_or(toplevel))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn unavailable_git_binds_new_roots_without_launching_the_installer() {
        const CHILD_DIR: &str = "AFT_STORAGE_GIT_UNAVAILABLE_ROOT";
        if let Some(base) = std::env::var_os(CHILD_DIR) {
            let base = PathBuf::from(base);
            assert!(!crate::developer_tools::git_usable());
            let result = resolve_git_root_from_user_path(
                &base.join("configured"),
                &base.join("requested").display().to_string(),
                |_| false,
            );
            let probe = crate::git_probe::toplevel(&base.join("requested"));
            let standing = crate::scoped_key::resolve_standing_root(
                &base.join("requested").display().to_string(),
            );
            let cache = crate::search_index::artifact_cache_key_with_memo(
                &base.join("requested"),
                &base.join("requested"),
                &base.join("storage"),
                None,
            );
            let outcomes = [
                matches!(result, Err(GitRootResolutionError::Other(_))),
                matches!(probe, Err(crate::git_probe::ProbeError::Spawn(_))),
                standing.is_ok_and(|root| {
                    root.artifact_key
                        == crate::search_index::artifact_path_identity_key(&base.join("requested"))
                }),
                cache.is_ok_and(|key| {
                    key == crate::search_index::artifact_path_identity_key(&base.join("requested"))
                }),
                !base.join("launched").exists(),
            ];
            assert_eq!(outcomes, [true; 5], "unavailable Git must preserve typed uncertainty, bind new roots by path and never launch the installer");
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let launcher = base.path().join("launcher");
        std::fs::create_dir_all(&launcher).unwrap();
        for name in ["configured", "requested"] {
            std::fs::create_dir(base.path().join(name)).unwrap();
        }
        std::fs::create_dir(base.path().join("requested/.git")).unwrap();
        let script = launcher.join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 1\n",
                base.path().join("launched").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "readonly_artifacts::tests::unavailable_git_binds_new_roots_without_launching_the_installer"])
            .env(CHILD_DIR, base.path()).env("PATH", &launcher).env("AFT_TEST_RAW_PATH", "1")
            .env("AFT_TEST_DEVELOPER_TOOLS", "absent").env("AFT_TEST_XCODE_LAUNCHER_DIRS", &launcher)
            .output().unwrap();
        assert!(
            output.status.success(),
            "unavailable-Git child: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!base.path().join("launched").exists());
    }

    #[test]
    fn borrow_only_checkout_opens_owner_artifacts_from_cache_root() {
        let (_repo, root) = fixture_project();
        let (_checkout, checkout) = linked_worktree(&root);
        let storage = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            storage_dir: Some(storage.path().to_path_buf()),
            cache_dir: Some(cache.path().to_path_buf()),
            project_root: Some(checkout.clone()),
            ..Default::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let artifact_root = crate::root_cache::cache_root(&config);
        let key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        build_search_artifact(&root, &artifact_root);
        build_semantic_artifact(&root, &artifact_root);
        crate::root_cache::configure_artifact_access(&checkout, &key, true);
        let claim = crate::artifact_owner::open_read_only_borrow(
            Some(storage.path()),
            &checkout,
            &key,
            &crate::path_identity::project_scope_key(&checkout),
        );
        assert!(claim.lease.is_none());
        assert_eq!(
            claim.status.mode,
            crate::artifact_owner::ArtifactOwnerMode::ReadOnly
        );
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        );
        assert!(matches!(
            ctx.open_borrowed_search_index(&checkout, ctx.config().cache_storage_dir().as_deref()),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert!(matches!(
            ctx.open_borrowed_semantic_index(
                &checkout,
                ctx.config().cache_storage_dir().as_deref()
            ),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert!(!storage.path().join("index").exists());
        assert!(!storage.path().join("semantic").exists());
    }
    use super::*;
    use crate::search_index::resolve_cache_dir;

    use std::collections::BTreeMap;
    use std::fs;
    use std::process::Command;
    use std::time::SystemTime;

    use tempfile::TempDir;

    use crate::context::BORROWED_INDEX_CACHE_CAPACITY;
    use crate::search_index::artifact_cache_key;
    use crate::semantic_index::{SemanticIndex, SemanticIndexFingerprint};

    /// With a relative PATH entry, `git` must still be found relative to the
    /// project folder, where this probe has always started its child.
    #[cfg(unix)]
    #[test]
    fn relative_path_entry_finds_git_from_the_project_folder() {
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::write_stub_git_for_test(&dir.path().join("bin"), "/stub-toplevel");
        let path = crate::git_probe::relative_bin_path_for_test();
        let toplevel = crate::git_probe::with_child_env_for_test(&[("PATH", &path)], || {
            git_toplevel(dir.path())
        })
        .unwrap();
        assert_eq!(toplevel, PathBuf::from("/stub-toplevel"));
    }

    /// A repository with a corrupt HEAD makes Git print its ordinary "not a git
    /// repository" message. That is damaged metadata, not proof of absence.
    #[test]
    fn corrupt_repository_metadata_is_a_git_failure_not_absence() {
        assert!(crate::developer_tools::git_usable());
        let dir = tempfile::tempdir().unwrap();
        crate::git_probe::init_repository_with_corrupt_head_for_test(dir.path());
        let error = git_toplevel(dir.path()).unwrap_err();
        assert!(error.starts_with("git rev-parse failed:"), "{error}");
    }

    #[test]
    fn plain_folder_resolves_to_its_own_scan_root() {
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let root = resolve_git_root_from_user_path(
            project.path(),
            folder.path().to_str().unwrap(),
            |_| false,
        )
        .unwrap();
        assert_eq!(
            root,
            crate::inspect::job::canonicalize_normalized(folder.path())
        );
    }

    #[test]
    fn plain_folder_scan_root_refuses_access_floor() {
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("artifact.stdout");
        std::fs::write(&file, "artifact").unwrap();
        let mut config = crate::config::Config::default();
        config.sandbox.read_deny = vec![folder.path().to_path_buf()];
        let policy = crate::access_policy::AccessPolicy::from_config(
            &config,
            &project.path().join("storage"),
            vec![project.path().to_path_buf()],
            project.path(),
        );
        for path in [folder.path(), file.as_path()] {
            let result =
                resolve_git_root_from_user_path(project.path(), &path.to_string_lossy(), |root| {
                    assert_eq!(
                        root,
                        crate::inspect::job::canonicalize_normalized(folder.path())
                    );
                    policy.decide(
                        crate::access_policy::AccessOperation::Read,
                        &crate::access_policy::policy_path(root),
                        false,
                    ) == crate::access_policy::AccessDecision::Deny(
                        crate::access_policy::AccessDenial::Floor,
                    )
                });
            assert!(
                matches!(result, Err(GitRootResolutionError::ProtectedScanRoot)),
                "{result:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn plain_folder_resolves_without_a_git_executable() {
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let root =
            crate::git_probe::with_child_env_for_test(&[("PATH", bin.path().as_os_str())], || {
                resolve_git_root_from_user_path(
                    project.path(),
                    folder.path().to_str().unwrap(),
                    |_| false,
                )
            })
            .unwrap();
        assert_eq!(
            root,
            crate::inspect::job::canonicalize_normalized(folder.path())
        );
    }

    #[cfg(unix)]
    #[test]
    fn repository_without_a_git_executable_keeps_resolution_failure() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let folder = tempfile::tempdir().unwrap();
        init_git(folder.path());
        let bin = tempfile::tempdir().unwrap();
        let error =
            crate::git_probe::with_child_env_for_test(&[("PATH", bin.path().as_os_str())], || {
                resolve_git_root_from_user_path(
                    project.path(),
                    folder.path().to_str().unwrap(),
                    |_| false,
                )
            })
            .unwrap_err();
        assert!(
            matches!(error, GitRootResolutionError::Other(ref message) if message.starts_with("failed to run git:")),
            "{error:?}"
        );
    }

    #[test]
    fn plain_folder_scan_roots_refuse_environment_and_account_homes() {
        let project = tempfile::tempdir().unwrap();
        let environment_home = tempfile::tempdir().unwrap();
        let account_home = tempfile::tempdir().unwrap();

        for (label, home) in [
            ("environment home", environment_home.path()),
            ("account home", account_home.path()),
        ] {
            for root in home.ancestors() {
                assert!(
                    matches!(
                        resolve_git_root_from_user_path_with_homes(
                            project.path(),
                            &root.to_string_lossy(),
                            Some(account_home.path()),
                            Some(environment_home.path()),
                            |_| false,
                        ),
                        Err(GitRootResolutionError::NotAGitRoot)
                    ),
                    "unsafe {label} scan root: {}",
                    root.display()
                );
            }
        }
    }

    #[test]
    fn real_account_home_is_refused_when_available() {
        // A host's account home may also be a Git checkout. Check the broad-root
        // policy directly so Git-root resolution cannot bypass this assertion.
        match crate::commands::configure::resolve_account_home_dir() {
            Some(home) => assert!(
                crate::commands::configure::project_root_is_too_broad(&home, false, Some(&home),),
                "account home must be treated as too broad: {}",
                home.display()
            ),
            None => eprintln!(
                "skipping real account-home assertion: the host could not provide an account home"
            ),
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct FileSnapshot {
        modified: SystemTime,
        hash: blake3::Hash,
    }

    fn git_command(root: &Path) -> Command {
        let mut command = Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(root));
        command
    }

    fn init_git(root: &Path) {
        let status = git_command(root)
            .args(["init"])
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        let status = git_command(root)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .expect("configure git email");
        assert!(status.success(), "git config email failed");
        let status = git_command(root)
            .args(["config", "user.name", "AFT Test"])
            .status()
            .expect("configure git name");
        assert!(status.success(), "git config name failed");
    }

    fn commit_all(root: &Path) {
        let status = git_command(root)
            .args(["add", "."])
            .status()
            .expect("git add");
        assert!(status.success(), "git add failed");
        let status = git_command(root)
            .args(["commit", "-m", "initial"])
            .status()
            .expect("git commit");
        assert!(status.success(), "git commit failed");
    }

    fn fixture_project() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("create project");
        init_git(temp.path());
        let source = temp.path().join("src/lib.rs");
        fs::create_dir_all(source.parent().expect("source parent")).expect("create src");
        fs::write(&source, "pub fn readonly_needle() -> bool { true }\n").expect("write source");
        commit_all(temp.path());
        let root = fs::canonicalize(temp.path()).expect("canonical project root");
        (temp, root)
    }

    fn snapshot_dir(root: &Path) -> BTreeMap<PathBuf, FileSnapshot> {
        fn visit(dir: &Path, out: &mut BTreeMap<PathBuf, FileSnapshot>, base: &Path) {
            for entry in fs::read_dir(dir).expect("read dir") {
                let entry = entry.expect("dir entry");
                let path = entry.path();
                let meta = entry.metadata().expect("entry metadata");
                if meta.is_dir() {
                    visit(&path, out, base);
                } else if meta.is_file() {
                    let bytes = fs::read(&path).expect("read snapshot file");
                    out.insert(
                        path.strip_prefix(base)
                            .expect("relative snapshot path")
                            .to_path_buf(),
                        FileSnapshot {
                            modified: meta.modified().expect("snapshot mtime"),
                            hash: blake3::hash(&bytes),
                        },
                    );
                }
            }
        }

        let mut out = BTreeMap::new();
        if root.exists() {
            visit(root, &mut out, root);
        }
        out
    }

    fn build_search_artifact(root: &Path, storage: &Path) -> PathBuf {
        let cache_dir = resolve_cache_dir(root, Some(storage));
        let mut index = SearchIndex::build(root);
        index.write_to_disk(
            &cache_dir,
            crate::search_index::current_git_head(root).as_deref(),
        );
        cache_dir
    }

    #[test]
    #[cfg(windows)]
    fn borrowed_search_keeps_plain_root_keys_for_disk_lookups() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, canonical_root) = fixture_project();
        let root = crate::windows_path::normalize_windows_path(&canonical_root);
        assert!(!root.to_string_lossy().starts_with(r"\\?\"));
        let storage = tempfile::tempdir().expect("storage");
        build_search_artifact(&canonical_root, storage.path());

        let index = match open_search_index_read_only(&root, Some(storage.path())) {
            ReadOnlyArtifact::Fresh(index) => index,
            other => panic!("expected a fresh borrowed artifact, got {other:?}"),
        };
        assert!(index.path_to_id.contains_key(&root.join("src/lib.rs")));
        assert_eq!(
            index
                .grep("readonly_needle", true, &[], &[], &root.join("src"), 10)
                .expect("grep over a test index")
                .matches
                .len(),
            1
        );
    }

    fn clone_checkout(root: &Path) -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("create clone dir");
        let clone_root = temp.path().join("clone");
        // fixture_project returns a canonicalized root, which on Windows is a
        // verbatim `\\?\` path. git cannot take verbatim paths as the clone
        // source, so strip the prefix for the git invocation only.
        let clone_source = root
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        let mut command = Command::new("git");
        let status = crate::test_env::apply_hermetic_git_env(&mut command)
            .arg("clone")
            .arg("--quiet")
            .arg(&clone_source)
            .arg(&clone_root)
            .status()
            .expect("git clone");
        assert!(status.success(), "git clone failed");
        let clone_root = fs::canonicalize(clone_root).expect("canonical clone root");
        (temp, clone_root)
    }

    fn build_semantic_artifact(root: &Path, storage: &Path) {
        build_semantic_artifact_with_key(root, storage, &artifact_cache_key(root));
    }

    fn build_semantic_artifact_with_key(root: &Path, storage: &Path, project_key: &str) {
        let source = root.join("src/lib.rs");
        let fingerprint = SemanticIndexFingerprint {
            backend: "openai_compatible".to_string(),
            model: "readonly-test".to_string(),
            base_url: "http://127.0.0.1".to_string(),
            dimension: 3,
            chunking_version: 1,
            ..Default::default()
        };
        let mut embed =
            |texts: Vec<String>| Ok::<_, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
        let mut index =
            SemanticIndex::build(root, &[source], &mut embed, 8).expect("build semantic index");
        index.set_fingerprint(fingerprint);
        index.write_to_disk(storage, project_key);
    }

    fn borrowed_context(root: &Path, storage: &Path) -> crate::context::AppContext {
        crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                ..crate::config::Config::default()
            },
        )
    }

    fn cached_search_index(
        ctx: &crate::context::AppContext,
        root: &Path,
        storage: &Path,
    ) -> std::sync::Arc<SearchIndex> {
        match ctx.open_borrowed_search_index(root, Some(storage)) {
            ReadOnlyArtifact::Fresh(index)
            | ReadOnlyArtifact::Stale(ReadOnlyStale { index, .. }) => index,
            other => panic!("expected borrowed search index, got {other:?}"),
        }
    }

    #[test]
    fn callgraph_cold_build_reopens_saved_path_identity_after_git_is_added() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(project.path()).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn writer_needle() {}\n").unwrap();
        let storage = tempfile::tempdir().unwrap();
        let owner = borrowed_context(&root, storage.path());
        let saved_key = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage.path(), None)
            .unwrap()
            .expect("settled path identity");
        crate::root_cache::configure_artifact_access(&root, &saved_key, false);

        init_git(&root);
        commit_all(&root);
        assert_ne!(artifact_cache_key(&root), saved_key);
        // A saved project identity must make writers independent of aft.db,
        // which held the older path-identity pin before identity files existed.
        fs::create_dir(storage.path().join("aft.db")).unwrap();
        let callgraph_dir = storage.path().join("callgraph").join(&saved_key);
        let (graph, _) = crate::callgraph_store::CallGraphStore::cold_build_with_lease(
            callgraph_dir.clone(),
            root.clone(),
            &[root.join("src/lib.rs")],
        )
        .unwrap();
        drop(graph);
        assert!(
            crate::callgraph_store::CallGraphStore::open_readonly(
                callgraph_dir.clone(),
                root.clone()
            )
            .unwrap()
            .is_some(),
            "a graph built after Git is added must reopen under the saved path identity"
        );
        use crate::callgraph_store::CallGraphStore;
        assert!(!CallGraphStore::needs_cold_build(&callgraph_dir, &root).unwrap());
        for open in [
            CallGraphStore::open_ready,
            CallGraphStore::open_ready_repairing,
            CallGraphStore::open_ready_no_rebuild,
            CallGraphStore::migrate_legacy_with_lease,
        ] {
            let graph = open(callgraph_dir.clone(), root.clone()).unwrap().unwrap();
            assert_eq!(graph.project_key(), saved_key);
            assert_eq!(graph.indexed_file_count().unwrap(), 1);
        }
        let (graph, rebuilt) = CallGraphStore::ensure_built_with_lease(
            callgraph_dir.clone(),
            root.clone(),
            &[root.join("src/lib.rs")],
        )
        .unwrap();
        assert!(
            rebuilt.is_none(),
            "the saved-key generation is already complete"
        );
        assert_eq!(graph.project_key(), saved_key);
        drop(graph);
        let graph = CallGraphStore::open(callgraph_dir, root.clone()).unwrap();
        assert_eq!(graph.project_key(), saved_key);
        assert_eq!(graph.indexed_file_count().unwrap(), 1);
    }

    #[test]
    fn callgraph_writer_and_reader_keep_the_full_standing_subtree_key() {
        let storage = tempfile::tempdir().unwrap();
        build_and_read_standing_subtree_callgraph(storage.path());
    }

    /// A standing subtree's callgraph files carry its 64-character key twice,
    /// in the directory and in each file name, so under a storage directory
    /// of ordinary depth their SQLite paths and sidecars pass Windows'
    /// 260-character `MAX_PATH`. Both the cold build (which writes a journal)
    /// and the read-only open of the published generation must still work.
    #[cfg(windows)]
    #[test]
    fn callgraph_of_a_standing_subtree_builds_and_reads_past_the_windows_path_limit() {
        let storage = tempfile::tempdir().unwrap();
        let deep = storage
            .path()
            .join("a-storage-directory-as-deep-as-a-real-profile")
            .join("another-level-of-ordinary-nesting");
        fs::create_dir_all(&deep).unwrap();
        // A generation file name is at least `<64-char key>.g<19-digit nanos>.<pid>.sqlite`.
        let generation_dir_len = deep
            .join("callgraph")
            .join("k".repeat(64))
            .as_os_str()
            .len();
        assert!(
            generation_dir_len + 1 + 94 >= 260,
            "the fixture must put the generation files past MAX_PATH"
        );
        build_and_read_standing_subtree_callgraph(&deep);
    }

    fn build_and_read_standing_subtree_callgraph(storage: &Path) {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (project, root) = fixture_project();
        let subtree = root.join("src");
        let resolved = crate::scoped_key::resolve_standing_root_in_storage(
            subtree.to_str().unwrap(),
            storage,
            true,
        )
        .unwrap();
        let key = resolved.artifact_key;
        assert_eq!(key.len(), 64);
        assert_ne!(
            crate::search_index::artifact_cache_key_read_only(&subtree, storage).unwrap(),
            key
        );
        crate::root_cache::configure_artifact_access(&subtree, &key, false);
        let dir = storage.join("callgraph").join(&key);
        let (graph, _) = crate::callgraph_store::CallGraphStore::cold_build_with_lease(
            dir.clone(),
            subtree.clone(),
            &[subtree.join("lib.rs")],
        )
        .unwrap();
        assert_eq!(graph.project_key(), key);
        drop(graph);
        let graph = crate::callgraph_store::CallGraphStore::open_readonly(dir, subtree)
            .unwrap()
            .unwrap();
        assert_eq!(graph.project_key(), key);
        assert_eq!(graph.indexed_file_count().unwrap(), 1);
        drop(project);
    }

    #[test]
    fn bound_artifact_builds_keep_saved_path_key_after_git_is_added() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(project.path()).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn readonly_needle() {}\n").unwrap();
        let storage = tempfile::tempdir().unwrap();
        let owner = borrowed_context(&root, storage.path());
        let saved_key = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage.path(), None)
            .unwrap()
            .unwrap();
        crate::root_cache::configure_artifact_access(&root, &saved_key, false);
        init_git(&root);
        commit_all(&root);
        assert_ne!(artifact_cache_key(&root), saved_key);
        fs::create_dir(storage.path().join("aft.db")).unwrap();
        assert_eq!(
            crate::search_index::artifact_cache_key_for_build(&root),
            saved_key
        );
        assert_eq!(owner.memoized_artifact_cache_key(&root), saved_key);

        let cache_dir =
            crate::search_index::resolve_cache_dir_with_key(&saved_key, Some(storage.path()));
        assert_eq!(resolve_cache_dir(&root, Some(storage.path())), cache_dir);
        let mut index = SearchIndex::build_with_limit_to_cache_dir(&root, 1_000_000, &cache_dir);
        let head = index.stored_git_head().map(str::to_owned);
        assert!(index.write_to_disk(&cache_dir, head.as_deref()));
        build_semantic_artifact_with_key(&root, storage.path(), &saved_key);
        assert!(matches!(
            open_search_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert!(matches!(
            open_semantic_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));

        let family = crate::migration::SemanticMigrationRequest::for_root(
            storage.path(),
            root.clone(),
            "test-model",
        )
        .family;
        assert_eq!(family, saved_key);
        assert!(matches!(
            crate::migration::rebuild_legacy_callgraph_once(storage.path(), &root, 0).unwrap(),
            crate::migration::CallgraphMigrationOutcome::Rebuilt
        ));
        assert!(crate::callgraph_store::CallGraphStore::open_readonly(
            storage.path().join("callgraph").join(&saved_key),
            root,
        )
        .unwrap()
        .is_some());
    }

    /// Bind a plain folder in storage A (path identity), then add Git and let
    /// a session in storage B save a Git identity for the same folder.
    /// Returns (project tempdir, root, path key in A, Git key in B, owner
    /// context bound in A).
    fn folder_with_path_identity_in_a_and_git_identity_in_b(
        owner_storage: &Path,
        other_storage: &Path,
    ) -> (TempDir, PathBuf, String, String, crate::context::AppContext) {
        let project = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(project.path()).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn switched_needle() {}\n").unwrap();
        let owner = borrowed_context(&root, owner_storage);
        let path_key = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, owner_storage, None)
            .unwrap()
            .expect("settled path identity in storage A");
        assert_eq!(
            path_key,
            crate::search_index::artifact_path_identity_key(&root)
        );

        init_git(&root);
        commit_all(&root);
        let other_session = borrowed_context(&root, other_storage);
        let git_key = other_session
            .memoized_artifact_cache_key_for_configure(&root, &root, other_storage, None)
            .unwrap()
            .expect("settled Git identity in storage B");
        assert_ne!(git_key, path_key);
        assert!(matches!(
            crate::root_identity::read(&root, other_storage).unwrap(),
            Some(crate::root_identity::Identity::Git { .. })
        ));
        (project, root, path_key, git_key, owner)
    }

    /// `storage.cache_dir` moves artifacts, not identity. The folder below has
    /// a saved path identity on its durable root and is a Git repository now,
    /// so a lookup in a storage root without that record would choose a Git
    /// key. Readers that name the cache root must resolve the key configure
    /// memoizes for the durable root, and the call-graph folder must sit on
    /// the cache root under that key.
    #[test]
    fn cache_root_readers_resolve_the_key_configure_memoizes_for_the_durable_root() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let storage = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let (_project, root, path_key, git_key, _owner) =
            folder_with_path_identity_in_a_and_git_identity_in_b(storage.path(), other.path());
        let mut config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            cache_dir: Some(cache.path().to_path_buf()),
            ..crate::config::Config::default()
        };
        assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
        let durable = crate::bash_background::storage_dir(config.storage_dir.as_deref());
        let artifacts = crate::root_cache::cache_root(&config);
        assert_ne!(artifacts, durable);
        let ctx = crate::context::AppContext::new(
            crate::context::default_language_provider_factory(),
            config,
        );

        let configured = ctx
            .memoized_artifact_cache_key_for_configure(&root, &root, &durable, None)
            .unwrap()
            .expect("settled path identity on the durable root");
        assert_eq!(configured, path_key);
        assert_ne!(configured, git_key);
        assert_eq!(
            artifact_cache_key_read_only(&root, &artifacts).unwrap(),
            configured,
            "a reader naming the cache root must use the durable root's identity"
        );
        let reader = borrowed_context(&root, storage.path());
        assert_eq!(
            reader
                .memoized_readonly_artifact_cache_key(&root, &artifacts)
                .unwrap(),
            configured
        );
        assert_eq!(
            ctx.callgraph_store_dir(),
            artifacts.join("callgraph").join(&configured)
        );
    }

    /// Without `storage.cache_dir` the artifact root is the durable root, so
    /// readers resolve keys and folders exactly where they did before the
    /// setting existed: the saved identity's key, under the storage root.
    #[test]
    fn without_cache_dir_reader_keys_and_folders_stay_on_the_durable_root() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let storage = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let (_project, root, path_key, _git_key, owner) =
            folder_with_path_identity_in_a_and_git_identity_in_b(storage.path(), other.path());
        let config = owner.config();
        assert!(config.cache_dir.is_none());
        let artifacts = config.cache_storage_dir().expect("configured storage");
        assert_eq!(artifacts, storage.path());
        assert_eq!(
            crate::root_cache::identity_storage_root(&artifacts),
            std::borrow::Cow::Borrowed(storage.path())
        );
        assert_eq!(
            path_key,
            crate::search_index::artifact_path_identity_key(&root)
        );
        assert_eq!(
            artifact_cache_key_read_only(&root, &artifacts).unwrap(),
            path_key
        );
        assert_eq!(
            owner
                .memoized_readonly_artifact_cache_key(&root, &artifacts)
                .unwrap(),
            path_key
        );
        assert_eq!(
            resolve_cache_dir_with_key(&path_key, Some(&artifacts)),
            storage.path().join("index").join(&path_key)
        );
        assert_eq!(
            owner.callgraph_store_dir(),
            storage.path().join("callgraph").join(&path_key)
        );
    }

    #[test]
    fn configure_key_follows_saved_identity_of_new_storage_root() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let storage_a = tempfile::tempdir().unwrap();
        let storage_b = tempfile::tempdir().unwrap();
        let (_project, root, path_key, git_key, owner) =
            folder_with_path_identity_in_a_and_git_identity_in_b(
                storage_a.path(),
                storage_b.path(),
            );

        // The owner's cache location moves to storage B. Configure must read
        // B's saved Git identity, not reuse the path key chosen in storage A.
        let configured = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage_b.path(), None)
            .unwrap()
            .expect("settled identity in storage B");
        assert_eq!(configured, git_key);
        assert_ne!(configured, path_key);
        assert_eq!(
            owner.cached_artifact_cache_key(&root),
            Some(git_key.clone())
        );

        // An index built under the configure key is what a borrowed reader of
        // storage B finds.
        let cache_dir =
            crate::search_index::resolve_cache_dir_with_key(&configured, Some(storage_b.path()));
        let mut index = SearchIndex::build_with_limit_to_cache_dir(&root, 1_000_000, &cache_dir);
        let head = index.stored_git_head().map(str::to_owned);
        assert!(index.write_to_disk(&cache_dir, head.as_deref()));
        assert_eq!(
            owner
                .memoized_readonly_artifact_cache_key(&root, storage_b.path())
                .unwrap(),
            configured
        );
        let borrower = borrowed_context(&root, storage_b.path());
        with_borrowed_search_load_limits_for_test(10_000, Duration::from_secs(5), || {
            for ctx in [&owner, &borrower] {
                let index = cached_search_index(ctx, &root, storage_b.path());
                assert_eq!(
                    index
                        .grep("switched_needle", true, &[], &[], &root, 10)
                        .expect("grep over a test index")
                        .matches
                        .len(),
                    1
                );
            }
        });
    }

    #[test]
    fn configure_key_is_byte_identical_for_an_unchanged_storage_root() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let storage_a = tempfile::tempdir().unwrap();
        let storage_b = tempfile::tempdir().unwrap();
        let (_project, root, path_key, git_key, owner) =
            folder_with_path_identity_in_a_and_git_identity_in_b(
                storage_a.path(),
                storage_b.path(),
            );

        // A repeat bind in the same storage root is a memo hit with the same
        // bytes, even though the folder is now a Git repository.
        let derivations = owner.artifact_cache_key_derivation_count_for_test();
        let again = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage_a.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(again.as_bytes(), path_key.as_bytes());
        assert_eq!(
            owner.artifact_cache_key_derivation_count_for_test(),
            derivations
        );

        // Visiting storage B and returning to A keeps each root's own key.
        let in_b = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage_b.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(in_b.as_bytes(), git_key.as_bytes());
        let back_in_a = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage_a.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(back_in_a.as_bytes(), path_key.as_bytes());
        // Artifact loads and the call-graph directory read the root-only
        // cache, so it must follow the storage root back to A as well.
        assert_eq!(
            owner.cached_artifact_cache_key(&root),
            Some(path_key.clone())
        );

        // A fresh context resolves the same bytes from storage A's record.
        let fresh = borrowed_context(&root, storage_a.path());
        let fresh_key = fresh
            .memoized_artifact_cache_key_for_configure(&root, &root, storage_a.path(), None)
            .unwrap()
            .unwrap();
        assert_eq!(fresh_key.as_bytes(), path_key.as_bytes());
    }

    #[test]
    fn borrowed_readers_share_saved_path_identity_after_git_is_added() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(project.path()).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn readonly_needle() {}\n").unwrap();
        let storage = tempfile::tempdir().unwrap();
        let owner = borrowed_context(&root, storage.path());
        let saved_key = owner
            .memoized_artifact_cache_key_for_configure(&root, &root, storage.path(), None)
            .unwrap()
            .expect("settled path identity");
        build_search_artifact(&root, storage.path());
        build_semantic_artifact(&root, storage.path());
        let callgraph_dir = storage.path().join("callgraph").join(&saved_key);
        crate::root_cache::configure_artifact_access(&root, &saved_key, false);
        let graph =
            crate::callgraph_store::CallGraphStore::open(callgraph_dir.clone(), root.clone())
                .unwrap();
        graph.cold_build(&[root.join("src/lib.rs")]).unwrap();
        drop(graph);

        init_git(&root);
        commit_all(&root);
        assert_ne!(artifact_cache_key(&root), saved_key);
        // A saved identity must be enough even when the older pin database
        // cannot be opened. Consulting it would refuse all these readers.
        fs::create_dir(storage.path().join("aft.db")).unwrap();
        let before = snapshot_dir(storage.path());
        let session = tempfile::tempdir().unwrap();
        let borrower = borrowed_context(session.path(), storage.path());
        with_borrowed_search_load_limits_for_test(10_000, Duration::from_secs(5), || {
            let index = cached_search_index(&borrower, &root, storage.path());
            assert_eq!(
                index
                    .grep("readonly_needle", true, &[], &[], &root, 10)
                    .expect("grep over a test index")
                    .matches
                    .len(),
                1
            );
        });
        assert!(matches!(
            borrower.open_borrowed_semantic_index(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        let opened = open_search_index_read_only(&root, Some(storage.path()));
        assert!(
            matches!(
                opened,
                ReadOnlyArtifact::Fresh(_) | ReadOnlyArtifact::Stale(_)
            ),
            "Git may change ignore rules, but must not hide the saved index: {opened:?}"
        );
        assert!(matches!(
            open_semantic_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert!(
            crate::callgraph_store::CallGraphStore::open_readonly(callgraph_dir, root.clone())
                .unwrap()
                .is_some()
        );
        let readonly_session = borrowed_context(&root, storage.path());
        assert_eq!(
            readonly_session.callgraph_store_dir(),
            storage.path().join("callgraph").join(&saved_key)
        );
        assert!(
            readonly_session.cached_artifact_cache_key(&root).is_none(),
            "reader keys must not bypass configure's bind verification"
        );
        // Callgraph query pins are ephemeral reader metadata, not identities.
        let after = snapshot_dir(storage.path());
        for (path, snapshot) in &before {
            assert_eq!(
                after.get(path),
                Some(snapshot),
                "reader changed {}",
                path.display()
            );
        }
        assert_eq!(
            crate::root_identity::read(&root, storage.path()).unwrap(),
            Some(crate::root_identity::Identity::Path)
        );
    }

    #[test]
    fn borrowed_lookup_reloads_identity_after_configure_replaces_repository() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = project.path().join("checkout");
        let replacement = project.path().join("replacement");
        for (path, name) in [
            (&root, "original_needle"),
            (&replacement, "replacement_needle"),
        ] {
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(path.join("src/lib.rs"), format!("pub fn {name}() {{}}\n")).unwrap();
            init_git(path);
            commit_all(path);
        }
        let root = fs::canonicalize(root).unwrap();
        let bind = || {
            let owner = borrowed_context(&root, storage.path());
            let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
                "id": "replace-repository",
                "command": "configure",
                "project_root": root,
                "harness": "opencode",
                "storage_dir": storage.path(),
                "config": [{
                    "tier": "user",
                    "source": "/fixture/aft.jsonc",
                    "doc": serde_json::json!({
                        "indexes": { "trigram": true, "semantic": false, "callgraph": false },
                        "views": { "enabled": false },
                    }).to_string(),
                }],
            }))
            .unwrap();
            let response = serde_json::to_value(crate::commands::configure::handle_configure(
                &request, &owner,
            ))
            .unwrap();
            assert_eq!(response["success"], true, "{response}");
            owner
                .cached_artifact_cache_key(&root)
                .expect("settled repository identity")
        };
        let original_key = bind();
        build_search_artifact(&root, storage.path());
        let session = tempfile::tempdir().unwrap();
        let borrower = borrowed_context(session.path(), storage.path());
        let first = cached_search_index(&borrower, &root, storage.path());
        assert_eq!(
            first
                .grep("original_needle", true, &[], &[], &root, 10)
                .expect("grep over a test index")
                .matches
                .len(),
            1
        );

        fs::rename(&root, project.path().join("retired")).unwrap();
        fs::rename(replacement, &root).unwrap();
        let replacement_key = bind();
        assert_ne!(replacement_key, original_key);
        build_search_artifact(&root, storage.path());
        let second = cached_search_index(&borrower, &root, storage.path());
        assert_eq!(
            second
                .grep("replacement_needle", true, &[], &[], &root, 10)
                .expect("grep over a test index")
                .matches
                .len(),
            1,
            "the next borrow must open configure's new repository identity"
        );
        assert!(!std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(
            borrower
                .memoized_readonly_artifact_cache_key(&root, storage.path())
                .unwrap(),
            replacement_key
        );
        assert_eq!(borrower.artifact_cache_key_derivation_count_for_test(), 2);
        assert!(std::sync::Arc::ptr_eq(
            &second,
            &cached_search_index(&borrower, &root, storage.path())
        ));
    }

    #[test]
    fn readonly_identity_lookup_never_persists_and_refuses_a_busy_legacy_database() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let project = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(project.path()).unwrap();
        let storage = tempfile::tempdir().unwrap();
        let missing = storage.path().join("not-created");
        assert!(artifact_cache_key_read_only(&root, &missing).is_ok());
        assert!(!missing.exists());
        let writer = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
        writer
            .execute_batch("CREATE TABLE marker (v TEXT); BEGIN EXCLUSIVE;")
            .unwrap();
        let before = snapshot_dir(storage.path());
        let started = std::time::Instant::now();
        let ctx = borrowed_context(&root, storage.path());
        let error = ctx
            .memoized_readonly_artifact_cache_key(&root, storage.path())
            .unwrap_err();
        assert!(
            error.to_string().contains("identity provisional"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(ctx.artifact_cache_key_derivation_count_for_test(), 0);
        assert_eq!(snapshot_dir(storage.path()), before);
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn repeated_borrowed_opens_cache_one_load_per_artifact_generation() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        build_search_artifact(&root, storage.path());
        let ctx = borrowed_context(&root, storage.path());
        search_index_artifact_generation(&root, Some(storage.path()))
            .expect("seed borrowed artifact generation");
        let artifact_before = snapshot_dir(storage.path());

        crate::cache_freshness::reset_verify_file_strict_count_for_debug();
        let first = cached_search_index(&ctx, &root, storage.path());
        let second = cached_search_index(&ctx, &root, storage.path());
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 1);
        assert_eq!(
            ctx.artifact_cache_key_derivation_count_for_test(),
            1,
            "artifact identity should be derived only on the first external search"
        );
        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_under_for_debug(&root),
            0,
            "neither the first load nor a cache hit may run a corpus census"
        );
        assert_eq!(snapshot_dir(storage.path()), artifact_before);

        let added = root.join("src/added.rs");
        fs::write(&added, "pub fn generation_two() {}\n").expect("add generation file");
        build_search_artifact(&root, storage.path());
        let rebuilt_snapshot = snapshot_dir(storage.path());
        let third = cached_search_index(&ctx, &root, storage.path());
        assert!(
            !std::sync::Arc::ptr_eq(&first, &third),
            "an owner rebuild must invalidate the cached rerooted generation"
        );
        assert!(third.path_to_id.contains_key(&added));
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 1);
        assert_eq!(ctx.artifact_cache_key_derivation_count_for_test(), 1);
        assert_eq!(snapshot_dir(storage.path()), rebuilt_snapshot);

        assert!(ctx.evict_idle_artifacts());
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 0);
    }

    #[test]
    fn borrowed_search_load_outlives_interactive_budget_and_reuses_generation() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        build_search_artifact(&root, storage.path());
        let ctx = borrowed_context(&root, storage.path());
        let probe = std::sync::Arc::new(BorrowedSearchLoadProbe::default());
        probe.held.store(true, std::sync::atomic::Ordering::SeqCst);
        BORROWED_SEARCH_LOAD_PROBES
            .lock()
            .unwrap()
            .insert(root.clone(), probe.clone());
        let initial = with_borrowed_search_load_limits_for_test(100_000, Duration::ZERO, || {
            ctx.open_borrowed_search_index(&root, Some(storage.path()))
        });
        assert!(matches!(initial, ReadOnlyArtifact::Degraded(_)));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while probe.starts.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        probe.held.store(false, std::sync::atomic::Ordering::SeqCst);
        let first = loop {
            match ctx.open_borrowed_search_index(&root, Some(storage.path())) {
                ReadOnlyArtifact::Fresh(index) => break index,
                ReadOnlyArtifact::Degraded(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => panic!("background load did not complete: {other:?}"),
            }
        };
        let second = cached_search_index(&ctx, &root, storage.path());
        assert_eq!(
            probe.starts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "later calls must not restart the borrowed parse"
        );
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        BORROWED_SEARCH_LOAD_PROBES.lock().unwrap().remove(&root);
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 1);
        assert!(ctx.evict_idle_artifacts());
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 0);
    }

    #[test]
    fn repeated_borrowed_semantic_opens_reuse_rerooted_generation() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        build_semantic_artifact(&root, storage.path());
        let ctx = borrowed_context(&root, storage.path());
        semantic_index_artifact_generation(&root, Some(storage.path()))
            .expect("seed semantic artifact generation");
        let artifact_before = snapshot_dir(storage.path());

        let first = match ctx.open_borrowed_semantic_index(&root, Some(storage.path())) {
            ReadOnlyArtifact::Fresh(index) => index,
            other => panic!("expected borrowed semantic index, got {other:?}"),
        };
        let second = match ctx.open_borrowed_semantic_index(&root, Some(storage.path())) {
            ReadOnlyArtifact::Fresh(index) => index,
            other => panic!("expected cached semantic index, got {other:?}"),
        };
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 1);
        assert_eq!(snapshot_dir(storage.path()), artifact_before);
    }

    #[test]
    fn borrowed_index_cache_is_bounded_and_idle_evictable() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let storage = tempfile::tempdir().expect("storage");
        let mut projects = Vec::new();
        for _ in 0..=BORROWED_INDEX_CACHE_CAPACITY {
            let (project, root) = fixture_project();
            build_search_artifact(&root, storage.path());
            projects.push((project, root));
        }
        let ctx = borrowed_context(&projects[0].1, storage.path());
        let first = cached_search_index(&ctx, &projects[0].1, storage.path());
        for (_, root) in projects.iter().skip(1) {
            cached_search_index(&ctx, root, storage.path());
        }
        assert_eq!(
            ctx.borrowed_index_cache_len_for_test(),
            BORROWED_INDEX_CACHE_CAPACITY
        );
        let reloaded_first = cached_search_index(&ctx, &projects[0].1, storage.path());
        assert!(!std::sync::Arc::ptr_eq(&first, &reloaded_first));

        assert!(ctx.evict_idle_artifacts());
        assert_eq!(ctx.borrowed_index_cache_len_for_test(), 0);
    }

    #[test]
    fn stale_posting_verification_stops_at_injected_file_budget() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        for file_index in 0..20 {
            fs::write(
                root.join(format!("src/stale_{file_index}.rs")),
                "pub fn stale_budget_needle() {}\n",
            )
            .expect("write indexed stale candidate");
        }
        let storage = tempfile::tempdir().expect("storage");
        build_search_artifact(&root, storage.path());
        for file_index in 0..20 {
            fs::write(
                root.join(format!("src/stale_{file_index}.rs")),
                "pub fn changed_after_index_build() {}\n",
            )
            .expect("replace indexed stale candidate");
        }
        let index = match open_search_index_read_only(&root, Some(storage.path())) {
            ReadOnlyArtifact::Fresh(index) => index,
            other => panic!("expected borrowed index, got {other:?}"),
        };
        let compiled = match crate::pattern_compile::compile(
            "stale_budget_needle",
            crate::pattern_compile::CompileOpts {
                literal: true,
                ..crate::pattern_compile::CompileOpts::default()
            },
        ) {
            crate::pattern_compile::CompileResult::Ok(compiled) => compiled,
            other => panic!("literal compile failed: {other:?}"),
        };

        let result = index
            .snapshot()
            .search_grep_bounded(
                &compiled,
                &[],
                &[],
                &root,
                10,
                None,
                1,
                std::time::Duration::from_secs(1),
            )
            .unwrap();
        assert!(result.matches.is_empty());
        assert!(result.files_searched <= 1);
        assert!(result.truncated);
        assert!(result.engine_capped);
    }

    #[test]
    fn churned_borrowed_generation_stops_at_load_budget() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        for file_index in 0..8 {
            fs::write(
                root.join(format!("src/module_{file_index}.rs")),
                format!("pub fn borrowed_budget_{file_index}() {{}}\n"),
            )
            .expect("write borrowed corpus file");
        }
        let storage = tempfile::tempdir().expect("storage");
        let cache_dir = build_search_artifact(&root, storage.path());
        assert!(matches!(
            open_search_index_from_cache_dir_with_budget(
                &root,
                cache_dir.clone(),
                10_000,
                Duration::from_secs(5),
            ),
            ReadOnlyArtifact::Fresh(_)
        ));

        for file_index in 0..8 {
            fs::write(
                root.join(format!("src/module_{file_index}.rs")),
                format!("pub fn churned_borrowed_budget_{file_index}() {{}}\n"),
            )
            .expect("churn borrowed corpus file");
        }
        build_search_artifact(&root, storage.path());

        // The churned generation holds 8 files, inside the 10-entry budget
        // below, while its trigram lookup table holds far more entries, so
        // the reload reads the 8 file entries and then stops at the entry
        // budget. The time budget is generous on purpose: with time unable
        // to stop the reload, a count inside 1..=10 proves the entry budget
        // is what stopped it. The lower bound matters too: a probe that
        // never saw the reload reads 0 and fails loudly instead of passing
        // vacuously. Elapsed time cannot show any of this on a loaded
        // runner, where even a stopped-early reload can exceed 1 s.
        let records = crate::search_index::observe_borrowed_load_records_read_for_test(&root);
        let opened = open_search_index_from_cache_dir_with_budget(
            &root,
            cache_dir,
            10,
            Duration::from_secs(60),
        );
        crate::search_index::unobserve_borrowed_load_records_read_for_test(&root);
        assert!(
            (1..=10).contains(&records.load(std::sync::atomic::Ordering::SeqCst)),
            "record budget must stop before rerooting the churned corpus, read {} records",
            records.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(matches!(
            opened,
            ReadOnlyArtifact::Degraded(BORROWED_SEARCH_LOAD_DEGRADATION)
        ));
    }

    #[test]
    fn cancelled_borrowed_load_stops_at_checkpoint() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let cache_dir = build_search_artifact(&root, storage.path());
        let cancellation = crate::executor::JobCancellation::new();
        cancellation.request_cancel();
        let _installed = crate::executor::install_job_cancellation(cancellation);

        assert!(matches!(
            open_search_index_from_cache_dir_with_budget(
                &root,
                cache_dir,
                10_000,
                Duration::from_secs(5),
            ),
            ReadOnlyArtifact::Cancelled
        ));
    }

    #[test]
    fn oversized_borrowed_semantic_artifact_degrades_before_parse() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let data_path = storage
            .path()
            .join("semantic")
            .join(artifact_cache_key(&root))
            .join("semantic.bin");
        fs::create_dir_all(data_path.parent().expect("semantic parent"))
            .expect("create semantic artifact directory");
        let file = fs::File::create(&data_path).expect("create sparse semantic artifact");
        file.set_len(BORROWED_SEMANTIC_MAX_BYTES + 1)
            .expect("size sparse semantic artifact");

        assert!(matches!(
            open_semantic_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Degraded(BORROWED_SEMANTIC_LOAD_DEGRADATION)
        ));
    }

    #[cfg(debug_assertions)]
    #[test]
    fn search_opener_skips_full_corpus_strict_census() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");

        assert!(matches!(
            open_search_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Absent
        ));

        build_search_artifact(&root, storage.path());
        crate::cache_freshness::reset_verify_file_strict_count_for_debug();
        assert!(matches!(
            open_search_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_under_for_debug(&root),
            0,
            "a borrowed open must not restore the full-corpus strict hash census"
        );

        fs::write(
            root.join("src/lib.rs"),
            "pub fn readonly_needle() -> bool { false }\n",
        )
        .expect("mutate fixture");
        match open_search_index_read_only(&root, Some(storage.path())) {
            ReadOnlyArtifact::Fresh(index) => {
                assert!(index.stored_git_head().is_some());
            }
            other => panic!("expected silently served borrowed artifact, got {other:?}"),
        }
        assert_eq!(
            crate::cache_freshness::verify_file_strict_count_under_for_debug(&root),
            0
        );
    }

    #[test]
    fn search_opener_marks_cross_checkout_ignore_rule_mismatch_as_stale() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let owner_only_ignore = root.join(".foo/.gitignore");
        fs::create_dir_all(owner_only_ignore.parent().expect("ignore parent"))
            .expect("create ignore dir");
        fs::write(&owner_only_ignore, "# owner-only ignore file\n").expect("write ignore file");
        build_search_artifact(&root, storage.path());

        let (_clone, sibling_root) = clone_checkout(&root);
        match open_search_index_read_only(&sibling_root, Some(storage.path())) {
            ReadOnlyArtifact::Stale(stale) => {
                assert!(stale.ignore_rules_differ);
                assert!(stale.index.stored_git_head().is_some());
            }
            other => panic!("expected stale borrowed artifact, got {other:?}"),
        }
    }

    #[test]
    fn owner_search_loader_stays_strict_on_ignore_rule_mismatch() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let cache_dir = build_search_artifact(&root, storage.path());
        let owner_only_ignore = root.join(".foo/.gitignore");
        fs::create_dir_all(owner_only_ignore.parent().expect("ignore parent"))
            .expect("create ignore dir");
        fs::write(&owner_only_ignore, "# owner-only ignore file\n").expect("write ignore file");

        assert!(SearchIndex::read_from_disk(&cache_dir, &root).is_none());
    }

    fn linked_worktree(root: &Path) -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("create worktree dir");
        let worktree = temp.path().join("wt");
        let status = git_command(root)
            .args(["worktree", "add", "-q", "--detach"])
            .arg(&worktree)
            .arg("HEAD")
            .status()
            .expect("git worktree add");
        assert!(status.success(), "git worktree add failed");
        let worktree = fs::canonicalize(worktree).expect("canonical worktree root");
        (temp, worktree)
    }

    fn write_info_exclude(root: &Path, contents: &str) {
        let exclude = root.join(".git/info/exclude");
        fs::create_dir_all(exclude.parent().expect("info dir")).expect("create info dir");
        fs::write(exclude, contents).expect("write info/exclude");
    }

    /// `.git/info/exclude` is shared by the home checkout and its worktrees but
    /// sits inside only the home checkout's root. Its location alone must not
    /// make a worktree's borrow look like an ignore-rule difference.
    #[test]
    fn linked_worktree_borrow_is_fresh_when_ignore_rules_match() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        write_info_exclude(&root, "*.local\n");
        let storage = tempfile::tempdir().expect("storage");
        build_search_artifact(&root, storage.path());
        let (_wt_dir, worktree) = linked_worktree(&root);

        let borrowed = with_borrowed_search_load_limits_for_test(
            BORROWED_SEARCH_BACKGROUND_MAX_RECORDS,
            Duration::from_secs(30),
            || open_search_index_read_only(&worktree, Some(storage.path())),
        );
        match borrowed {
            ReadOnlyArtifact::Fresh(index) => {
                assert!(index.path_to_id.contains_key(&worktree.join("src/lib.rs")));
            }
            other => panic!("expected a fresh borrowed artifact, got {other:?}"),
        }
        assert_eq!(
            crate::search_index::ignore_rules_fingerprint(&root),
            crate::search_index::ignore_rules_fingerprint(&worktree)
        );
    }

    /// The shared `info/exclude` content still drives the fingerprint: edited
    /// rules must be reported to a borrower and refused by the owner.
    #[test]
    fn info_exclude_content_change_is_still_an_ignore_rule_difference() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        write_info_exclude(&root, "*.local\n");
        let storage = tempfile::tempdir().expect("storage");
        let cache_dir = build_search_artifact(&root, storage.path());
        let (_wt_dir, worktree) = linked_worktree(&root);
        write_info_exclude(&root, "*.local\n*.generated\n");

        match open_search_index_read_only(&worktree, Some(storage.path())) {
            ReadOnlyArtifact::Stale(stale) => assert!(stale.ignore_rules_differ),
            other => panic!("expected a stale borrowed artifact, got {other:?}"),
        }
        assert!(SearchIndex::read_from_disk(&cache_dir, &root).is_none());
    }

    /// Older versions let a session opened in a subfolder publish its partial
    /// snapshot under the repository key. Such an artifact must be reported as
    /// a coverage mismatch to a borrowing worktree, and rebuilt by the owner,
    /// rather than served as a complete index.
    #[test]
    fn repository_artifact_written_by_a_subfolder_session_is_not_adopted() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let sub = root.join("src");
        let sub_cache = build_search_artifact(&sub, storage.path());
        let repository_cache = resolve_cache_dir(&root, Some(storage.path()));
        assert_ne!(sub_cache, repository_cache);
        fs::create_dir_all(&repository_cache).expect("create repository cache dir");
        fs::copy(
            sub_cache.join("cache.bin"),
            repository_cache.join("cache.bin"),
        )
        .expect("plant subfolder snapshot under the repository key");
        let (_wt_dir, worktree) = linked_worktree(&root);

        match open_search_index_read_only(&worktree, Some(storage.path())) {
            ReadOnlyArtifact::Degraded(degradation) => {
                assert_eq!(degradation, BORROWED_SEARCH_COVERAGE_DEGRADATION);
            }
            other => panic!("expected a coverage-mismatch degradation, got {other:?}"),
        }
        assert!(SearchIndex::read_from_disk(&repository_cache, &root).is_none());
        assert!(
            SearchIndex::read_from_disk(&sub_cache, &sub).is_some(),
            "the subfolder's own artifact must stay usable for the subfolder"
        );
    }

    #[test]
    fn read_only_openers_never_modify_artifact_directory() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let search_cache_dir = build_search_artifact(&root, storage.path());
        build_semantic_artifact(&root, storage.path());
        crate::search_index::artifact_cache_key_with_memo(&root, &root, storage.path(), None)
            .expect("seed cache-key memo before read-only snapshot");
        let semantic_cache_dir = storage
            .path()
            .join("semantic")
            .join(artifact_cache_key(&root));

        let before = snapshot_dir(storage.path());
        assert!(matches!(
            open_search_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert!(matches!(
            open_semantic_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert_eq!(snapshot_dir(storage.path()), before);

        fs::write(
            root.join("src/lib.rs"),
            "pub fn readonly_needle() -> bool { false }\n",
        )
        .expect("mutate fixture");
        let stale_before = snapshot_dir(storage.path());
        assert!(matches!(
            open_search_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert!(matches!(
            open_semantic_index_read_only(&root, Some(storage.path())),
            ReadOnlyArtifact::Fresh(_)
        ));
        assert_eq!(snapshot_dir(storage.path()), stale_before);
        assert!(search_cache_dir.join("cache.bin").is_file());
        assert!(semantic_cache_dir.join("semantic.bin").is_file());
    }
}
