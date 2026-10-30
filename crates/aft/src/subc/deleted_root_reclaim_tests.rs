use super::*;
use crate::bash_background::{registry::root_reclaim_tests::*, BgTaskStatus};

struct ReclaimFixture {
    _dir: tempfile::TempDir,
    storage: tempfile::TempDir,
    root: ProjectRootId,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    roots: HashMap<ProjectRootId, RootMeta>,
    metrics: DispatchPathMetrics,
    now: Instant,
}

impl ReclaimFixture {
    fn new() -> Self {
        let (dir, root) = test_support::test_root("deleted-root-reclaim");
        let ctx = test_support::test_ctx();
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        let now = Instant::now();
        let mut meta = RootMeta::new(now);
        // Two independent probes have already confirmed absence; each test
        // exercises the task cleanup sweeps that follow that confirmation.
        meta.consecutive_missing_sweeps = 2;
        let roots = HashMap::from([(root.clone(), meta)]);
        std::fs::remove_dir_all(root.as_path()).unwrap();
        let deadline = now + Duration::from_secs(5);
        while executor.dispatch_loop_liveness().has_pending_work() {
            assert!(Instant::now() < deadline, "registration did not settle");
            std::thread::yield_now();
        }
        Self {
            _dir: dir,
            storage: tempfile::tempdir().unwrap(),
            root,
            ctx,
            executor,
            roots,
            metrics: DispatchPathMetrics::new(),
            now,
        }
    }

    fn task(&self, id: &str, root: &Path, status: BgTaskStatus, pid: Option<u32>) {
        register_task(
            self.ctx.bash_background(),
            self.storage.path(),
            root,
            id,
            status,
            pid,
        );
    }

    fn sweep(&mut self) -> IdleReapOutcome {
        reap_idle_roots_with_presence(
            self.now,
            &mut self.roots,
            &HashMap::new(),
            &HashMap::new(),
            &self.executor,
            &self.metrics,
            &RootPresence::default(),
        )
    }

    fn wait_for_kill(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.roots[&self.root]
            .reclaim_kill_in_flight
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            assert!(Instant::now() < deadline, "reclaim kill did not complete");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn assert_reclaimed(&self, outcome: IdleReapOutcome) {
        assert_eq!(
            outcome.forgotten_deleted_roots,
            vec![self.root.clone()],
            "root retained: {}",
            self.metrics.reap_census_for_test()
        );
        assert!(!self.roots.contains_key(&self.root));
        assert!(self.executor.actor_context(&self.root).is_none());
    }
}

#[test]
fn deleted_root_terminal_survivor_is_killed_and_reclaimed_in_two_sweeps() {
    assert_terminal_survivor_reclaimed(false);
}

#[test]
fn deleted_root_terminal_group_with_no_sample_is_checked_before_reclaim() {
    assert_terminal_survivor_reclaimed(true);
}

fn assert_terminal_survivor_reclaimed(clear_sample: bool) {
    use std::os::unix::process::ExitStatusExt;

    let mut fixture = ReclaimFixture::new();
    let mut group = OwnedGroup::spawn();
    fixture.task(
        "bash-0000000000000001",
        fixture.root.as_path(),
        BgTaskStatus::Completed,
        Some(group.0.id()),
    );
    if clear_sample {
        clear_descendant_sample(fixture.ctx.bash_background(), "bash-0000000000000001");
    }
    assert_eq!(fixture.sweep().evicted, 0);
    fixture.wait_for_kill();
    // The worker settles group liveness, not this test's Child handle. Reap
    // the child to observe the signal rather than requiring try_wait to be
    // ready immediately after the worker clears its in-flight flag. The wait
    // is bounded so a group that survived fails the test instead of hanging it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = group.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "terminal task's owned group survived reclaim"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(
        matches!(status.signal(), Some(libc::SIGTERM) | Some(libc::SIGKILL)),
        "terminal task's owned group was not killed by a reclaim signal: {status}"
    );
    let outcome = fixture.sweep();
    fixture.assert_reclaimed(outcome);
}

#[test]
fn deleted_root_is_not_blocked_by_another_roots_running_task() {
    let mut fixture = ReclaimFixture::new();
    let other = tempfile::tempdir().unwrap();
    let mut group = OwnedGroup::spawn();
    fixture.task(
        "bash-0000000000000002",
        other.path(),
        BgTaskStatus::Running,
        Some(group.0.id()),
    );
    // Actor retirement detaches its registry. Keep the task itself so the
    // assertion observes its state rather than racing the registry teardown.
    let foreign = fixture
        .ctx
        .bash_background()
        .task_for_test("bash-0000000000000002")
        .unwrap();
    let outcome = fixture.sweep();
    fixture.assert_reclaimed(outcome);
    assert!(foreign.is_running());
    assert!(
        group.0.try_wait().unwrap().is_none(),
        "foreign task must not be signaled"
    );
    assert_eq!(kill_attempts(fixture.ctx.bash_background()), 0);
}

#[test]
fn deleted_root_unkillable_task_backs_off_counted_kill_threads() {
    let mut fixture = ReclaimFixture::new();
    fixture.task(
        "bash-0000000000000003",
        fixture.root.as_path(),
        BgTaskStatus::Running,
        None,
    );
    refuse_reclaim_kills(fixture.ctx.bash_background());
    for _ in 0..100 {
        assert_eq!(fixture.sweep().evicted, 0);
        fixture.wait_for_kill();
        fixture.now += Duration::from_millis(100);
    }
    assert_eq!(
        kill_attempts(fixture.ctx.bash_background()),
        1,
        "100 sweeps over 10 seconds must spawn only the initial kill thread"
    );
    assert_eq!(
        fixture.metrics.reap_census_for_test()["deleted_task_blockers"]["task_kill_backoff"],
        1
    );
    fixture.now += RECLAIM_KILL_RETRY;
    assert_eq!(fixture.sweep().evicted, 0);
    fixture.wait_for_kill();
    assert_eq!(
        kill_attempts(fixture.ctx.bash_background()),
        2,
        "retry once the backoff expires"
    );
    finish_task(fixture.ctx.bash_background(), "bash-0000000000000003");
    let outcome = fixture.sweep();
    fixture.assert_reclaimed(outcome);
}

#[test]
fn deleted_root_running_task_is_killed_before_reclaim() {
    let mut fixture = ReclaimFixture::new();
    let mut group = OwnedGroup::spawn();
    fixture.task(
        "bash-0000000000000004",
        fixture.root.as_path(),
        BgTaskStatus::Running,
        Some(group.0.id()),
    );
    assert_eq!(fixture.sweep().evicted, 0);
    assert_eq!(
        fixture.metrics.reap_census_for_test()["deleted_task_blockers"]["task_kill_in_flight"],
        1
    );
    assert!(fixture.roots.contains_key(&fixture.root));
    fixture.wait_for_kill();
    assert!(
        group.0.try_wait().unwrap().is_some(),
        "running task survived reclaim"
    );
    assert_eq!(
        fixture
            .ctx
            .bash_background()
            .observed_status("bash-0000000000000004", "reclaim", 0)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Killed
    );
    let outcome = fixture.sweep();
    fixture.assert_reclaimed(outcome);
}

#[test]
fn deleted_root_task_registry_contention_is_censused_without_kill_threads() {
    let mut fixture = ReclaimFixture::new();
    let ctx = Arc::clone(&fixture.ctx);
    assert_eq!(
        while_task_map_locked(ctx.bash_background(), || fixture.sweep()).evicted,
        0
    );
    assert_eq!(
        fixture.metrics.reap_census_for_test()["deleted_task_blockers"]["task_registry_busy"],
        1
    );
    assert_eq!(kill_attempts(ctx.bash_background()), 0);
    let outcome = fixture.sweep();
    fixture.assert_reclaimed(outcome);
}
