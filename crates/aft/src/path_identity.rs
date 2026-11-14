pub use aft_base::path_identity::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {

    use super::*;
    use std::fs;

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

        let main_artifact = crate::root_identity::key::artifact_cache_key(&main);
        let wt_artifact = crate::root_identity::key::artifact_cache_key(&worktree);
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
