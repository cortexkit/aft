use super::*;
use crate::gh_shim::ticket_repository_snapshot;

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    crate::test_env::apply_hermetic_git_env(&mut command);
    let output = command
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run fixture git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    git(
        root,
        &[
            "-c",
            "user.name=Snapshot Test",
            "-c",
            "user.email=snapshot@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
}

#[cfg(unix)]
fn thread_cpu() -> std::time::Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // The thread clock excludes the CPU used by git children and other threads.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
        0
    );
    std::time::Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

#[cfg(unix)]
#[test]
#[ignore = "explicit wall-time and calling-thread CPU measurement"]
fn ticket_repository_snapshot_cost() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let worktree = temp.path().join("linked");
    repository(&repo);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-qb",
            "linked",
            worktree.to_str().unwrap(),
        ],
    );
    for (label, root) in [("checkout", &repo), ("linked_worktree", &worktree)] {
        assert_eq!(
            ticket_repository_snapshot(root).as_deref(),
            Some("owner/repo")
        );
        let count = 100;
        let cpu = thread_cpu();
        let wall = std::time::Instant::now();
        for _ in 0..count {
            assert_eq!(
                ticket_repository_snapshot(root).as_deref(),
                Some("owner/repo")
            );
        }
        let elapsed = wall.elapsed();
        let cpu = thread_cpu() - cpu;
        println!(
            "snapshot_cost os={} kind={label} calls={count} wall_us_per_call={:.3} thread_cpu_us_per_call={:.3}",
            std::env::consts::OS,
            elapsed.as_secs_f64() * 1e6 / f64::from(count),
            cpu.as_secs_f64() * 1e6 / f64::from(count),
        );
    }
}

fn assert_snapshot(cache: &Mutex<Cache>, root: &Path, expected: Option<&str>, spawns: usize) {
    SPAWNS.with(|count| count.set(0));
    let actual = snapshot_with_cache(root, cache);
    assert_eq!(actual.as_deref(), expected);
    assert_eq!(SPAWNS.with(std::cell::Cell::get), spawns, "git spawn count");
    let fresh = uncached(root, &LookupEnv::read().unwrap(), &mut None);
    assert_eq!(actual, fresh, "cached and uncached bindings differ");
}

#[test]
fn ticket_repository_snapshot_reuses_first_answer_and_canonical_root_without_spawning() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    repository(temp.path());
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 0);
    assert_snapshot(&cache, &temp.path().join("."), Some("owner/repo"), 0);
}

#[test]
fn ticket_repository_snapshot_invalidates_origin_rewrite_and_caches_no_origin() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    repository(temp.path());
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
    git(
        temp.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:new-owner/new-repo.git",
        ],
    );
    assert_snapshot(&cache, temp.path(), Some("new-owner/new-repo"), 2);
    assert_snapshot(&cache, temp.path(), Some("new-owner/new-repo"), 0);
    git(temp.path(), &["remote", "remove", "origin"]);
    assert_snapshot(&cache, temp.path(), None, 2);
    assert_snapshot(&cache, temp.path(), None, 0);
    git(
        temp.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/restored/repo.git",
        ],
    );
    assert_snapshot(&cache, temp.path(), Some("restored/repo"), 2);
}

#[test]
fn ticket_repository_snapshot_invalidates_replaced_dot_git_directory() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let other = temp.path().join("other");
    repository(&repo);
    repository(&other);
    git(
        &other,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/other/repo.git",
        ],
    );
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, &repo, Some("owner/repo"), 2);
    fs::rename(repo.join(".git"), temp.path().join("saved-git")).unwrap();
    fs::rename(other.join(".git"), repo.join(".git")).unwrap();
    assert_snapshot(&cache, &repo, Some("other/repo"), 2);
    assert_snapshot(&cache, &repo, Some("other/repo"), 0);
}

#[test]
fn ticket_repository_snapshot_worktree_uses_common_config_and_rechecks_git_file() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let linked = temp.path().join("linked");
    let other = temp.path().join("other");
    repository(&repo);
    repository(&other);
    git(
        &repo,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    git(&repo, &["config", "extensions.worktreeConfig", "true"]);
    git(
        &linked,
        &[
            "config",
            "--worktree",
            "remote.origin.url",
            "https://github.com/override/repo.git",
        ],
    );
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, &linked, Some("owner/repo"), 2);
    assert_snapshot(&cache, &linked, Some("owner/repo"), 0);
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/common/changed.git",
        ],
    );
    assert_snapshot(&cache, &linked, Some("common/changed"), 2);
    assert_snapshot(&cache, &linked, Some("common/changed"), 0);

    let replacement = temp.path().join("replacement-git-file");
    fs::write(
        &replacement,
        format!("gitdir: {}\n", other.join(".git").display()),
    )
    .unwrap();
    fs::remove_file(linked.join(".git")).unwrap();
    fs::rename(replacement, linked.join(".git")).unwrap();
    assert_snapshot(&cache, &linked, Some("owner/repo"), 2);
    assert_snapshot(&cache, &linked, Some("owner/repo"), 0);
}

#[test]
fn ticket_repository_snapshot_rechecks_worktree_commondir_and_gitdir() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let linked = temp.path().join("linked");
    let other = temp.path().join("other");
    repository(&repo);
    repository(&other);
    git(
        &other,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/other/repo.git",
        ],
    );
    git(
        &repo,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, &linked, Some("owner/repo"), 2);
    let inputs = Inputs::read(&linked, &repo.join(".git")).unwrap();
    let dot_git_before = stamp(&linked.join(".git"));

    // Preserve the commondir file itself while replacing its parent directory.
    let saved = inputs.git_dir.with_file_name("saved-gitdir");
    fs::rename(&inputs.git_dir, &saved).unwrap();
    fs::create_dir(&inputs.git_dir).unwrap();
    for entry in fs::read_dir(&saved).unwrap() {
        let entry = entry.unwrap();
        fs::rename(entry.path(), inputs.git_dir.join(entry.file_name())).unwrap();
    }
    assert_eq!(
        optional_stamp(&inputs.common_dir_pointer).unwrap(),
        inputs.common_dir_pointer_stamp
    );
    assert_ne!(stamp(&inputs.git_dir).as_ref(), Some(&inputs.git_dir_stamp));
    assert_snapshot(&cache, &linked, Some("owner/repo"), 2);
    assert_snapshot(&cache, &linked, Some("owner/repo"), 0);

    fs::write(
        &inputs.common_dir_pointer,
        format!("{}\n", other.join(".git").display()),
    )
    .unwrap();
    assert_eq!(stamp(&linked.join(".git")), dot_git_before);
    assert_snapshot(&cache, &linked, Some("other/repo"), 2);
    assert_snapshot(&cache, &linked, Some("other/repo"), 0);
}

#[test]
fn ticket_repository_snapshot_without_dot_git_never_caches_parent_discovery() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    repository(&repo);
    let child = repo.join("child");
    fs::create_dir(&child).unwrap();
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, &child, Some("owner/repo"), 2);
    assert_snapshot(&cache, &child, Some("owner/repo"), 2);
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/parent/changed.git",
        ],
    );
    assert_snapshot(&cache, &child, Some("parent/changed"), 2);
    assert_snapshot(&cache, temp.path(), None, 1);
    assert_snapshot(&cache, temp.path(), None, 1);
    assert!(cache.lock().unwrap().0.is_empty());
}

#[test]
fn ticket_repository_snapshot_rechecks_after_config_stat_error() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    repository(temp.path());
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
    fs::remove_file(temp.path().join(".git/config")).unwrap();
    assert_snapshot(&cache, temp.path(), None, 2);
    assert_snapshot(&cache, temp.path(), None, 2);
    git(
        temp.path(),
        &[
            "config",
            "remote.origin.url",
            "https://github.com/restored/repo.git",
        ],
    );
    assert_snapshot(&cache, temp.path(), Some("restored/repo"), 2);
    assert_snapshot(&cache, temp.path(), Some("restored/repo"), 0);
}

#[test]
fn ticket_repository_snapshot_rechecks_exact_path_changes() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    repository(temp.path());
    let cache = Mutex::new(Cache::default());
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 0);
    let path = std::env::var_os("PATH").unwrap();
    let paths =
        std::iter::once(temp.path().join("no-git-here")).chain(std::env::split_paths(&path));
    let path = std::env::join_paths(paths).unwrap();
    {
        let _path =
            crate::gh_shim::tests::ScopedTestEnvVar::set("PATH", Some(path.to_str().unwrap()));
        assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
        assert_snapshot(&cache, temp.path(), Some("owner/repo"), 0);
    }
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
    {
        let _path = crate::gh_shim::tests::ScopedTestEnvVar::set("PATH", Some(""));
        assert_snapshot(&cache, temp.path(), None, 0);
    }
    assert_snapshot(&cache, temp.path(), Some("owner/repo"), 2);
}

#[test]
fn ticket_repository_snapshot_cache_evicts_least_recently_used_root_at_256() {
    let _lock = crate::test_env::process_env_lock();
    let temp = tempfile::tempdir().unwrap();
    repository(temp.path());
    let dot_git = stamp(&temp.path().join(".git")).unwrap();
    let env = LookupEnv::read().unwrap();
    let entry = |n| Entry {
        root: PathBuf::from(format!("root-{n}")),
        dot_git: dot_git.clone(),
        inputs: Inputs::read(temp.path(), &temp.path().join(".git")).unwrap(),
        env: env.clone(),
        value: Some("owner/repo".into()),
    };
    let mut cache = Cache::default();
    for n in 0..256 {
        cache.insert(entry(n));
    }
    assert_eq!(cache.0.len(), 256);
    assert_eq!(
        cache.get(Path::new("root-0"), &dot_git, &env),
        Some(Some("owner/repo".into()))
    );
    cache.insert(entry(256));
    assert_eq!(cache.0.len(), 256);
    assert!(cache.get(Path::new("root-1"), &dot_git, &env).is_none());
    assert!(cache.get(Path::new("root-0"), &dot_git, &env).is_some());
    assert!(cache.get(Path::new("root-256"), &dot_git, &env).is_some());
}
