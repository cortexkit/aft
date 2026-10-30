use super::super::persistence::{allocate_task_layout, work_counts, write_task_at};
use super::*;

fn fixture(
    storage: &Path,
    session: &str,
    delivered: bool,
    days: u64,
) -> (PersistedTask, TaskPaths) {
    let resolved = allocate_task_layout(storage, session).unwrap();
    let mut metadata = PersistedTask::starting(
        resolved.paths.task_id.clone(),
        session.into(),
        "watchdog-fixture".into(),
        storage.join("deleted-workdir"),
        Some(storage.join("deleted-root")),
        None,
        true,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    metadata.completion_delivered = delivered;
    metadata.finished_at = Some(unix_millis() - days * 24 * 60 * 60 * 1000);
    if days > 0 {
        metadata.started_at = metadata.finished_at.unwrap().saturating_sub(60_000);
        metadata.duration_ms = Some(60_000);
    }
    write_task_at(&resolved, &metadata).unwrap();
    for artifact in [
        &resolved.paths.stdout,
        &resolved.paths.stderr,
        &resolved.paths.exit,
    ] {
        fs::write(artifact, b"").unwrap();
    }
    filetime::set_file_mtime(
        &resolved.paths.json,
        filetime::FileTime::from_unix_time((unix_millis() / 1000) as i64 - 25 * 60 * 60, 0),
    )
    .unwrap();
    (metadata, resolved.paths)
}

#[test]
fn persisted_task_gc_preserves_readable_tasks_when_the_storage_floor_refuses_them() {
    let storage = tempfile::tempdir().unwrap();
    let task_storage = storage.path().join("opencode");
    let registry = BgTaskRegistry::default();
    let tasks = [
        fixture(&task_storage, "session", true, 30),
        fixture(&task_storage, "session", false, 30),
    ];
    let files: Vec<_> = tasks
        .iter()
        .flat_map(|(_, paths)| [&paths.json, &paths.stdout, &paths.stderr, &paths.exit])
        .map(|path| {
            (
                path.clone(),
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        })
        .collect();
    let store = crate::persisted_format::PersistedStore::BashTask;
    crate::reader_floor::raise(storage.path(), &[(store, store.supported() + 1)]).unwrap();
    let refusals = crate::reader_floor::prepare(storage.path());
    let floor = refusals
        .into_iter()
        .find(|refusal| refusal.store == store)
        .expect("the real storage floor must refuse this build");
    assert_eq!(
        registry.maybe_gc_persisted(&task_storage).unwrap(),
        0,
        "GC must neither retire nor collect readable tasks under a newer floor"
    );
    for (path, bytes, modified) in files {
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "task artifact changed: {}",
            path.display()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            modified,
            "task artifact was rewritten: {}",
            path.display()
        );
    }
    for (metadata, paths) in tasks {
        assert_eq!(
            crate::persisted_format::gate(
                store,
                &paths.json,
                &paths.json,
                Some(u64::from(metadata.schema_version))
            ),
            Err(floor.clone())
        );
        assert_eq!(
            crate::persisted_format::refusal_covering(store, &paths.json),
            Some(floor.clone())
        );
        let read = super::super::persistence::read_task(&paths.json).unwrap_err();
        assert_eq!(
            crate::persisted_format::UnsupportedPersistedFormat::from_io_error(&read),
            Some(&floor)
        );
    }
}

#[cfg(unix)]
#[test]
fn watchdog_cost_live_children_open_no_artifacts() {
    let storage = tempfile::tempdir().unwrap();
    let registries: Vec<_> = (0..30).map(|_| BgTaskRegistry::default()).collect();
    // Real directory-layout history, not a proxy counter for the task map.
    for index in 0..10_000 {
        let (metadata, paths) = fixture(storage.path(), &format!("root-{}", index % 30), true, 2);
        registries[index % 30]
            .insert_rehydrated_task(metadata, paths, true)
            .unwrap();
    }
    let mut tasks = Vec::new();
    for index in 0..40 {
        let child = std::process::Command::new("/bin/sleep")
            .arg("300")
            .spawn()
            .unwrap();
        let (mut metadata, paths) =
            fixture(storage.path(), &format!("root-{}", index % 30), false, 0);
        metadata.mark_running(child.id(), child.id() as i32);
        metadata.finished_at = None;
        metadata.duration_ms = None;
        write_task_at(
            &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
            &metadata,
        )
        .unwrap();
        let registry = &registries[index % 30];
        registry
            .insert_rehydrated_task(metadata, paths.clone(), false)
            .unwrap();
        let task = registry.task(&paths.task_id).unwrap();
        std::mem::swap(
            &mut task.state.lock().unwrap().runtime,
            &mut TaskRuntime::Piped(Some(child.into())),
        );
        tasks.push((index % 30, task));
    }
    work_counts::reset();
    for (root, task) in &tasks {
        registries[*root].poll_task(task).unwrap();
        registries[*root].reap_child(task);
    }
    let counts = work_counts::get();
    work_counts::reset();
    for registry in &registries {
        registry.cleanup_finished(Duration::from_secs(3600));
    }
    let cleanup = work_counts::get();
    let finished_remaining: usize = registries
        .iter()
        .map(|registry| registry.inner.tasks.lock().unwrap().len())
        .sum::<usize>()
        - 40;
    eprintln!(
        "one minute cleanup / 30 registries: {cleanup:?}; finished deleted={}",
        10_000 - finished_remaining
    );
    work_counts::reset();
    let gc_deleted = registries[0].maybe_gc_persisted(storage.path()).unwrap();
    let persisted_cleanup = work_counts::get();
    eprintln!("one minute shared persisted GC: {persisted_cleanup:?}; deleted={gc_deleted}");
    eprintln!("40 children / 30 registries / 10000 finished folders: per tick {counts:?}; per minute opens={} reads={} stats={}", counts.opens * 120, (counts.metadata_reads + counts.marker_reads) * 120, counts.stats * 120);
    for (_, task) in tasks {
        if let TaskRuntime::Piped(Some(mut child)) = std::mem::replace(
            &mut task.state.lock().unwrap().runtime,
            TaskRuntime::Piped(None),
        ) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
    assert_eq!(
        counts.opens, 0,
        "live child ticks reopened persisted artifacts"
    );
}

#[test]
fn watchdog_cost_cleanup_is_bounded_and_makes_progress() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    for _ in 0..300 {
        let (metadata, paths) = fixture(storage.path(), "session", true, 2);
        registry
            .insert_rehydrated_task(metadata, paths.clone(), true)
            .unwrap();
        *registry
            .task(&paths.task_id)
            .unwrap()
            .terminal_at
            .lock()
            .unwrap() = Some(Instant::now() - Duration::from_secs(7200));
    }
    registry.cleanup_finished(Duration::from_secs(3600));
    let remaining = registry.inner.tasks.lock().unwrap().len();
    assert!(
        remaining >= 172,
        "one cleanup scanned/deleted all history: {remaining} remain"
    );
    for _ in 0..6 {
        registry.cleanup_finished(Duration::from_secs(3600));
    }
    assert!(
        registry.inner.tasks.lock().unwrap().is_empty(),
        "cursor starved later entries"
    );
}

#[test]
fn watchdog_cost_persisted_gc_is_bounded_at_directory_iterator() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut paths = Vec::new();
    for _ in 0..600 {
        paths.push(fixture(storage.path(), "session", true, 2).1);
    }
    work_counts::reset();
    registry.maybe_gc_persisted(storage.path()).unwrap();
    let counts = work_counts::get();
    eprintln!("persisted GC first pass: {counts:?}");
    assert!(
        counts.gc_entries <= 128,
        "GC eagerly advanced the directory iterator: {} entries",
        counts.gc_entries
    );
    let remain = paths.iter().filter(|path| path.json.exists()).count();
    assert!(
        remain >= 472,
        "GC exhausted the directory before applying its budget: {remain}"
    );
    for _ in 0..12 {
        registry.maybe_gc_persisted(storage.path()).unwrap();
    }
    assert!(paths.iter().all(|path| !path.json.exists()));
}

/// Rewrites a fixture's metadata the way the daemon does (atomic replacement)
/// and backdates it past the modification grace.
fn rewrite_fixture(paths: &TaskPaths, update: impl FnOnce(&mut PersistedTask)) {
    let resolved = resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap();
    let mut metadata = read_task_at(&resolved).unwrap();
    update(&mut metadata);
    write_task_at(&resolved, &metadata).unwrap();
    filetime::set_file_mtime(
        &paths.json,
        filetime::FileTime::from_unix_time((unix_millis() / 1000) as i64 - 25 * 60 * 60, 0),
    )
    .unwrap();
}

/// A lap over a store where nothing is deletable must not be repeated on
/// every later lap: a task whose files are unchanged is skipped without
/// opening its folders or reading its metadata, and only the tasks that
/// changed are opened again.
#[test]
fn watchdog_cost_persisted_gc_reopens_only_changed_tasks() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut paths = Vec::new();
    for index in 0..300 {
        // Finished two days ago, result never delivered, worktree gone: kept
        // until delivered or seven days old.
        paths.push(fixture(storage.path(), &format!("session-{}", index % 3), false, 2).1);
    }
    // Three sessions of 100 tasks are 307 directory-iterator steps per lap,
    // three passes at 128 steps each; eight passes cover two whole laps.
    for _ in 0..8 {
        registry.maybe_gc_persisted(storage.path()).unwrap();
    }
    work_counts::reset();
    for _ in 0..8 {
        assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
    }
    let unchanged = work_counts::get();
    eprintln!("8 passes over 300 unchanged undeletable tasks: {unchanged:?}");
    assert_eq!(
        unchanged.metadata_reads, 0,
        "unchanged undeletable tasks were reread on every lap: {unchanged:?}"
    );
    // The only opens left are the walk's own: the root and each session
    // folder once per lap (at most three laps begun in eight passes), plus
    // one attempt per pass to open the absent quarantine root.
    assert!(
        unchanged.opens <= 3 * 4 + 8,
        "unchanged undeletable tasks were reopened on every lap: {unchanged:?}"
    );

    let changed = 5;
    for path in paths.iter().step_by(37).take(changed) {
        rewrite_fixture(path, |metadata| metadata.command.push_str(" # edited"));
    }
    work_counts::reset();
    // Three passes from anywhere in the lap reach every task at least once.
    for _ in 0..3 {
        assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
    }
    let after_change = work_counts::get();
    eprintln!("3 passes after {changed} rewrites: {after_change:?}");
    assert!(
        after_change.metadata_reads >= changed && after_change.metadata_reads <= 2 * changed,
        "expected only the {changed} rewritten tasks to be read again: {after_change:?}"
    );
    assert!(paths.iter().all(|path| path.json.exists()));
}

/// Running tasks whose recorded process is alive, and finished tasks that a
/// registry in this process still owns while it removes their scratch folder,
/// are kept on every lap. Later laps check that process or that ownership
/// instead of reopening and rereading each task, and judge a task again as
/// soon as the process exits or the ownership ends.
#[cfg(unix)]
#[test]
fn watchdog_cost_persisted_gc_skips_unchanged_running_and_scratch_owned_tasks() {
    const EACH: usize = 100;
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    // Spawned before the fixtures are written, so their start times are not
    // earlier than the process start (an older task start would read as a
    // recycled process id).
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let mut running = Vec::new();
    for _ in 0..EACH {
        let (_, paths) = fixture(storage.path(), "session-running", true, 0);
        rewrite_fixture(&paths, |metadata| {
            metadata.mark_running(child.id(), child.id() as i32);
            metadata.finished_at = None;
            metadata.duration_ms = None;
        });
        running.push(paths);
    }
    let mut owned = Vec::new();
    let mut owners = Vec::new();
    for index in 0..EACH {
        let (_, paths) = fixture(storage.path(), "session-owned", true, 2);
        rewrite_fixture(&paths, |metadata| {
            metadata.sandbox_temp_dir = Some(storage.path().join(format!("scratch-{index}")));
        });
        owners.push(DbWriteOrder::for_task("session-owned", &paths.task_id));
        owned.push(paths);
    }

    gc_until_lap(&registry, storage.path(), 1);
    work_counts::reset();
    for _ in 0..8 {
        assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
    }
    let unchanged = work_counts::get();
    eprintln!(
        "8 passes over {EACH} running and {EACH} scratch-owned unchanged tasks: {unchanged:?}"
    );
    assert_eq!(
        unchanged.metadata_reads, 0,
        "unchanged running or scratch-owned tasks were reread on every lap: {unchanged:?}"
    );
    assert_eq!(gc_health(storage.path()).remembered_tasks, 2 * EACH);
    assert!(running.iter().chain(&owned).all(|path| path.json.exists()));

    // The ownership ends: the finished, delivered tasks are deleted on the
    // next lap.
    drop(owners);
    let laps = gc_health(storage.path()).laps_completed;
    gc_until_lap(&registry, storage.path(), laps + 1);
    assert!(
        owned.iter().all(|path| !path.dir.exists()),
        "a remembered task outlived the end of its scratch ownership"
    );

    // The process exits: the running tasks are read and judged again.
    child.kill().unwrap();
    child.wait().unwrap();
    work_counts::reset();
    let laps = gc_health(storage.path()).laps_completed;
    gc_until_lap(&registry, storage.path(), laps + 1);
    let after_exit = work_counts::get();
    eprintln!("one lap after the recorded process exited: {after_exit:?}");
    assert!(
        after_exit.metadata_reads >= EACH,
        "running tasks were not judged again after their process exited: {after_exit:?}"
    );
}

fn gc_health(storage: &Path) -> crate::bash_background::PersistedGcHealth {
    super::super::gc_memo::persisted_gc_health_for(&canonicalized_path(&storage.join("bash-tasks")))
        .expect("sweep health recorded")
}

/// Runs sweep passes until `laps` laps over the store have completed.
fn gc_until_lap(registry: &BgTaskRegistry, storage: &Path, laps: u64) {
    for _ in 0..64 {
        registry.maybe_gc_persisted(storage).unwrap();
        if gc_health(storage).laps_completed >= laps {
            return;
        }
    }
    panic!("the sweep never completed lap {laps}");
}

#[test]
fn watchdog_cost_persisted_gc_selects_newly_eligible_history() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut paths = Vec::new();
    for _ in 0..4_000 {
        paths.push(fixture(storage.path(), "session", false, 2).1);
    }
    work_counts::reset();
    let start = Instant::now();
    registry.maybe_gc_persisted(storage.path()).unwrap();
    eprintln!(
        "4000-task cold invocation: {:?}, {:?}",
        start.elapsed(),
        work_counts::get()
    );
    gc_until_lap(&registry, storage.path(), 2);
    work_counts::reset();
    let start = Instant::now();
    registry.maybe_gc_persisted(storage.path()).unwrap();
    eprintln!(
        "4000-task warm invocation: {:?}, {:?}",
        start.elapsed(),
        work_counts::get()
    );
    for path in paths.iter().step_by(397).take(10) {
        rewrite_fixture(path, |metadata| metadata.completion_delivered = true);
    }
    work_counts::reset();
    let deleted = registry.maybe_gc_persisted(storage.path()).unwrap();
    let counts = work_counts::get();
    eprintln!("4000 tasks / 10 newly eligible: deleted={deleted}, {counts:?}");
    assert_eq!(
        deleted, 10,
        "newly eligible tasks waited behind unrelated history"
    );
    assert!(
        counts.metadata_reads <= 40,
        "unrelated metadata was reread: {counts:?}"
    );
    assert!(
        counts.gc_entries <= 128,
        "discovery exceeded its iterator budget: {counts:?}"
    );
}

fn persisted_gc_expiration_work(task_count: usize) -> work_counts::Counts {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let wall = SystemTime::now();
    let tick = Instant::now();
    let mut paths = Vec::new();
    for index in 0..task_count {
        let (_, path) = fixture(storage.path(), "session", true, 2);
        let modified = if index < 10 {
            wall - Duration::from_secs(23 * 60 * 60)
        } else {
            wall
        };
        filetime::set_file_mtime(&path.json, filetime::FileTime::from_system_time(modified))
            .unwrap();
        paths.push(path);
    }
    gc_until_lap(&registry, storage.path(), 1);
    registry
        .scheduled_gc_persisted(storage.path(), tick, || wall)
        .unwrap();
    work_counts::reset();
    let start = Instant::now();
    let sqlite_opens = crate::db::lifecycle::thread_counts::total_opens_on_this_thread(
        crate::db::lifecycle::SqliteStore::AftDb,
    );
    assert_eq!(
        registry
            .scheduled_gc_persisted(storage.path(), tick + Duration::from_secs(1), || wall
                + Duration::from_secs(2 * 60 * 60))
            .unwrap(),
        10
    );
    let counts = work_counts::get();
    assert_eq!(
        crate::db::lifecycle::thread_counts::total_opens_on_this_thread(
            crate::db::lifecycle::SqliteStore::AftDb
        ),
        sqlite_opens,
        "retention work opened an independent SQLite connection"
    );
    eprintln!(
        "{task_count} deadlines / 10 expired, validation paced: {:?}, {counts:?}",
        start.elapsed()
    );
    assert!(
        counts.gc_deadlines <= 11,
        "scanned future deadlines: {counts:?}"
    );
    assert_eq!(
        counts.gc_entries, 0,
        "validation ran before its next minute: {counts:?}"
    );
    assert!(
        counts.metadata_reads <= 40,
        "only ten due tasks may be opened: {counts:?}"
    );
    assert!(paths[..10].iter().all(|path| !path.dir.exists()));
    assert!(paths[10..].iter().all(|path| path.json.exists()));
    // The control only calibrates expiration work. Its complete small-store
    // lap is paced differently; exercise minute-slice resumption with history.
    if task_count == 10 {
        return counts;
    }
    work_counts::reset();
    registry
        .scheduled_gc_persisted(storage.path(), tick + Duration::from_secs(59), || wall)
        .unwrap();
    assert_eq!(work_counts::get().gc_entries, 0);
    work_counts::reset();
    registry
        .scheduled_gc_persisted(storage.path(), tick + Duration::from_secs(60), || wall)
        .unwrap();
    let entries = work_counts::get().gc_entries;
    assert!(
        entries > 0 && entries <= 128,
        "validation did not resume at its bounded minute rate: {entries}"
    );
    counts
}

#[test]
fn watchdog_cost_persisted_gc_deadlines_bypass_paced_validation() {
    let control = persisted_gc_expiration_work(10);
    let history = persisted_gc_expiration_work(4_000);
    // Windows bundle removal performs more identity checks than Unix. Measure
    // the deadline gate separately, then compare the same ten expirations with
    // and without retained history instead of assuming a platform's stat cost.
    let gate_work = |counts: work_counts::Counts| {
        (
            counts.opens - counts.gc_teardown_opens,
            counts.metadata_reads - counts.gc_teardown_metadata_reads,
            counts.stats - counts.gc_teardown_stats,
        )
    };
    let control_gate = gate_work(control);
    let history_gate = gate_work(history);
    eprintln!("deadline gate (opens, metadata reads, stats): control={control_gate:?}, history={history_gate:?}; total and teardown work: control={control:?}, history={history:?}");
    assert!(
        history_gate.0 <= control_gate.0 + 2,
        "deadline gate opens grew with retained history: {control_gate:?} -> {history_gate:?}"
    );
    assert!(history_gate.1 <= control_gate.1 + 2, "deadline gate metadata reads grew with retained history: {control_gate:?} -> {history_gate:?}");
    assert!(
        history_gate.2 <= control_gate.2 + 2,
        "deadline gate stats grew with retained history: {control_gate:?} -> {history_gate:?}"
    );
}

#[test]
fn watchdog_cost_persisted_gc_many_roots_share_one_store_sweep() {
    let storage = tempfile::tempdir().unwrap();
    for _ in 0..4_000 {
        fixture(storage.path(), "session", false, 2);
    }
    let registries: Vec<_> = (0..170).map(|_| BgTaskRegistry::default()).collect();
    let start = Instant::now();
    for registry in &registries {
        registry.request_persisted_gc(storage.path());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while registries
        .iter()
        .any(|registry| registry.persisted_gc_thread().is_none())
    {
        assert!(Instant::now() < deadline, "shared sweep did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    let runs: u64 = registries
        .iter()
        .map(|registry| registry.inner.persisted_gc_runs.load(Ordering::SeqCst))
        .sum();
    eprintln!(
        "170 roots / one 4000-task store: {runs} sweeps in {:?}",
        start.elapsed()
    );
    assert_eq!(runs, 1, "project roots multiplied shared-store work");
}

#[test]
fn gc_grace_rechecks_clock_after_mid_pass_ack() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (mut metadata, paths) = fixture(storage.path(), "session", false, 2);
    fs::write(&paths.stdout, b"saved output\n").unwrap();
    let pass_started = SystemTime::now();
    let rewritten_at = pass_started + Duration::from_secs(1);
    let examined_at = pass_started + Duration::from_secs(2);
    let write_paths = paths.clone();
    let mut clock_samples = Vec::new();
    let deleted = with_gc_after_start_for_test(
        move || {
            metadata.completion_delivered = true;
            let task = resolve_task_layout(&write_paths.session_dir, &write_paths.task_id).unwrap();
            write_task_at(&task, &metadata).unwrap();
            filetime::set_file_mtime(
                &write_paths.json,
                filetime::FileTime::from_system_time(rewritten_at),
            )
            .unwrap();
        },
        || {
            registry
                .gc_persisted_with_clock(
                    storage.path(),
                    || {
                        let now = if clock_samples.is_empty() {
                            pass_started
                        } else {
                            examined_at
                        };
                        clock_samples.push(now);
                        now
                    },
                    true,
                )
                .unwrap()
        },
    );
    assert_eq!(
        deleted, 0,
        "acknowledged output lost its modification grace"
    );
    assert!(paths.json.exists(), "the rewritten task bundle was deleted");
    assert_eq!(fs::read(&paths.stdout).unwrap(), b"saved output\n");
    // Keeping future timestamps alone would mask reuse of the pass-start
    // clock. Require a fresh sample after the acknowledgement as well.
    assert_eq!(clock_samples, [pass_started, examined_at]);
}

#[test]
fn gc_stamp_cache_revalidates_same_time_metadata_replacement() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, paths) = fixture(storage.path(), "session", false, 2);
    gc_until_lap(&registry, storage.path(), 1);
    let stamp = super::super::gc_memo::Stamp::of(&paths.json).unwrap();
    work_counts::reset();
    assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
    let unchanged = work_counts::get();
    assert_eq!(
        unchanged.metadata_reads, 0,
        "an unchanged task lost its cached verdict: {unchanged:?}"
    );
    assert_eq!(
        super::super::gc_memo::Stamp::of(&paths.json).unwrap(),
        stamp
    );

    // Keep size, modification time and Windows creation time identical, and
    // bypass writer notifications so only the replacement's identity can make
    // the cached verdict demand validation.
    let original = fs::read_to_string(&paths.json).unwrap();
    let rewritten = original.replacen("watchdog-fixture", "Watchdog-fixture", 1);
    assert_ne!(original, rewritten);
    assert_eq!(original.len(), rewritten.len());
    crate::file_identity::replace_keeping_times(&paths.json, rewritten.as_bytes());
    work_counts::reset();
    assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
    let changed = work_counts::get();
    eprintln!("cached metadata: unchanged={unchanged:?}; same-size/time replacement={changed:?}");
    assert!(
        changed.metadata_reads > 0,
        "a real metadata replacement bypassed validation: {changed:?}"
    );
    assert!(paths.json.exists());
    let task = resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap();
    assert_eq!(read_task_at(&task).unwrap().command, "Watchdog-fixture");
}

/// Remembering undeletable tasks must not delay or prevent deletion: a pass
/// that runs out of budget says so and the next resumes where it stopped,
/// deletable tasks are all removed within one lap, and a remembered task that
/// becomes deletable (its result is delivered) is removed on the next lap.
#[test]
fn watchdog_cost_persisted_gc_resumes_partial_pass_and_still_deletes() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut due = Vec::new();
    let mut kept = Vec::new();
    for index in 0..400 {
        if index % 2 == 0 {
            due.push(fixture(storage.path(), "session", true, 2).1);
        } else {
            kept.push(fixture(storage.path(), "session", false, 2).1);
        }
    }

    let first = registry.maybe_gc_persisted(storage.path()).unwrap();
    let health = gc_health(storage.path());
    assert!(
        health.last_pass_out_of_budget,
        "a pass that stopped at its budget did not say so: {health:?}"
    );
    assert!(health.last_pass_tasks < 128, "{health:?}");
    assert_eq!(health.last_pass_evaluated, health.last_pass_tasks);
    assert!(first > 0 && first < due.len(), "first pass deleted {first}");

    let second = registry.maybe_gc_persisted(storage.path()).unwrap();
    let health = gc_health(storage.path());
    assert!(second > 0, "the second pass made no progress");
    // The second pass resumed where the first stopped: it reached no task the
    // first pass had already judged and remembered.
    assert_eq!(health.last_pass_unchanged, 0, "{health:?}");
    assert_eq!(health.last_pass_evaluated, health.last_pass_tasks);

    gc_until_lap(&registry, storage.path(), 1);
    let health = gc_health(storage.path());
    assert!(!health.last_pass_out_of_budget, "{health:?}");
    assert!(health.last_error.is_none(), "{health:?}");
    assert_eq!(health.remembered_tasks, kept.len(), "{health:?}");
    assert!(
        due.iter().all(|path| !path.dir.exists()),
        "a delivered task survived a whole lap"
    );
    assert!(kept.iter().all(|path| path.json.exists()));

    // Deleting the delivered tasks changed their session folder, which is
    // part of every remembered verdict there, so the next lap judges the kept
    // tasks once more. The lap after that judges nothing.
    gc_until_lap(&registry, storage.path(), 2);
    for _ in 0..8 {
        registry.maybe_gc_persisted(storage.path()).unwrap();
        let health = gc_health(storage.path());
        assert_eq!(health.last_pass_evaluated, 0, "{health:?}");
        if health.laps_completed >= 3 {
            break;
        }
    }

    rewrite_fixture(&kept[0], |metadata| metadata.completion_delivered = true);
    let laps = gc_health(storage.path()).laps_completed;
    gc_until_lap(&registry, storage.path(), laps + 1);
    assert!(
        !kept[0].dir.exists(),
        "a remembered task stayed after its result was delivered"
    );
    assert!(kept[1..].iter().all(|path| path.json.exists()));
    assert_eq!(
        gc_health(storage.path()).remembered_tasks,
        kept.len() - 1,
        "the deleted task's verdict outlived it"
    );
}

/// A task store that exists but cannot be walked is reported on the health
/// surface; the pass must not look like a sweep that found nothing to do.
#[test]
fn watchdog_cost_unreadable_task_store_is_reported_not_treated_as_empty() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    fs::write(storage.path().join("bash-tasks"), b"not a folder").unwrap();
    assert_eq!(registry.maybe_gc_persisted(storage.path()).unwrap(), 0);
    let health = gc_health(storage.path());
    assert!(health.last_error.is_some(), "{health:?}");
    assert_eq!(health.laps_completed, 0, "{health:?}");
    assert!(crate::bash_background::persisted_gc_health().roots_failing >= 1);
}

/// A session that cannot be opened must not keep readable sessions from being swept.
#[cfg(unix)]
#[test]
fn watchdog_cost_unreadable_session_does_not_hide_remaining_tasks() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    struct RestorePermissions(PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ =
                fs::set_permissions(&self.0, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        }
    }

    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, readable) = fixture(storage.path(), "readable-session", true, 2);
    let (_, unreadable) = fixture(storage.path(), "unreadable-session", true, 2);
    let restore = RestorePermissions(unreadable.session_dir.clone());
    fs::set_permissions(
        &unreadable.session_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0),
    )
    .unwrap();

    registry.maybe_gc_persisted(storage.path()).unwrap();

    drop(restore);
    assert!(
        !readable.dir.exists(),
        "a due task in a readable session was not deleted"
    );
    assert!(
        unreadable.dir.exists(),
        "a task in the unreadable session should not have been visited"
    );
    let health = gc_health(storage.path());
    assert_eq!(health.last_pass_unreadable_sessions, 1, "{health:?}");
    assert_eq!(health.last_pass_directory_read_errors, 0, "{health:?}");
    assert!(
        health
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains(&unreadable.session_dir.display().to_string())),
        "the skipped session was not named in health: {health:?}"
    );
    assert_eq!(
        health.laps_completed, 0,
        "an incomplete scan looked clean: {health:?}"
    );
    assert!(
        !health.last_pass_out_of_budget,
        "the scan error is not a budget overrun: {health:?}"
    );
    assert!(crate::bash_background::persisted_gc_health().roots_failing >= 1);
}

/// A failed session read must not prevent stale verdicts in readable sessions
/// from being pruned at the end of the store walk.
#[cfg(unix)]
#[test]
fn watchdog_cost_unreadable_session_prunes_stale_readable_memo() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    struct RestorePermissions(PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ =
                fs::set_permissions(&self.0, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        }
    }

    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, readable) = fixture(storage.path(), "readable-session", false, 2);
    let (_, unreadable) = fixture(storage.path(), "unreadable-session", false, 2);
    gc_until_lap(&registry, storage.path(), 1);
    assert_eq!(gc_health(storage.path()).remembered_tasks, 2);

    fs::remove_dir_all(&readable.dir).unwrap();
    let restore = RestorePermissions(unreadable.session_dir.clone());
    fs::set_permissions(
        &unreadable.session_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0),
    )
    .unwrap();

    registry.maybe_gc_persisted(storage.path()).unwrap();

    drop(restore);
    let health = gc_health(storage.path());
    assert_eq!(
        health.remembered_tasks, 1,
        "the deleted readable task's verdict survived the failed lap: {health:?}"
    );
    assert_eq!(health.last_pass_unreadable_sessions, 1, "{health:?}");
    assert!(
        health
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains(&unreadable.session_dir.display().to_string())),
        "the unreadable session was not named in health: {health:?}"
    );
    assert_eq!(health.laps_completed, 1, "an incomplete scan looked clean");
    assert!(crate::bash_background::persisted_gc_health().roots_failing >= 1);
}

/// Abandonment depends on the worktree existing, not on the task's files, so
/// a remembered undelivered result is retired once its worktree is deleted.
#[test]
fn watchdog_cost_remembered_task_is_retired_when_its_worktree_goes() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let worktree = storage.path().join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    let (_, paths) = fixture(storage.path(), "session", false, 8);
    rewrite_fixture(&paths, |metadata| {
        metadata.project_root = Some(worktree.clone());
    });
    gc_until_lap(&registry, storage.path(), 1);
    assert!(
        paths.json.exists(),
        "a result whose worktree exists was retired"
    );
    assert_eq!(gc_health(storage.path()).remembered_tasks, 1);

    fs::remove_dir_all(&worktree).unwrap();
    registry.maybe_gc_persisted(storage.path()).unwrap();
    assert!(
        !paths.dir.exists(),
        "a remembered result outlived its deleted worktree"
    );
    assert_eq!(
        registry
            .try_health_counts()
            .unwrap()
            .retired_undelivered_completions,
        1
    );
}

#[test]
fn watchdog_cost_abandonment_requires_both_paths_missing_and_seven_days() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, abandoned) = fixture(storage.path(), "session", false, 8);
    let (_, recent) = fixture(storage.path(), "session", false, 6);
    let (mut metadata, workdir_live) = fixture(storage.path(), "session", false, 8);
    metadata.workdir = storage.path().to_path_buf();
    write_task_at(
        &resolve_task_layout(&workdir_live.session_dir, &workdir_live.task_id).unwrap(),
        &metadata,
    )
    .unwrap();
    filetime::set_file_mtime(
        &workdir_live.json,
        filetime::FileTime::from_unix_time((unix_millis() / 1000) as i64 - 25 * 3600, 0),
    )
    .unwrap();
    let (mut metadata, root_live) = fixture(storage.path(), "session", false, 8);
    metadata.project_root = Some(storage.path().to_path_buf());
    write_task_at(
        &resolve_task_layout(&root_live.session_dir, &root_live.task_id).unwrap(),
        &metadata,
    )
    .unwrap();
    filetime::set_file_mtime(
        &root_live.json,
        filetime::FileTime::from_unix_time((unix_millis() / 1000) as i64 - 25 * 3600, 0),
    )
    .unwrap();
    registry.maybe_gc_persisted(storage.path()).unwrap();
    assert!(
        !abandoned.json.exists(),
        "deleted-worktree completion retained forever"
    );
    assert!(recent.json.exists() && workdir_live.json.exists() && root_live.json.exists());
    assert_eq!(
        registry
            .try_health_counts()
            .unwrap()
            .retired_undelivered_completions,
        1
    );
}

/// The collector runs every few seconds, so a pass that retires nothing must
/// stay out of the log; a pass that retires something still reports it.
#[test]
fn watchdog_cost_retirement_is_logged_only_when_nonzero() {
    const LINE: &str = "undelivered completions of deleted worktrees";
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (_, idle_lines) = crate::logging::capture_log_lines(|| {
        registry.maybe_gc_persisted(storage.path()).unwrap();
    });
    assert!(
        !idle_lines.iter().any(|line| line.contains(LINE)),
        "a pass that retired nothing was logged: {idle_lines:?}"
    );

    let (_, abandoned) = fixture(storage.path(), "session", false, 8);
    let (_, lines) = crate::logging::capture_log_lines(|| {
        registry.maybe_gc_persisted(storage.path()).unwrap();
    });
    assert!(!abandoned.json.exists());
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&format!("retired 1 {LINE}"))),
        "a pass that retired a completion was not logged: {lines:?}"
    );
}

#[test]
fn watchdog_cost_scheduler_does_not_own_idle_registries() {
    let registry = BgTaskRegistry::default();
    let weak = Arc::downgrade(&registry.inner);
    registry.start_watchdog();
    drop(registry);
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak.strong_count() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        weak.strong_count(),
        0,
        "watchdog keeps an idle project registry alive forever"
    );
}

#[test]
fn watchdog_cost_status_poll_preserves_session_completion_delivery() {
    let storage = tempfile::tempdir().unwrap();
    for tool_response in [false, true] {
        for queued_before_poll in [false, true] {
            let ctx = crate::context::AppContext::new(
                crate::context::default_language_provider_factory(),
                crate::config::Config::default(),
            );
            let registry = ctx.bash_background();
            let (metadata, paths) = fixture(storage.path(), "origin", false, 0);
            registry
                .insert_rehydrated_task(metadata.clone(), paths.clone(), true)
                .unwrap();
            if queued_before_poll {
                registry.enqueue_completion_from_parts(&metadata, None, Some(&paths), false, None);
            }
            let data = serde_json::json!({"task_id": paths.task_id, "status": "completed", "exit_code": 0, "output_preview": "done"});
            let mut status = crate::protocol::Response::success("status", data.clone());
            if tool_response {
                crate::response_finalize::finalize_tool_response(
                    &mut status,
                    &mut String::new(),
                    &ctx,
                    "origin",
                    "bash_status",
                    true,
                );
            } else {
                crate::response_finalize::finalize_response_with_bg_completions(
                    &mut status,
                    &ctx,
                    "origin",
                    "bash_status",
                    true,
                );
            }
            assert!(status.data.get("bg_completions").is_none());
            assert!(
                !read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered,
                "a read-only terminal status poll acknowledged an undelivered completion"
            );
            let retention = Duration::from_secs(24 * 60 * 60);
            let cleanup_at = Instant::now() + retention + Duration::from_secs(1);
            registry.cleanup_finished_at(retention, cleanup_at);
            assert!(paths.json.exists(), "cleanup removed an undelivered result");
            // Terminal rendering can enqueue after a status snapshot is returned.
            if !queued_before_poll {
                registry.enqueue_completion_from_parts(&metadata, None, Some(&paths), false, None);
            }
            let mut foreign = crate::protocol::Response::success("foreign", serde_json::json!({}));
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut foreign,
                &ctx,
                "other",
                "echo",
                true,
            );
            assert!(foreign.data.get("bg_completions").is_none());
            let mut origin = crate::protocol::Response::success("origin", serde_json::json!({}));
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut origin,
                &ctx,
                "origin",
                "echo",
                true,
            );
            assert_eq!(origin.data["bg_completions"][0]["task_id"], paths.task_id);
            let mut watch = crate::protocol::Response::success("watch", data);
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut watch,
                &ctx,
                "origin",
                "bash_watch",
                true,
            );
            assert!(registry
                .drain_completions_for_session(Some("origin"))
                .is_empty());
            assert!(
                read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered
            );
            registry.cleanup_finished_at(retention, cleanup_at);
            assert!(
                !paths.dir.exists(),
                "cleanup retained a collected result past 24 hours"
            );
        }
    }
}

#[test]
fn watchdog_cost_terminal_reply_acknowledges_only_originating_session() {
    let storage = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config::default(),
    );
    let registry = ctx.bash_background();
    for harness in [
        Harness::Runner,
        Harness::Opencode,
        Harness::Pi,
        Harness::Mcp {
            client: "fixture".into(),
        },
    ] {
        registry.set_harness(harness);
        for command in ["bash_watch", "bash"] {
            let (metadata, paths) = fixture(storage.path(), "origin", false, 0);
            registry
                .insert_rehydrated_task(metadata, paths.clone(), true)
                .unwrap();
            let data = serde_json::json!({"task_id": paths.task_id, "status": "completed", "exit_code": 0, "output_preview": "done"});
            let mut foreign = crate::protocol::Response::success("foreign", data.clone());
            crate::response_finalize::finalize_response_with_bg_completions(
                &mut foreign,
                &ctx,
                "other",
                command,
                false,
            );
            assert!(
                !read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered
            );
            let mut text = "done".to_string();
            let mut reply = crate::protocol::Response::success("reply", data);
            crate::response_finalize::finalize_tool_response(
                &mut reply, &mut text, &ctx, "origin", command, false,
            );
            assert!(
                read_task_at(&resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap())
                    .unwrap()
                    .completion_delivered,
                "{command} returned a terminal result but left it undelivered"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn watchdog_cost_adopted_live_pid_uses_kernel_exit_and_delivers_once() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let (mut metadata, paths) = fixture(storage.path(), "session", false, 0);
    metadata.mark_running(child.id(), child.id() as i32);
    metadata.finished_at = None;
    metadata.duration_ms = None;
    write_task_at(
        &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
        &metadata,
    )
    .unwrap();
    registry
        .insert_rehydrated_task(metadata, paths.clone(), true)
        .unwrap();
    let task = registry.task(&paths.task_id).unwrap();
    work_counts::reset();
    for _ in 0..120 {
        registry.poll_task(&task).unwrap();
        registry.reap_child(&task);
    }
    let counts = work_counts::get();
    fs::write(&paths.exit, b"0").unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    registry.poll_task(&task).unwrap();
    registry.poll_task(&task).unwrap();
    assert_eq!(counts.opens, 0, "adopted live PID polled its marker");
    assert_eq!(
        registry
            .observed_status(&paths.task_id, "session", 0)
            .unwrap()
            .info
            .status,
        BgTaskStatus::Completed
    );
    assert_eq!(
        registry.drain_completions().len(),
        1,
        "exit was delivered more than once"
    );
}

#[test]
fn watchdog_cost_failed_cleanup_deletion_is_retried() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (metadata, paths) = fixture(storage.path(), "session", true, 2);
    registry
        .insert_rehydrated_task(metadata, paths.clone(), true)
        .unwrap();
    let hidden = paths.control_dir.join("hidden-metadata.json");
    fs::rename(&paths.json, &hidden).unwrap();
    registry.cleanup_finished(Duration::ZERO);
    assert!(
        registry.task(&paths.task_id).is_some(),
        "failed deletion discarded the only retry candidate"
    );
    fs::rename(hidden, &paths.json).unwrap();
    registry.cleanup_finished(Duration::ZERO);
    assert!(registry.task(&paths.task_id).is_none());
    assert!(!paths.dir.exists());
}

#[cfg(unix)]
#[test]
fn watchdog_cost_terminal_scratch_retry_does_not_reread_exit_marker() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (mut metadata, paths) = fixture(storage.path(), "session", false, 0);
    let scratch = storage.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    metadata.mode = BgMode::Pty;
    // Model a leader that exits while its process group still has descendants.
    // The test's own group stays alive until cleanup is explicitly unblocked.
    metadata.mark_running(std::process::id(), unsafe { libc::getpgrp() });
    metadata.finished_at = None;
    metadata.duration_ms = None;
    metadata.sandbox_temp_dir = Some(scratch.clone());
    write_task_at(
        &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
        &metadata,
    )
    .unwrap();
    fs::write(&paths.exit, b"0\n").unwrap();
    registry
        .insert_rehydrated_task(metadata, paths.clone(), true)
        .unwrap();
    let task = registry.task(&paths.task_id).unwrap();
    let mut awaiting_reader = HashMap::new();
    let mut pass = || {
        super::super::watchdog::run_pass(&registry, WatchdogPassCause::Tick, &mut awaiting_reader);
    };
    pass();
    assert!(
        !task.is_running(),
        "the written marker must settle the leader"
    );
    assert_eq!(
        registry.running_tasks().len(),
        1,
        "scratch still needs cleanup"
    );

    work_counts::reset();
    for _ in 0..20 {
        pass();
    }
    let counts = work_counts::get();
    assert_eq!(
        counts.marker_reads, 0,
        "a terminal cleanup retry re-read the marker"
    );
    assert_eq!(
        counts.metadata_reads, 0,
        "a terminal cleanup retry resolved the layout"
    );
    assert_eq!(counts.opens, 0);
    assert!(scratch.exists());
    assert_eq!(registry.drain_completions().len(), 1);

    // Once the process group is gone, the same watchdog candidate must still
    // settle scratch retention and retire, without observing the exit again.
    {
        let mut state = task.lock_state().unwrap();
        state.metadata.child_pid = None;
        state.metadata.pgid = None;
    }
    pass();
    assert!(registry.running_tasks().is_empty());
    assert!(
        scratch.exists(),
        "scratch must remain readable during retention"
    );
}

/// Opt-in plain-read census of the live task store; no DB or GC is involved.
#[cfg(unix)]
#[test]
#[ignore = "requires an explicitly supplied live task storage root"]
fn watchdog_cost_live_storage_marker_census() {
    let root = std::env::var_os("AFT_WATCHDOG_CENSUS_ROOT")
        .expect("supply the task store root explicitly");
    let mut running = Vec::new();
    for namespace in fs::read_dir(root).unwrap().flatten() {
        let Ok(sessions) = fs::read_dir(namespace.path().join("bash-tasks")) else {
            continue;
        };
        for session in sessions.flatten() {
            let Ok(folders) = fs::read_dir(session.path()) else {
                continue;
            };
            for folder in folders.flatten() {
                let path = folder.path().join("control/metadata.json");
                let Ok(metadata) = super::super::persistence::read_task(&path) else {
                    continue;
                };
                if metadata.status == BgTaskStatus::Running
                    && metadata
                        .child_pid
                        .is_some_and(|pid| is_recorded_process_alive(pid, metadata.started_at))
                {
                    if let Ok(resolved) = resolve_task_layout(&session.path(), &metadata.task_id) {
                        running.push((metadata.project_root, resolved.paths));
                    }
                }
            }
        }
    }
    let mut per_root = BTreeMap::new();
    work_counts::reset();
    for (root, paths) in &running {
        let _ = read_exit_marker(paths);
        *per_root.entry(root.clone()).or_insert(0usize) += 1;
    }
    let counts = work_counts::get();
    eprintln!("LIVE marker path: {} recorded-live tasks, {} roots, {counts:?}; per-root task counts {:?}; per-minute opens={} reads={} stats={}", running.len(), per_root.len(), per_root.values().collect::<Vec<_>>(), counts.opens*120, (counts.metadata_reads+counts.marker_reads)*120, counts.stats*120);
    assert!(!running.is_empty());
}

#[test]
fn watchdog_cost_persisted_gc_recurs_without_a_new_replay() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    registry.request_persisted_gc(storage.path());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let done = persisted_gc_slots()
            .lock()
            .unwrap()
            .get(&canonicalized_path(storage.path()))
            .is_some_and(|slot| !slot.running);
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "initial GC did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (_, paths) = fixture(storage.path(), "later-session", true, 2);
    persisted_gc_slots()
        .lock()
        .unwrap()
        .get_mut(&canonicalized_path(storage.path()))
        .unwrap()
        .last_finished = Some((
        Instant::now() - Duration::from_secs(61),
        "aft-bash-task-gc".into(),
    ));
    registry.request_recurring_persisted_gc();
    while paths.json.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !paths.json.exists(),
        "once-at-replay GC left later delivered bundles forever"
    );
}

#[test]
fn watchdog_cost_gc_recovers_unobserved_exit_in_an_unloaded_worktree() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (mut metadata, paths) = fixture(storage.path(), "session", false, 8);
    metadata.mark_running(999_999_999, 999_999_999);
    metadata.started_at = unix_millis() - 9 * 24 * 3600 * 1000;
    metadata.finished_at = None;
    write_task_at(
        &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
        &metadata,
    )
    .unwrap();
    fs::write(&paths.exit, b"0").unwrap();
    let old = filetime::FileTime::from_unix_time((unix_millis() / 1000) as i64 - 8 * 24 * 3600, 0);
    filetime::set_file_mtime(&paths.exit, old).unwrap();
    filetime::set_file_mtime(&paths.json, old).unwrap();
    registry.maybe_gc_persisted(storage.path()).unwrap();
    assert!(
        !paths.json.exists(),
        "a dead task with an exit marker stayed running forever"
    );
}

#[test]
fn watchdog_cost_inactive_harness_store_is_swept_without_binding_its_worktree() {
    let storage = tempfile::tempdir().unwrap();
    let runner = storage.path().join("runner");
    let opencode = storage.path().join("opencode");
    fs::create_dir_all(&runner).unwrap();
    let (_, paths) = fixture(&opencode, "old-session", true, 2);
    let registry = BgTaskRegistry::default();
    registry.request_persisted_gc(&runner);
    registry.request_recurring_persisted_gc();
    let deadline = Instant::now() + Duration::from_secs(30);
    while paths.json.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !paths.json.exists(),
        "inactive harness needed a deleted worktree to bind again before it could reap history"
    );
}

#[test]
fn watchdog_cost_terminal_reply_ack_fences_a_late_completion_snapshot() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let (snapshot, paths) = fixture(storage.path(), "session", false, 0);
    registry
        .insert_rehydrated_task(snapshot.clone(), paths.clone(), true)
        .unwrap();
    registry.ack_terminal_result_for_session(&paths.task_id, "session");
    // Rendering may finish after the caller has already received a terminal
    // watch reply. Its pre-ack snapshot must not restore a notification afterward.
    registry.enqueue_completion_from_parts(&snapshot, None, Some(&paths), false, None);
    assert!(
        registry.drain_completions().is_empty(),
        "a late completion snapshot undid the reply acknowledgment"
    );
}

#[cfg(unix)]
#[test]
fn watchdog_cost_adopted_exit_with_refused_marker_still_settles() {
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::default();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let (mut metadata, paths) = fixture(storage.path(), "session", false, 0);
    metadata.mark_running(child.id(), child.id() as i32);
    metadata.finished_at = None;
    metadata.duration_ms = None;
    write_task_at(
        &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
        &metadata,
    )
    .unwrap();
    registry
        .insert_rehydrated_task(metadata, paths.clone(), true)
        .unwrap();
    let task = registry.task(&paths.task_id).unwrap();
    registry.poll_task(&task).unwrap();
    fs::remove_file(&paths.exit).unwrap();
    fs::create_dir(&paths.exit).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        registry.poll_task(&task).is_err(),
        "a directory marker must be refused"
    );
    assert!(
        !task.is_running(),
        "a dead adopted process stayed running after its marker was refused"
    );
    assert_eq!(registry.drain_completions().len(), 1);
}

/// Poll the marker the way the watchdog does, returning the result and how
/// many full marker reads (the validated open-read-parse path) it took.
fn probe_once(
    paths: &TaskPaths,
    probe: &mut super::super::persistence::ExitMarkerProbe,
) -> (Option<super::super::persistence::ExitMarker>, usize) {
    work_counts::reset();
    let marker = super::super::persistence::poll_exit_marker(paths, probe).unwrap();
    (marker, work_counts::get().marker_reads)
}

/// Replace the marker by renaming a new file over it, the way atomic writers
/// publish it, so the path names a different file than the previous poll saw.
fn replace_marker(paths: &TaskPaths, content: &[u8]) {
    let staged = paths.io_dir.join("exit.staged");
    fs::write(&staged, content).unwrap();
    fs::rename(&staged, &paths.exit).unwrap();
}

#[test]
fn watchdog_cost_exit_marker_poll_sees_marker_that_appears_between_ticks() {
    use super::super::persistence::{ExitMarker, ExitMarkerProbe};
    let storage = tempfile::tempdir().unwrap();
    let (_, paths) = fixture(storage.path(), "session", false, 0);
    let mut probe = ExitMarkerProbe::default();

    // A running task's marker exists and is empty. The first poll does the
    // full read; later polls of the unchanged empty file do not.
    assert_eq!(probe_once(&paths, &mut probe), (None, 1));
    assert_eq!(probe_once(&paths, &mut probe), (None, 0));
    assert_eq!(probe_once(&paths, &mut probe), (None, 0));

    // The shell writes the status into the same file in place: seen next tick.
    fs::write(&paths.exit, b"0\n").unwrap();
    assert_eq!(
        probe_once(&paths, &mut probe),
        (Some(ExitMarker::Code(0)), 1)
    );

    // A missing marker costs no full read and is seen as soon as it appears.
    fs::remove_file(&paths.exit).unwrap();
    assert_eq!(probe_once(&paths, &mut probe), (None, 0));
    assert_eq!(probe_once(&paths, &mut probe), (None, 0));
    fs::write(&paths.exit, b"7").unwrap();
    assert_eq!(
        probe_once(&paths, &mut probe),
        (Some(ExitMarker::Code(7)), 1)
    );
}

#[test]
fn watchdog_cost_exit_marker_poll_retries_partial_marker() {
    use super::super::persistence::{ExitMarker, ExitMarkerProbe};
    let storage = tempfile::tempdir().unwrap();
    let (_, paths) = fixture(storage.path(), "session", false, 0);
    let mut probe = ExitMarkerProbe::default();
    assert_eq!(probe_once(&paths, &mut probe), (None, 1));

    // A marker caught mid-write is not a result, and is read again on every
    // poll rather than remembered, until it is complete.
    fs::write(&paths.exit, b"kil").unwrap();
    assert_eq!(probe_once(&paths, &mut probe), (None, 1));
    assert_eq!(probe_once(&paths, &mut probe), (None, 1));
    fs::write(&paths.exit, b"killed\n").unwrap();
    assert_eq!(
        probe_once(&paths, &mut probe),
        (Some(ExitMarker::Killed), 1)
    );
}

#[test]
fn watchdog_cost_exit_marker_poll_rereads_replaced_marker() {
    use super::super::persistence::{ExitMarker, ExitMarkerProbe};
    let storage = tempfile::tempdir().unwrap();
    let (_, paths) = fixture(storage.path(), "session", false, 0);
    let mut probe = ExitMarkerProbe::default();
    assert_eq!(probe_once(&paths, &mut probe), (None, 1));
    assert_eq!(probe_once(&paths, &mut probe), (None, 0));

    // Another empty file renamed over the marker is a different file: the
    // next poll validates it with a full read, then skips it again.
    // Windows exposes no stable file identity through `std`, and NTFS may
    // carry the old creation time over to a file renamed into the same name,
    // so only Unix can tell two empty files apart. Either way an empty marker
    // means no result.
    replace_marker(&paths, b"");
    let (marker, full_reads) = probe_once(&paths, &mut probe);
    assert_eq!(marker, None);
    #[cfg(unix)]
    assert_eq!(full_reads, 1, "a replaced empty marker was not re-read");
    #[cfg(not(unix))]
    let _ = full_reads;
    assert_eq!(probe_once(&paths, &mut probe), (None, 0));

    // An atomically published result is seen on the next poll.
    replace_marker(&paths, b"3\n");
    assert_eq!(
        probe_once(&paths, &mut probe),
        (Some(ExitMarker::Code(3)), 1)
    );
}

/// Per-tick cost of the watchdog's exit-marker check for N running tasks,
/// before (one full `read_exit_marker` per task, as `poll_task` did) and after
/// (`poll_task` with the probe). Prints read syscalls and bytes from
/// `/proc/self/io`, thread CPU time, and the module's own open/read counters.
/// Run with `--ignored --nocapture --test-threads=1` so no other test shares
/// the process-wide `/proc/self/io` counters.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "measurement: prints per-tick exit-marker poll cost"]
fn watchdog_cost_exit_marker_poll_measurement() {
    fn proc_io() -> (u64, u64) {
        let text = fs::read_to_string("/proc/self/io").unwrap();
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap()
        };
        (field("syscr:"), field("rchar:"))
    }
    fn thread_cpu_micros() -> u64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) },
            0
        );
        let micros = |time: libc::timeval| time.tv_sec as u64 * 1_000_000 + time.tv_usec as u64;
        micros(usage.ru_utime) + micros(usage.ru_stime)
    }
    fn measure(ticks: usize, mut tick: impl FnMut()) -> String {
        tick();
        work_counts::reset();
        let (syscalls, bytes) = proc_io();
        let cpu = thread_cpu_micros();
        for _ in 0..ticks {
            tick();
        }
        let cpu = thread_cpu_micros() - cpu;
        let (syscalls_after, bytes_after) = proc_io();
        let counts = work_counts::get();
        let per = |value: u64| value as f64 / ticks as f64;
        format!(
            "read syscalls/tick={:.0} bytes read/tick={:.0} cpu/tick={:.0}us opens/tick={:.0} metadata reads/tick={:.0} marker reads/tick={:.0} stats/tick={:.0}",
            per(syscalls_after - syscalls),
            per(bytes_after - bytes),
            per(cpu),
            per(counts.opens as u64),
            per(counts.metadata_reads as u64),
            per(counts.marker_reads as u64),
            per(counts.stats as u64),
        )
    }
    const TICKS: usize = 20;
    for count in [10usize, 100, 500] {
        let storage = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let mut tasks = Vec::new();
        for _ in 0..count {
            // A PTY task restored after a restart has no runtime handle, so
            // every tick reaches the marker check (as every task does on
            // Windows). Its recorded child is this live test process.
            let (mut metadata, paths) = fixture(storage.path(), "session", false, 0);
            metadata.mode = BgMode::Pty;
            metadata.mark_running(std::process::id(), std::process::id() as i32);
            metadata.finished_at = None;
            metadata.duration_ms = None;
            write_task_at(
                &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
                &metadata,
            )
            .unwrap();
            registry
                .insert_rehydrated_task(metadata, paths.clone(), true)
                .unwrap();
            tasks.push(registry.task(&paths.task_id).unwrap());
        }
        let before = measure(TICKS, || {
            for task in &tasks {
                assert_eq!(read_exit_marker(&task.paths).unwrap(), None);
            }
        });
        let after = measure(TICKS, || {
            for task in &tasks {
                registry.poll_task(task).unwrap();
            }
        });
        assert!(tasks.iter().all(|task| task.is_running()));
        eprintln!("{count} running tasks, before: {before}");
        eprintln!("{count} running tasks, after:  {after}");
    }
}

/// Measure the current watchdog path, including terminal candidates retained
/// for a live process group. Trace the BEGIN/END windows with strace to exclude
/// fixture creation from syscall counts. Run alone with --ignored --nocapture
/// --test-threads=1; CPU is measured only on the calling thread.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "measurement: prints retained-terminal and running poll-pass cost"]
fn watchdog_cost_retained_terminal_poll_measurement() {
    fn cpu_micros() -> u64 {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut usage) },
            0
        );
        let micros = |time: libc::timeval| time.tv_sec as u64 * 1_000_000 + time.tv_usec as u64;
        micros(usage.ru_utime) + micros(usage.ru_stime)
    }
    const PASSES: u64 = 200;
    for count in [100usize, 500] {
        for retained in [true, false] {
            let storage = tempfile::tempdir().unwrap();
            let registry = BgTaskRegistry::default();
            for _ in 0..count {
                let (mut metadata, paths) = fixture(storage.path(), "session", true, 0);
                metadata.mode = BgMode::Pty;
                metadata.child_pid = Some(std::process::id());
                metadata.pgid = Some(unsafe { libc::getpgrp() });
                if retained {
                    metadata.sandbox_temp_dir = Some(storage.path().join("scratch"));
                    fs::write(&paths.exit, b"0\n").unwrap();
                } else {
                    metadata.mark_running(std::process::id(), unsafe { libc::getpgrp() });
                    metadata.finished_at = None;
                    metadata.duration_ms = None;
                }
                write_task_at(
                    &resolve_task_layout(&paths.session_dir, &paths.task_id).unwrap(),
                    &metadata,
                )
                .unwrap();
                registry
                    .insert_rehydrated_task(metadata, paths, true)
                    .unwrap();
            }
            let mut awaiting = HashMap::new();
            let mut pass = || {
                super::super::watchdog::run_pass(&registry, WatchdogPassCause::Tick, &mut awaiting)
            };
            pass();
            assert_eq!(registry.running_tasks().len(), count);
            let kind = if retained {
                "retained_terminal"
            } else {
                "running"
            };
            work_counts::reset();
            eprintln!("WATCHDOG_MEASUREMENT_BEGIN tasks={count} kind={kind} passes={PASSES}");
            let cpu = cpu_micros();
            for _ in 0..PASSES {
                pass();
            }
            let cpu = cpu_micros() - cpu;
            eprintln!("WATCHDOG_MEASUREMENT_END");
            let counts = work_counts::get();
            let per = |value: usize| value as f64 / PASSES as f64;
            eprintln!("tasks={count} kind={kind} cpu_us/pass={:.2} opens/pass={:.0} metadata_reads/pass={:.0} marker_reads/pass={:.0} stats/pass={:.0}",
                cpu as f64 / PASSES as f64, per(counts.opens), per(counts.metadata_reads),
                per(counts.marker_reads), per(counts.stats));
            if !retained {
                assert_eq!(counts.metadata_reads, 0, "running control resolved layouts");
                assert_eq!(
                    counts.marker_reads, 0,
                    "running control re-read empty markers"
                );
                assert_eq!(counts.stats, count * PASSES as usize);
            }
        }
    }
}
