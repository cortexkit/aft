use super::publication_fixture::{wait_until, PublicationFixture};
use super::*;
use crate::config::Config;
use crate::memory::profile::MemoryMode;

/// Route changes in a composed context that has pinned the fixture checkout.
#[doc(hidden)]
pub trait PublicationRouting {
    fn set_worker_only_routes(&self, worker_only: bool);
    fn set_standing_artifact_exempt(&self, exempt: bool);
}

pub fn memory_live_change_applies_to_next_cold_build_admission(
    set_config: &dyn Fn(Config),
    reload: &dyn Fn(&Config, &Config) -> Config,
    restore: &dyn Fn(Option<MemoryMode>),
) {
    use crate::memory::profile::{self as memory_profile, MemoryMode};
    let original = memory_profile::current_effective();
    struct Restore<'a>(Option<MemoryMode>, &'a dyn Fn(Option<MemoryMode>));
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            (self.1)(self.0);
        }
    }
    let _restore = Restore(
        (original.source == "config").then_some(original.value),
        restore,
    );
    let published = crate::config::Config {
        memory: Some(MemoryMode::Balanced),
        ..Default::default()
    };
    set_config(published.clone());
    assert_eq!(Pool::Build.cap(), 2);
    assert_eq!(Pool::Fill.cap(), 2);
    let old_profile = memory_profile::current();
    let mut state = PoolState::new();
    state.queue.push_back(Box::new(|| {}));
    state.queue.push_back(Box::new(|| {}));
    assert!(state.admit(Pool::Build.cap()).is_some());
    let candidate = crate::config::Config {
        memory: Some(MemoryMode::Lean),
        ..published.clone()
    };
    set_config(reload(&published, &candidate));
    assert_eq!(Pool::Build.cap(), 2);
    assert_eq!(Pool::Fill.cap(), 2);
    memory_profile::admitted(old_profile, || {
        assert_eq!(memory_profile::current().resolver_threads, None);
        assert_eq!(
            memory_profile::current().block_cache_bytes,
            256 * 1024 * 1024
        );
    });
    let job = state
        .admit(Pool::Build.cap())
        .expect("lean keeps the second cold-build slot available");
    let next = memory_profile::current();
    assert_eq!(
        next.resolver_threads,
        if cfg!(windows) { None } else { Some(4) }
    );
    assert_eq!(next.block_cache_bytes, 128 * 1024 * 1024);
    memory_profile::admitted(next, job);
    set_config(published);
    assert_eq!(Pool::Build.cap(), 2);
}

#[cfg(target_os = "linux")]
pub fn binding_many_roots_shares_waiting_threads_and_eviction_removes_deadlines(
    make_registration: &dyn Fn() -> Arc<dyn Fn() -> Option<u64>>,
) {
    use crate::shared_timer::Timer;
    const ROOTS: usize = 12;
    let mut fixtures = Vec::new();
    let mut managers = Vec::new();
    let mut works = Vec::new();
    for _ in 0..ROOTS {
        let fixture = PublicationFixture::new(QUIET, QUIET);
        fixture.handle.quiet.changed();
        fixture.handle.work.settle.changed();
        // Keep real retry registrations pending while all roots are bound.
        fixture.handle.work.retry.lock().unwrap().in_a_row = 10;
        fixture.handle.work.fill_retry.lock().unwrap().in_a_row = 10;
        fixture
            .handle
            .work
            .retry_build_later(&DataError::Cancelled, false);
        fixture
            .handle
            .work
            .retry_fill_later("retry registration test");
        let manager = make_registration();
        works.push(Arc::clone(&fixture.handle.work));
        managers.push(manager);
        fixtures.push(fixture);
    }
    // Initial checks run immediately, then register the actual idle deadline.
    wait_until(|| {
        managers.iter().all(|manager| {
            manager()
                .and_then(Timer::deadline_for_test)
                .is_some_and(|at| at > Instant::now() + Duration::from_secs(60))
        })
    });
    let mut registrations = Vec::new();
    for (fixture, manager) in fixtures.iter().zip(&managers) {
        for timer in [&fixture.handle.quiet, &fixture.handle.work.settle] {
            registrations.push(
                timer
                    .timer
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .registration_for_test(),
            );
        }
        for retry in [&fixture.handle.work.retry, &fixture.handle.work.fill_retry] {
            registrations.push(
                retry
                    .lock()
                    .unwrap()
                    .timer
                    .as_ref()
                    .unwrap()
                    .registration_for_test(),
            );
        }
        registrations.push(manager().unwrap());
    }
    wait_until(|| {
        registrations
            .iter()
            .all(|id| Timer::registered_for_test(*id))
    });
    let names = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|entry| {
            std::fs::read_to_string(entry.unwrap().path().join("comm")).unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let count = |name: &str| names.iter().filter(|comm| comm.trim_end() == name).count();
    // Linux comm truncates thread names to fifteen bytes.
    assert_eq!(
        count("aft-shared-time"),
        1,
        "one deadline service for every root"
    );
    assert_eq!(
        count("aft-dead-code-r"),
        2,
        "release workers grew with roots"
    );
    assert_eq!(count("aft-data-quiet"), 0);
    assert_eq!(count("aft-data-fill-r"), 0);
    assert_eq!(count("aft-data-build-"), 0);

    let quiet = Arc::downgrade(&fixtures[0].handle.quiet);
    let manager = Arc::downgrade(&managers[0]);
    drop(fixtures);
    drop(managers);
    wait_until(|| quiet.upgrade().is_none() && manager.upgrade().is_none());
    assert!(
        registrations
            .iter()
            .all(|id| !Timer::registered_for_test(*id)),
        "an evicted root left a timer registered"
    );
    // Even a pool job retaining Work cannot rearm an evicted checkout.
    for work in &works {
        work.retry_build_later(&DataError::Cancelled, false);
        work.retry_fill_later("evicted root");
        assert!(work.retry.lock().unwrap().timer.is_none());
        assert!(work.fill_retry.lock().unwrap().timer.is_none());
    }
}

pub fn a_head_joining_worker_only_routes_arms_pending_changes_and_leaving_cancels_them(
    make_context: &dyn Fn(&PublicationFixture) -> Box<dyn PublicationRouting>,
) {
    let quiet = Duration::from_millis(100);
    let fixture = PublicationFixture::new(quiet, quiet);
    fixture.handle.quiet.use_manual_clock_for_test();
    let ctx = make_context(&fixture);
    ctx.set_worker_only_routes(true);
    fixture.add("worker_pending", 1);
    ctx.set_worker_only_routes(false);
    fixture.handle.quiet.advance_for_test(quiet);
    wait_until(|| fixture.handle.checkout().changed_paths() == 0);
    wait_until(|| !fixture.handle.work.running());
    let before = fixture.refreshes();

    fixture.add("mixed_pending", 1);
    assert!(fixture.handle.quiet.armed.load(Ordering::Acquire));
    ctx.set_worker_only_routes(true);
    fixture.handle.quiet.advance_for_test(quiet);
    wait_until(|| !fixture.handle.work.running());
    assert_eq!(fixture.refreshes(), before);
    assert!(pending(fixture.handle.checkout()));
    assert!(!fixture.handle.quiet.armed.load(Ordering::Acquire));
}

pub fn an_admitted_refresh_finishes_after_routes_become_worker_only_without_rearming_quiet(
    make_context: &dyn Fn(&PublicationFixture) -> Box<dyn PublicationRouting>,
) {
    let quiet = Duration::from_millis(100);
    let fixture = PublicationFixture::new(quiet, quiet);
    let ctx = make_context(&fixture);
    let before = fixture.refreshes();
    fixture
        .handle
        .work
        .refresh_hold
        .store(true, Ordering::Release);
    let release = ClearOnDrop(Arc::clone(&fixture.handle.work.refresh_hold));
    fixture.add("admitted_edit", 1);
    wait_until(|| fixture.refreshes() > before);
    ctx.set_worker_only_routes(true);
    assert!(pending(fixture.handle.checkout()));
    drop(release);
    wait_until(|| !pending(fixture.handle.checkout()) && !fixture.handle.work.running());
    assert_eq!(fixture.refreshes(), before + 1);

    fixture.add("worker_after_admission", 1);
    std::thread::sleep(quiet * 3);
    assert_eq!(fixture.refreshes(), before + 1);
    assert!(pending(fixture.handle.checkout()));
    assert!(!fixture.handle.quiet.armed.load(Ordering::Acquire));
}

pub fn standing_roots_keep_quiet_refresh_with_worker_only_routes(
    make_context: &dyn Fn(&PublicationFixture) -> Box<dyn PublicationRouting>,
) {
    let quiet = Duration::from_millis(100);
    let fixture = PublicationFixture::new(quiet, quiet);
    let ctx = make_context(&fixture);
    ctx.set_worker_only_routes(true);
    ctx.set_standing_artifact_exempt(true);
    fixture.add("standing_edit", 1);
    wait_until(|| fixture.handle.checkout().changed_paths() == 0);
    wait_until(|| !fixture.handle.work.running());
    assert_eq!(fixture.publications(), 1);
    ctx.set_standing_artifact_exempt(false);
    let before = fixture.refreshes();
    fixture.add("no_longer_standing", 1);
    std::thread::sleep(quiet * 3);
    assert_eq!(fixture.refreshes(), before);
    assert!(pending(fixture.handle.checkout()));
}

pub fn worker_only_publication_thresholds_do_not_schedule_follow_up_refreshes(
    make_context: &dyn Fn(&PublicationFixture) -> Box<dyn PublicationRouting>,
) {
    let quiet = Duration::from_millis(100);
    let fixture = PublicationFixture::new(quiet, quiet);
    let ctx = make_context(&fixture);
    ctx.set_worker_only_routes(true);
    fixture.add("threshold", PUBLISH_AT_REFRESHED);
    fixture.refresh();
    assert!(fixture.handle.checkout().changed_paths() >= PUBLISH_AT);
    assert!(fixture.handle.checkout().refreshed_files() >= PUBLISH_AT_REFRESHED);
    assert!(!fixture.handle.work.settle.armed.load(Ordering::Acquire));
    let before = fixture.refreshes();
    fixture.add("after_threshold", 1);
    std::thread::sleep(quiet * 3);
    assert_eq!(fixture.refreshes(), before);
    assert!(pending(fixture.handle.checkout()));
}

pub fn worker_only_rescans_refresh_only_when_a_bind_requests_them(
    make_context: &dyn Fn(&PublicationFixture) -> Box<dyn PublicationRouting>,
) {
    let quiet = Duration::from_millis(100);
    let fixture = PublicationFixture::new(quiet, quiet);
    let ctx = make_context(&fixture);
    ctx.set_worker_only_routes(true);
    let before = fixture.refreshes();
    std::fs::write(
        fixture.handle.root().join("src/lib.rs"),
        "pub fn changed_without_a_watcher_event() {}\n",
    )
    .unwrap();
    fixture.handle.rescan();
    wait_until(|| !fixture.handle.work.rescanning.load(Ordering::Acquire));
    std::thread::sleep(quiet * 3);
    assert_eq!(fixture.refreshes(), before);
    assert!(pending(fixture.handle.checkout()));

    fixture.handle.work.start_rescan(true);
    wait_until(|| !pending(fixture.handle.checkout()) && !fixture.handle.work.running());
    assert_eq!(fixture.refreshes(), before + 1);
    assert!(!pending(fixture.handle.checkout()));
}

pub fn admitted_work_runs_off_the_executor_and_blocks_no_other_work(
    setup: impl FnOnce(
        &Path,
    ) -> (
        RootMaintenanceExecutor,
        Box<dyn Fn() -> crate::executor::CompletionHandle>,
    ),
) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (maintenance, submit_read) = setup(&root);
    let runner = Runner {
        root: root.clone(),
        executor: parking_lot::Mutex::new(Some(maintenance)),
    };

    // Fill every place in the fill pool with rounds that wait until the
    // test lets them go.
    let release = Arc::new(std::sync::Barrier::new(Pool::Fill.cap() + 1));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    for _ in 0..Pool::Fill.cap() {
        let release = Arc::clone(&release);
        let started = started_tx.clone();
        runner.run(FILL_REQUEST, Pool::Fill, move || {
            started
                .send(crate::executor::current_thread_is_executor_worker())
                .unwrap();
            release.wait();
        });
    }
    for _ in 0..Pool::Fill.cap() {
        let on_worker = started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!on_worker, "a fill round ran on an executor worker");
    }

    // A build still starts while every fill is stuck.
    let (built_tx, built_rx) = std::sync::mpsc::channel();
    runner.run(BUILD_REQUEST, Pool::Build, move || {
        built_tx
            .send(crate::executor::current_thread_is_executor_worker())
            .unwrap();
    });
    let on_worker = built_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("a build waited behind stuck fill rounds");
    assert!(!on_worker, "a build ran on an executor worker");

    // And a tool call on the same root runs at once.
    let started = Instant::now();
    let response = submit_read()
        .recv_timeout(Duration::from_secs(10))
        .expect("a read on the root never ran");
    assert!(response.success);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a read on the root waited {:?} behind its background work",
        started.elapsed()
    );
    release.wait();
}

pub fn an_unbound_root_builds_nothing_until_it_is_bound(
    setup: impl FnOnce(&Path) -> (RootMaintenanceExecutor, Box<dyn Fn(bool)>),
) {
    let dir = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "mod util;\npub fn caller() { util::callee(); }\n",
    )
    .unwrap();
    std::fs::write(root.join("src/util.rs"), "pub fn callee() {}\n").unwrap();

    let (maintenance, set_bound) = setup(&root);
    set_bound(false);

    let handle = DataCheckout::open(&root, storage.path(), Some(maintenance)).unwrap();
    // The executor refuses the build the bind asked for, and drops it
    // unrun: nothing is published, and nothing stays marked as running.
    let deadline = Instant::now() + Duration::from_secs(10);
    while handle.work.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!handle.work.running(), "refused work stayed marked running");
    std::thread::sleep(Duration::from_millis(500));
    assert!(handle.view().is_none(), "an unbound root was built");
    assert!(handle.checkout().needs_build());

    set_bound(true);
    handle.request_refresh();
    let view = match handle.fresh_view(Duration::from_secs(60)) {
        Fresh::Ready(view) => view,
        Fresh::Building => panic!("the call graph was never built"),
        Fresh::Failed(reason) => panic!("the build failed: {reason}"),
    };
    assert!(view.status().callgraph_published);
}

impl QuietTimer {
    pub(super) fn use_manual_clock_for_test(&self) {
        assert!(!self.armed.load(Ordering::Acquire));
        *self.manual_clock.lock() = Some(ManualQuietClock {
            now: self.epoch,
            pending: None,
        });
    }

    pub(super) fn advance_for_test(&self, delta: Duration) {
        let pending = {
            let mut clock = self.manual_clock.lock();
            let clock = clock.as_mut().expect("manual clock is enabled");
            clock.now += delta;
            clock.pending.take_if(|deadline| deadline.at <= clock.now)
        };
        let Some(mut deadline) = pending else { return };
        // The real deadline callback reads the clock again and can enqueue work.
        if let Some(at) = (deadline.fire)() {
            deadline.at = at;
            self.manual_clock.lock().as_mut().unwrap().pending = Some(deadline);
        }
    }
}
