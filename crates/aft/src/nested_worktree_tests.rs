//! Cross-walker regression tests: a directory that is a nested linked
//! worktree of this repository must be skipped by every content-reading walk,
//! while a nested independent repository and a submodule-style checkout are
//! still walked. An explicitly named target inside a worktree keeps the rule
//! that already applies to explicitly named gitignored paths: a named file is
//! read directly, and a named directory is walked with the index bypassed.

#[allow(unused_imports)]
pub(crate) use aft_base::nested_worktree_tests::*;
use aft_engine::test_bridge::search_misc::search::SearchSnapshotTest as _;

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

/// A `.git` file is a linked worktree's or submodule's pointer to Git data,
/// never project content. It used to be walked as a source file, so a linked
/// worktree's views manifest had an entry its base lacked.
#[test]
fn project_walk_skips_git_pointer_files() {
    let (_dir, fixture) = fixture();
    for root in [&fixture.root, &fixture.worktree] {
        let walked = relative_set(
            root,
            crate::search_index::project_walk_builder(root)
                .build()
                .filter_map(Result::ok)
                .filter(|entry| entry.depth() > 0)
                .map(|entry| entry.into_path()),
        );
        assert!(
            !walked
                .iter()
                .any(|path| path == ".git" || path.ends_with("/.git")),
            "a .git entry was walked under {}: {walked:?}",
            root.display()
        );
    }
    assert!(
        std::fs::metadata(fixture.worktree.join(".git"))
            .unwrap()
            .is_file(),
        "the linked worktree's .git must be a file for this test to mean anything"
    );
}

#[test]
fn callgraph_walk_skips_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let walked = relative_set(
        root,
        crate::callgraph::walk_project_files(
            root,
            if crate::data_layer::read_fence::active() {
                crate::project_walk::IgnoreFileAccess::Fenced
            } else {
                crate::project_walk::IgnoreFileAccess::Allowed
            },
        ),
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
fn grep_fallback_walk_skips_nested_worktrees() {
    let (_dir, fixture) = fixture();
    let root = &fixture.root;
    let filters = crate::search_index::PathFilters::default();
    let outcome = aft_engine::test_bridge::search_misc::bounded_fallback_walk_files(
        root, root, &filters, "grep",
    );
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
    let outcome = aft_engine::test_bridge::search_misc::bounded_fallback_walk_files(
        &fixture.worktree,
        &fixture.worktree,
        &filters,
        "grep",
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
    let ctx = crate::test_context(
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
    let index = aft_engine::test_bridge::search_misc::search_index_fixture::build(root);
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
        aft_engine::test_bridge::search_misc::watcher_path_is_infra_skip_under(root, &linked_file),
        "events under a nested linked worktree must be dropped"
    );
    assert!(
        aft_engine::test_bridge::search_misc::watcher_path_is_infra_skip_under(
            root,
            &fixture.worktree
        ),
        "the worktree root itself must be dropped"
    );
    for path in [&main_file, &independent_file, &submodule_file] {
        assert!(
            !aft_engine::test_bridge::search_misc::watcher_path_is_infra_skip_under(root, path),
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
