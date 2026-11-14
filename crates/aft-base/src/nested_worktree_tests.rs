//! Cross-walker regression tests: a directory that is a nested linked
//! worktree of this repository must be skipped by every content-reading walk,
//! while a nested independent repository and a submodule-style checkout are
//! still walked. An explicitly named target inside a worktree keeps the rule
//! that already applies to explicitly named gitignored paths: a named file is
//! read directly, and a named directory is walked with the index bypassed.

use std::collections::BTreeSet;
use std::path::PathBuf;

const MARKER: &str = "nested_worktree_probe_746";

pub fn fixture() -> (tempfile::TempDir, crate::git_probe::NestedWorktreeFixture) {
    let dir = tempfile::tempdir().unwrap();
    let fixture = crate::git_probe::init_repo_with_nested_worktree_for_test(dir.path(), MARKER);
    (dir, fixture)
}

pub fn relative_set(
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
