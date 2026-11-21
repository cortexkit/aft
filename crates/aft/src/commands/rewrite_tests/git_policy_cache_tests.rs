use crate::config::SandboxMode;
use crate::sandbox_spawn::{install_sandbox_launcher_for_test, with_worker_jail};
use aft_engine::test_bridge::bash_remote::git_policy_bench_support;
use aft_engine::test_bridge::bash_remote::git_policy_cache_support::{commit, git_ok, Fixture};
use aft_engine::test_bridge::bash_remote::BgTaskRegistryTestExt as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Measures worker bash start latency under the default relaxed worker
/// profile, alternating spawns that reuse the policy's stored Git answers
/// with spawns that ignore them and query Git as the builders did before.
/// Needs a separately built aft binary as the sandbox launcher.
#[test]
#[ignore = "manual Linux measurement; AFT_GIT_POLICY_BENCH_LAUNCHER names the aft binary"]
#[cfg(target_os = "linux")]
fn relaxed_worker_bash_spawn_latency() {
    crate::effective_path::initialize_process_path();
    let launcher = PathBuf::from(std::env::var_os("AFT_GIT_POLICY_BENCH_LAUNCHER").unwrap())
        .canonicalize()
        .unwrap();
    let spawns = std::env::var("AFT_GIT_POLICY_BENCH_SPAWNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(200);
    let fixture = Fixture::new();
    let storage = tempfile::tempdir().unwrap();
    let hooks = tempfile::tempdir().unwrap();
    fixture.hooks(hooks.path().to_str().unwrap());
    commit(fixture.root());
    let linked = tempfile::tempdir().unwrap();
    let mut worktrees = Vec::new();
    for index in 0..4 {
        let root = linked.path().join(format!("worker-{index}"));
        git_ok(
            fixture.root(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                root.to_str().unwrap(),
                "HEAD",
            ],
        );
        worktrees.push(root);
    }
    let root = worktrees[0].clone();
    fixture.register_root(&root);
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(storage.path().join("store"));
    config.github.shim = false;
    config.git.co_author = "off".into();
    config.bash.linux_scope = false;
    config.sandbox.profile.worker = SandboxMode::Relaxed;
    let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let _launcher = install_sandbox_launcher_for_test(&root, launcher);
    let mut samples: [Vec<(f64, usize)>; 2] = [Vec::new(), Vec::new()];
    // Two warm-up spawns fill the cache in both modes before sampling.
    for index in 0..spawns * 2 + 2 {
        let ignore_answers = index % 2 == 0;
        git_policy_bench_support::ignore_cached_answers(ignore_answers);
        let before = git_policy_bench_support::counts(&root)[3];
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "spawn-latency", "command": "bash", "session_id": "spawn-latency",
            "params": {"command": "true", "compressed": false}
        }))
        .unwrap();
        let began = Instant::now();
        // Unlike the strict benchmark, the relaxed Linux profile writes no
        // marker files into account home directories, so no fixture home
        // is needed.
        let response = with_worker_jail(true, || crate::commands::bash::handle(&request, &ctx));
        let elapsed = began.elapsed().as_secs_f64() * 1000.0;
        let calls = git_policy_bench_support::counts(&root)[3] - before;
        assert!(response.success, "{response:?}");
        let id = response.data["task_id"].as_str().unwrap().to_string();
        while !ctx.bash_background().is_task_terminal(&id) {
            ctx.bash_background()
                .status(&id, "spawn-latency", None, None, 1024)
                .unwrap();
            assert!(began.elapsed() < Duration::from_secs(15));
            std::thread::sleep(Duration::from_millis(1));
        }
        if index >= 2 {
            samples[usize::from(!ignore_answers)].push((elapsed, calls));
        }
    }
    git_policy_bench_support::ignore_cached_answers(false);
    for (label, samples) in [("base", &samples[0]), ("answers", &samples[1])] {
        let mut times = samples.iter().map(|(ms, _)| *ms).collect::<Vec<_>>();
        times.sort_by(f64::total_cmp);
        let percentile = |p: usize| times[(times.len() * p / 100).min(times.len() - 1)];
        let helpers = samples.iter().map(|(_, calls)| *calls).sum::<usize>();
        eprintln!(
            "relaxed_spawn mode={label} spawns={} p50_ms={:.3} p90_ms={:.3} git_helpers_per_spawn={:.2}",
            times.len(),
            percentile(50),
            percentile(90),
            helpers as f64 / times.len() as f64
        );
    }
    assert!(samples[1].iter().all(|(_, calls)| *calls == 0));
}

/// Measures real worker bash starts under the strict sandbox profile. That
/// profile launches each command through the aft binary, so the test needs
/// a separately built one. All repositories, linked worktrees and task
/// storage are disposable.
#[test]
#[ignore = "manual Linux measurement; AFT_GIT_POLICY_BENCH_LAUNCHER names the aft binary"]
#[cfg(target_os = "linux")]
fn worker_bash_git_policy_cost() {
    crate::effective_path::initialize_process_path();
    let launcher = PathBuf::from(std::env::var_os("AFT_GIT_POLICY_BENCH_LAUNCHER").unwrap())
        .canonicalize()
        .unwrap();
    let fixture = Fixture::new();
    let storage = tempfile::tempdir().unwrap();
    let account = tempfile::tempdir().unwrap();
    let hooks = tempfile::tempdir().unwrap();
    // Landlock rules only add access, so a hooks folder inside a worktree
    // the command may write cannot be made read-only. Keep hooks outside.
    fixture.hooks(hooks.path().to_str().unwrap());
    let commit = git_policy_bench_support::git_output(
        fixture.root(),
        &[
            "-c",
            "user.name=Policy Bench",
            "-c",
            "user.email=bench@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ],
    )
    .unwrap();
    assert!(commit.status.success(), "{commit:?}");
    let linked = tempfile::tempdir().unwrap();
    let mut roots = vec![(fixture.root().clone(), 10)];
    for index in 0..4 {
        let root = linked.path().join(format!("worker-{index}"));
        let output = git_policy_bench_support::git_output(
            fixture.root(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                root.to_str().unwrap(),
                "HEAD",
            ],
        )
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        roots.push((root, 2));
    }
    for (label, unbatched) in [("before", true), ("after", false)] {
        git_policy_bench_support::use_unbatched(unbatched);
        for (root_index, (root, starts)) in roots.iter().enumerate() {
            fixture.register_root(root);
            git_policy_bench_support::evict(root);
            let mut config = crate::config::Config::default();
            config.project_root = Some(root.clone());
            config.storage_dir = Some(storage.path().join(format!("store-{label}-{root_index}")));
            config.github.shim = false;
            config.git.co_author = "off".into();
            config.bash.linux_scope = false;
            config.sandbox.profile.head = SandboxMode::Strict;
            config.sandbox.profile.worker = SandboxMode::Strict;
            let ctx =
                crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
            let _launcher = install_sandbox_launcher_for_test(root, launcher.clone());
            for start in 0..*starts {
                let before = git_policy_bench_support::counts(root);
                let request: crate::protocol::RawRequest =
                    serde_json::from_value(serde_json::json!({
                        "id": "policy-cost", "command": "bash", "session_id": "policy-cost",
                        "params": {"command": "printf git-policy-bench", "compressed": false}
                    }))
                    .unwrap();
                let began = Instant::now();
                let response = crate::worker_jail_canary::with_test_preparation_home(
                    account.path(),
                    1,
                    || with_worker_jail(true, || crate::commands::bash::handle(&request, &ctx)),
                );
                let elapsed = began.elapsed();
                assert!(response.success, "{response:?}");
                let id = response.data["task_id"].as_str().unwrap();
                while !ctx.bash_background().is_task_terminal(id) {
                    ctx.bash_background()
                        .status(id, "policy-cost", None, None, 1024)
                        .unwrap();
                    assert!(began.elapsed() < Duration::from_secs(15));
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert_eq!(
                    ctx.bash_background()
                        .status(id, "policy-cost", None, None, 1024)
                        .unwrap()
                        .exit_code,
                    Some(0)
                );
                let after = git_policy_bench_support::counts(root);
                let delta = std::array::from_fn::<_, 4, _>(|index| after[index] - before[index]);
                assert_eq!(delta[1], usize::from(start == 0));
                assert_eq!(delta[2], 0);
                eprintln!("worker_bash phase={label} root={root_index} start={start} hits={} misses={} key_none={} git_helpers={} startup_ms={:.3}", delta[0], delta[1], delta[2], delta[3], elapsed.as_secs_f64() * 1000.0);
            }
        }
    }
    git_policy_bench_support::use_unbatched(false);
}
