use std::path::{Path, PathBuf};

// These openers borrow cache artifacts that may be owned by a different AFT
// session. They therefore only read and verify the opened snapshot; any repair,
// migration, deletion, or rebuild must be left to the session that owns writes
// for that project.
use crate::cache_freshness::{artifact_generation, ArtifactGeneration};
use crate::search_index::{
    artifact_cache_key_with_memo, resolve_cache_dir, resolve_cache_dir_with_key, SearchIndex,
};
use crate::semantic_index::SemanticIndex;

#[derive(Clone, Debug)]
pub(crate) enum ReadOnlyArtifact<T> {
    Fresh(T),
    Stale(ReadOnlyStale<T>),
    Absent,
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
    Other(String),
}

pub(crate) fn resolve_git_root_from_user_path(
    project_root: &Path,
    raw_path: &str,
) -> Result<PathBuf, GitRootResolutionError> {
    let expanded = expand_tilde(raw_path);
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
    git_toplevel(&git_base).map_err(|error| match error.as_str() {
        "not_a_git_root" => GitRootResolutionError::NotAGitRoot,
        _ => GitRootResolutionError::Other(error),
    })
}

#[cfg(test)]
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

pub(crate) fn open_search_index_read_only(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> ReadOnlyArtifact<SearchIndex> {
    let Some(cache_dir) = search_index_cache_dir(project_root, storage_dir) else {
        return ReadOnlyArtifact::Absent;
    };
    open_search_index_from_cache_dir(project_root, cache_dir)
}

pub(crate) fn open_search_index_read_only_with_key(
    project_root: &Path,
    storage_dir: Option<&Path>,
    project_key: &str,
) -> ReadOnlyArtifact<SearchIndex> {
    open_search_index_from_cache_dir(
        project_root,
        resolve_cache_dir_with_key(project_key, storage_dir),
    )
}

fn open_search_index_from_cache_dir(
    project_root: &Path,
    cache_dir: PathBuf,
) -> ReadOnlyArtifact<SearchIndex> {
    if !cache_dir.join("cache.bin").is_file() {
        return ReadOnlyArtifact::Absent;
    }

    let Some((mut index, ignore_rules_differ)) =
        SearchIndex::read_from_disk_borrow_tolerant(&cache_dir, project_root)
    else {
        return ReadOnlyArtifact::Absent;
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
        Some(storage_dir) => {
            match artifact_cache_key_with_memo(project_root, project_root, storage_dir, None) {
                Ok(project_key) => {
                    Some(resolve_cache_dir_with_key(&project_key, Some(storage_dir)))
                }
                Err(error) => {
                    crate::slog_warn!("read-only search index unavailable: {}", error);
                    None
                }
            }
        }
        None => Some(resolve_cache_dir(project_root, None)),
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
    let Some(storage_dir) = storage_dir else {
        return ReadOnlyArtifact::Absent;
    };
    if !data_path.is_file() {
        return ReadOnlyArtifact::Absent;
    }

    SemanticIndex::read_from_disk_borrow_tolerant(storage_dir, project_key, project_root)
        .map(ReadOnlyArtifact::Fresh)
        .unwrap_or(ReadOnlyArtifact::Absent)
}

fn semantic_index_location(
    project_root: &Path,
    storage_dir: Option<&Path>,
) -> Option<(PathBuf, String)> {
    let storage_dir = storage_dir?;
    let project_key =
        match artifact_cache_key_with_memo(project_root, project_root, storage_dir, None) {
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

fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
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
    let output = crate::effective_path::new_command("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(base_dir)
        .output()
        .map_err(|error| format!("failed to run git: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Err("not_a_git_root".to_string());
        }
        return Err(format!("git rev-parse failed: {}", stderr.trim()));
    }

    let toplevel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if toplevel.is_empty() {
        return Err("git rev-parse returned an empty toplevel".to_string());
    }
    let toplevel = PathBuf::from(toplevel);
    Ok(std::fs::canonicalize(&toplevel).unwrap_or(toplevel))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::fs;
    use std::process::Command;
    use std::time::SystemTime;

    use tempfile::TempDir;

    use crate::context::BORROWED_INDEX_CACHE_CAPACITY;
    use crate::search_index::artifact_cache_key;
    use crate::semantic_index::{SemanticIndex, SemanticIndexFingerprint};

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
        let source = root.join("src/lib.rs");
        let fingerprint = SemanticIndexFingerprint {
            backend: "openai_compatible".to_string(),
            model: "readonly-test".to_string(),
            base_url: "http://127.0.0.1".to_string(),
            dimension: 3,
            chunking_version: 1,
        };
        let mut embed =
            |texts: Vec<String>| Ok::<_, String>(vec![vec![0.1, 0.2, 0.3]; texts.len()]);
        let mut index =
            SemanticIndex::build(root, &[source], &mut embed, 8).expect("build semantic index");
        index.set_fingerprint(fingerprint);
        index.write_to_disk(storage, &artifact_cache_key(root));
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
            ReadOnlyArtifact::Absent => panic!("expected borrowed search index"),
        }
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

        let result = index.snapshot().search_grep_bounded(
            &compiled,
            &[],
            &[],
            &root,
            10,
            None,
            1,
            std::time::Duration::from_secs(1),
        );
        assert!(result.matches.is_empty());
        assert!(result.files_searched <= 1);
        assert!(result.truncated);
        assert!(result.engine_capped);
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

    #[test]
    fn read_only_openers_never_modify_artifact_directory() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (_project, root) = fixture_project();
        let storage = tempfile::tempdir().expect("storage");
        let search_cache_dir = build_search_artifact(&root, storage.path());
        build_semantic_artifact(&root, storage.path());
        artifact_cache_key_with_memo(&root, &root, storage.path(), None)
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
