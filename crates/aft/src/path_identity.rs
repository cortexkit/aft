//! Project-root identity primitives (P0 of the subc migration).
//!
//! Three distinct identity roles live here or are named here — do NOT conflate
//! them, because conflating them is the bug class this module exists to kill:
//!
//! 1. [`ProjectRootId`] (re-exported from `cortexkit-paths`) — the canonical
//!    *path* identity for ROUTING and per-root SCOPING: bridge routing, RPC
//!    port files, the (future) ProjectActor map and lease, and the
//!    bash/compression/backup/checkpoint scope keys derived via
//!    [`project_scope_key`]. Shared byte-for-byte with subc by construction
//!    (same crate), so a session attaches to the same root subc routed it to.
//!
//! Use one shared cache key for data that can be reused across all worktrees of
//! the same Git repository, and a separate key for checkout-specific inspect
//! data. For Git repositories, derive the shared key from the repository's root
//! commit so every worktree can reuse the same search cache; for non-Git
//! projects, derive it from the canonical filesystem path. Keep the previous
//! `project_cache_key` format for compatibility with existing caches.
//!
//! 3. Operation-target / lexical path handling (create-file fallback, relative
//!    path joins) — not identity; stays local to its call sites.
//!
//! Consequence of the split: two worktrees of one repo SHARE the artifact key
//! (same root commit → shared index) but get DISTINCT `ProjectRootId`s → a
//! distinct [`project_scope_key`]. A background bash task, undo history, or
//! token-savings stat created in worktree A never surfaces under worktree B.

use std::path::{Path, PathBuf};

pub use cortexkit_paths::{IdentityError, ProjectRootId};

/// Stable 16-hex scope key for per-root mutable state keyed by *canonical path*:
/// background bash tasks, compression aggregation, backup metadata, checkpoint
/// locks.
///
/// Derived from [`ProjectRootId`] (the shared canonical path), so it is the
/// per-checkout identity — distinct from `artifact_cache_key`, which is the
/// per-repository identity (root commit). Two worktrees of one repo get
/// different scope keys (correct) but the same artifact key (shared index).
///
/// Non-existent roots fall back to a lexical normalization so derivation is
/// total: the shared crate rejects non-existent paths, and while runtime
/// project roots always exist, `current_dir`-derived callers (e.g. a default
/// checkpoint store) and tests may pass a path that does not.
pub fn project_scope_key(project_root: &Path) -> String {
    let canonical = ProjectRootId::from_path(project_root)
        .map(ProjectRootId::into_path_buf)
        .unwrap_or_else(|_| lexical_normalize(project_root));
    hash16(&canonical)
}

fn hash16(path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..16].to_string()
}

/// Lexically resolve `.`/`..`/`CurDir` without touching the filesystem. Mirrors
/// the non-existent-path fallback used elsewhere so a missing root still yields
/// a stable, traversal-safe key instead of panicking.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !result.pop() {
                    result.push(component);
                }
            }
            Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh root directory, removed when the returned guard drops.
    fn temp_root(label: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("aft-path-identity-{label}-"))
            .tempdir()
            .expect("create temp root");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn scope_key_is_stable_and_16_hex() {
        let (_root_dir, root) = temp_root("stable");
        let a = project_scope_key(&root);
        let b = project_scope_key(&root);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scope_key_distinguishes_distinct_roots() {
        let (_a_dir, a) = temp_root("root-a");
        let (_b_dir, b) = temp_root("root-b");
        assert_ne!(project_scope_key(&a), project_scope_key(&b));
    }

    #[test]
    fn scope_key_canonicalizes_equivalent_spellings() {
        let (_root_dir, root) = temp_root("spelling");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("create nested");
        // `root/nested/..` and `root/.` both canonicalize to `root`.
        assert_eq!(
            project_scope_key(&root),
            project_scope_key(&nested.join("..")),
        );
        assert_eq!(project_scope_key(&root), project_scope_key(&root.join(".")));
    }

    #[test]
    fn scope_key_total_on_non_existent_root() {
        // Non-existent path: lexical fallback, no panic, stable.
        let missing = std::env::temp_dir().join("aft-path-identity-definitely-missing-xyz/sub/..");
        let key = project_scope_key(&missing);
        assert_eq!(key.len(), 16);
    }

    /// The headline P0 invariant: a linked git worktree SHARES the main
    /// checkout's artifact cache key (same root commit → shared index, opened
    /// read-only) but gets a DISTINCT scope key (its own checkout path → its own
    /// bash/compression/backup/checkpoint namespace). Before the split, both
    /// were the same root-commit key, so worktree A's tasks/undo bled into B.
    #[test]
    fn worktree_shares_artifact_key_but_has_distinct_scope_key() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        use std::process::Command;

        let mut git_version = Command::new("git");
        let git_ok = crate::test_env::apply_hermetic_git_env(&mut git_version)
            .arg("--version")
            .output()
            .is_ok();
        if !git_ok {
            eprintln!("skipping: git not available");
            return;
        }

        let tmp = std::env::temp_dir().join(format!(
            "aft-worktree-iso-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let main = tmp.join("main");
        fs::create_dir_all(&main).expect("create main checkout");

        let run = |args: &[&str], cwd: &std::path::Path| {
            let mut command = Command::new("git");
            assert!(
                crate::test_env::apply_hermetic_git_env(command.current_dir(cwd))
                    .args(args)
                    .status()
                    .expect("run git")
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init"], &main);
        fs::write(main.join("f.txt"), "x\n").expect("write file");
        run(&["add", "."], &main);
        run(
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.x",
                "commit",
                "-m",
                "init",
            ],
            &main,
        );

        let worktree = tmp.join("wt");
        run(
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "-b",
                "feature",
            ],
            &main,
        );

        let main_artifact = crate::search_index::artifact_cache_key(&main);
        let wt_artifact = crate::search_index::artifact_cache_key(&worktree);
        let main_scope = project_scope_key(&main);
        let wt_scope = project_scope_key(&worktree);

        // Same repo (same root commit) → SHARED artifact key (worktree reuses
        // main's on-disk index, read-only).
        assert_eq!(
            main_artifact, wt_artifact,
            "worktree must share the main checkout's artifact cache key"
        );
        // Distinct checkout path → DISTINCT scope key (no bash/undo/stats bleed).
        assert_ne!(
            main_scope, wt_scope,
            "worktree must get its own per-checkout scope key"
        );

        let _ = fs::remove_dir_all(&tmp);
    }
}
