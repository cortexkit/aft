use super::{git_policy_output, Path};

thread_local! {
    pub(super) static BENCH_UNBATCHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Keep the former per-key query schedule for the manual benchmark. Both
// schedules re-read the Git configuration with the same sandbox rules,
// filesystem inputs and cache validation.
pub(super) fn legacy_git_path_values(
    project_root: &Path,
    keys: &[&str],
) -> Result<Vec<Option<String>>, String> {
    keys.iter()
        .map(|key| {
            let extension = git_policy_output(
                project_root,
                &["config", "--local", "--bool", "extensions.worktreeConfig"],
            )
            .map_err(|error| format!("failed to query worktree config extension: {error}"))?;
            let worktree_config = extension.status.success() && extension.stdout == b"true\n";
            for scope in ["--worktree", "--local", "--global", "--system"] {
                if scope == "--worktree" && !worktree_config {
                    continue;
                }
                let output =
                    git_policy_output(project_root, &["config", scope, "--path", "--get", key])
                        .map_err(|error| format!("failed to query {key}: {error}"))?;
                if output.status.code() == Some(1) && output.stdout.is_empty() {
                    continue;
                }
                if !output.status.success() {
                    return Err(format!(
                        "failed to query {key}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value = String::from_utf8(output.stdout)
                    .map_err(|error| format!("{key} is not UTF-8: {error}"))?;
                if !value.trim().is_empty() {
                    return Ok(Some(value));
                }
            }
            Ok(None)
        })
        .collect()
}

pub fn ignore_cached_answers(ignore: bool) {
    super::GIT_POLICY_IGNORE_ANSWERS.with(|value| value.set(ignore));
}

pub fn use_unbatched(unbatched: bool) {
    BENCH_UNBATCHED.with(|value| value.set(unbatched));
}

pub fn evict(root: &Path) {
    let key =
        super::git_policy_cache_key(root, &super::git_policy_environment(root), true).unwrap();
    super::git_policy_cache().lock().unwrap().remove(&key);
}

pub fn counts(root: &Path) -> [usize; 4] {
    super::with_git_policy_test_state(root, |state| {
        [state.hits, state.misses, state.uncached, state.calls]
    })
    .unwrap()
}

pub fn git_output(root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    git_policy_output(root, args)
}
