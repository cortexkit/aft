use super::*;

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
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
