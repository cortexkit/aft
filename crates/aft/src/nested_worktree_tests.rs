//! Cross-walker regression tests: a directory that is a nested linked
//! worktree of this repository must be skipped by every content-reading walk,
//! while a nested independent repository and a submodule-style checkout are
//! still walked. An explicitly named target inside a worktree keeps the rule
//! that already applies to explicitly named gitignored paths: a named file is
//! read directly, and a named directory is walked with the index bypassed.

use std::collections::BTreeSet;
use std::path::PathBuf;

const MARKER: &str = "nested_worktree_probe_746";

fn fixture() -> (tempfile::TempDir, crate::git_probe::NestedWorktreeFixture) {
    let dir = tempfile::tempdir().unwrap();
    let fixture = crate::git_probe::init_repo_with_nested_worktree_for_test(dir.path(), MARKER);
    (dir, fixture)
}

fn relative_set(
    root: &std::path::Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> BTreeSet<String> {
    paths
        .into_iter()
        .map(|path| {
            path.strip_prefix(root)
                .unwrap_or_else(|_| panic!("{} is outside {}", path.display(), root.display()))
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

#[test]
fn boundary_marks_only_this_repositories_linked_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let boundary = crate::project_content::NestedWorktreeBoundary::for_root(root);
    assert!(
        boundary.is_nested_worktree_root(&fixture.worktree),
        "linked worktree not detected"
    );
    // Repeated lookups agree (per-directory verdict cache).
    assert!(boundary.is_nested_worktree_root(&fixture.worktree));
    assert!(
        !boundary.is_nested_worktree_root(root),
        "project root is never a worktree"
    );
    assert!(
        !boundary.is_nested_worktree_root(&fixture.independent),
        "nested independent repository must still be walked"
    );
    assert!(
        !boundary.is_nested_worktree_root(&fixture.submodule),
        "submodule-style gitdir must still be walked"
    );
    assert!(!boundary.is_nested_worktree_root(&root.join("src")));
    assert!(boundary.is_under_nested_worktree(&fixture.worktree.join("src/main.rs")));
    assert!(boundary.is_under_nested_worktree(&fixture.worktree));
    assert!(!boundary.is_under_nested_worktree(&root.join("src/main.rs")));
    assert!(!boundary.is_under_nested_worktree(root));
    assert!(!boundary.is_under_nested_worktree(&fixture.independent.join("inner.rs")));
    assert!(!boundary.is_under_nested_worktree(&fixture.submodule.join("code.rs")));
}

#[test]
fn boundary_ignores_a_worktree_of_another_repository() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    // A `.git` file naming a `worktrees/` entry of a different common dir is
    // a foreign checkout nested here, not this repository's worktree.
    let foreign = root.join("foreign");
    let foreign_admin = root.join("elsewhere.git").join("worktrees").join("foreign");
    std::fs::create_dir_all(&foreign_admin).unwrap();
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(
        foreign.join(".git"),
        format!("gitdir: {}\n", foreign_admin.display()),
    )
    .unwrap();
    assert!(
        !crate::project_content::NestedWorktreeBoundary::for_root(root)
            .is_nested_worktree_root(&foreign),
        "foreign worktrees entry must still be walked"
    );
}

#[test]
fn trigram_project_walk_skips_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let walked = relative_set(
        root,
        crate::search_index::project_walk_builder(root)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .map(|entry| entry.into_path()),
    );
    assert!(
        walked.contains("src/main.rs"),
        "missing main file: {walked:?}"
    );
    assert!(
        walked.contains("lib/util.rs"),
        "missing main file: {walked:?}"
    );
    assert!(
        walked.contains("vendor/independent/inner.rs"),
        "nested independent repository must still be walked: {walked:?}"
    );
    assert!(
        walked.contains("submod/code.rs"),
        "submodule-style checkout must still be walked: {walked:?}"
    );
    assert!(
        !walked.iter().any(|path| path.starts_with("linked/")),
        "nested linked worktree files were walked: {walked:?}"
    );
}

#[test]
fn callgraph_walk_skips_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let walked = relative_set(root, crate::callgraph::walk_project_files(root));
    assert!(
        walked.contains("src/main.rs"),
        "missing main file: {walked:?}"
    );
    assert!(
        walked.contains("lib/util.rs"),
        "missing main file: {walked:?}"
    );
    assert!(
        walked.contains("vendor/independent/inner.rs"),
        "nested independent repository must still be walked: {walked:?}"
    );
    assert!(
        walked.contains("submod/code.rs"),
        "submodule-style checkout must still be walked: {walked:?}"
    );
    assert!(
        !walked.iter().any(|path| path.starts_with("linked/")),
        "nested linked worktree files were walked: {walked:?}"
    );
}

#[test]
fn grep_fallback_walk_skips_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let filters = crate::search_index::PathFilters::default();
    let outcome = crate::grep_executor::bounded_fallback_walk_files(root, root, &filters);
    assert!(!outcome.walk_truncated, "fixture walk must complete");
    let walked = relative_set(root, outcome.files);
    assert!(
        walked.contains("src/main.rs"),
        "missing main file: {walked:?}"
    );
    assert!(
        walked.contains("vendor/independent/inner.rs"),
        "nested independent repository must still be walked: {walked:?}"
    );
    assert!(
        walked.contains("submod/code.rs"),
        "submodule-style checkout must still be walked: {walked:?}"
    );
    assert!(
        !walked.iter().any(|path| path.starts_with("linked/")),
        "nested linked worktree files were walked: {walked:?}"
    );
}

#[test]
fn explicitly_named_worktree_target_still_walks_like_an_ignored_target() {
    let (_dir, fixture) = fixture();
    let filters = crate::search_index::PathFilters::default();
    // A directory explicitly named inside the worktree is walked: the boundary
    // only prunes entries below the walk's own root, never the root itself,
    // the same rule explicitly named gitignored directories follow.
    let outcome = crate::grep_executor::bounded_fallback_walk_files(
        &fixture.worktree,
        &fixture.worktree,
        &filters,
    );
    assert!(!outcome.walk_truncated, "fixture walk must complete");
    let walked = relative_set(&fixture.worktree, outcome.files);
    assert!(
        walked.contains("src/main.rs"),
        "explicitly named worktree target must walk: {walked:?}"
    );
    assert!(
        walked.contains("extra.rs"),
        "explicitly named worktree target must walk: {walked:?}"
    );
    // A file explicitly named inside the worktree stays readable, exactly like
    // an explicitly named gitignored file: only storage and SQLite names are
    // refused by the guarded readers.
    let named = fixture.worktree.join("src/main.rs");
    assert!(
        !crate::project_content::is_excluded(&named),
        "explicitly named worktree file must stay readable"
    );
    assert!(crate::project_content::read(&named).is_ok());
}

#[cfg(unix)]
#[test]
fn glob_fallback_skips_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config {
            project_root: Some(root.clone()),
            ..Default::default()
        },
    );
    // No index is installed, so glob answers from the fallback walk.
    let (files, source, _) =
        crate::commands::glob::glob_root_files_for_test(&ctx, root, root, "**/*.rs", 100, None);
    assert_eq!(source, "fallback");
    let walked = relative_set(root, files);
    assert!(
        walked.contains("src/main.rs"),
        "missing main file: {walked:?}"
    );
    assert!(
        walked.contains("vendor/independent/inner.rs"),
        "nested independent repository must still be walked: {walked:?}"
    );
    assert!(
        walked.contains("submod/code.rs"),
        "submodule-style checkout must still be walked: {walked:?}"
    );
    assert!(
        !walked.iter().any(|path| path.starts_with("linked/")),
        "nested linked worktree files were walked: {walked:?}"
    );
}

#[test]
fn trigram_index_holds_no_nested_worktree_files() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let index = crate::search_index::SearchIndex::build(root);
    assert!(index.is_ready(), "fixture index must build");
    let snapshot = index.snapshot();
    assert!(snapshot.has_file_in_scope(&root.join("src/main.rs")));
    assert!(snapshot.has_file_in_scope(&root.join("vendor/independent/inner.rs")));
    assert!(!snapshot.has_file_in_scope(&fixture.worktree.join("src/main.rs")));
    assert!(!snapshot.has_file_in_scope(&fixture.worktree.join("extra.rs")));
}

#[test]
fn watcher_drops_events_under_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let linked_file = fixture.worktree.join("src/main.rs");
    let main_file = root.join("src/main.rs");
    let independent_file = fixture.independent.join("inner.rs");
    let submodule_file = fixture.submodule.join("code.rs");
    assert!(
        crate::watcher_filter::watcher_path_is_infra_skip_under(root, &linked_file),
        "events under a nested linked worktree must be dropped"
    );
    assert!(
        crate::watcher_filter::watcher_path_is_infra_skip_under(root, &fixture.worktree),
        "the worktree root itself must be dropped"
    );
    for path in [&main_file, &independent_file, &submodule_file] {
        assert!(
            !crate::watcher_filter::watcher_path_is_infra_skip_under(root, path),
            "{} must still be watched",
            path.display()
        );
    }
    // End to end through one filter flush: the worktree event is dropped, the
    // others are kept.
    let config = crate::watcher_filter::WatcherFilterConfig::new(root.clone(), None);
    let matcher = std::sync::Arc::new(std::sync::RwLock::new(None));
    let filtered = crate::watcher_filter::filter_watcher_raw_paths_for_test(
        &config,
        &matcher,
        [linked_file.clone(), main_file.clone()],
    );
    assert!(
        !filtered.changed.contains(&linked_file),
        "worktree event reached the checkout: {:?}",
        filtered.changed
    );
    assert!(
        filtered.changed.contains(&main_file),
        "main checkout event was dropped: {:?}",
        filtered.changed
    );
}
