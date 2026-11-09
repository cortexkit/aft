//! Deterministic workload for the registry's persistence and cleanup paths.
use super::*;
use crate::bash_background::persistence::work_counts;

fn cpu_micros() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // RUSAGE_SELF includes cleanup workers as well as this test thread.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let usage = unsafe { usage.assume_init() };
    let micros = |time: libc::timeval| time.tv_sec as u64 * 1_000_000 + time.tv_usec as u64;
    micros(usage.ru_utime) + micros(usage.ru_stime)
}

fn phase(name: &str, tasks: usize, run: impl FnOnce()) -> work_counts::Counts {
    eprintln!("bookkeeping begin {name}: tasks={tasks}");
    work_counts::reset();
    let cpu = cpu_micros();
    let wall = Instant::now();
    run();
    let cpu = cpu_micros() - cpu;
    let counts = work_counts::get();
    eprintln!(
        "bookkeeping {name}: tasks={tasks} cpu_us={cpu} wall_ms={} syncs={} reads={} parses={} opens={} writes={} renames={} per_task(cpu_us/syncs/reads/parses)={:.2}/{:.2}/{:.2}/{:.2}",
        wall.elapsed().as_millis(), counts.syncs, counts.metadata_reads, counts.parses,
        counts.opens, counts.metadata_writes, counts.renames, cpu as f64 / tasks as f64,
        counts.syncs as f64 / tasks as f64, counts.metadata_reads as f64 / tasks as f64,
        counts.parses as f64 / tasks as f64,
    );
    counts
}

#[test]
fn bash_background_bookkeeping_day_probe() {
    const TASKS: usize = 300;
    const REMOTE: usize = TASKS / 2;
    const SECONDS: u64 = 180;
    let dir = tempfile::tempdir().unwrap();
    // Reuse the real remote recovery fixture's metadata shape, not its worker.
    let (template_registry, template_id, _) =
        persist_accepted_with_snapshot(dir.path(), dir.path().join("unused"), "probe");
    let template = template_registry
        .task(&template_id)
        .unwrap()
        .state
        .lock()
        .unwrap()
        .metadata
        .remote
        .clone()
        .unwrap();
    let registry = registry();
    registry.set_harness(crate::harness::Harness::Runner);
    let db = Arc::new(Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    registry.set_db_pool(db.clone());
    let mut tasks = Vec::new();
    phase("create", TASKS, || {
        for index in 0..TASKS {
            let layout = allocate_task_layout(dir.path(), "day").unwrap();
            let mut metadata = PersistedTask::starting(
                layout.paths.task_id.clone(),
                "day".into(),
                "probe".into(),
                dir.path().into(),
                Some(dir.path().into()),
                None,
                true,
                false,
            );
            metadata.harness = Some("runner".into());
            metadata.status = BgTaskStatus::Running;
            let scratch = dir.path().join(format!("scratch-{index}"));
            fs::create_dir(&scratch).unwrap();
            metadata.sandbox_temp_dir = Some(scratch);
            if index < REMOTE {
                let mut remote = template.clone();
                remote.job_id = None;
                remote.accepted = false;
                remote.fallback_digest = None;
                metadata.remote = Some(remote);
            }
            let handles = TaskIoHandles::create(&layout, BgMode::Pipes, true).unwrap();
            write_task_at(&layout, &metadata).unwrap();
            registry
                .insert_rehydrated_task(metadata, layout.paths.clone(), false)
                .unwrap();
            let task = registry.task(&layout.paths.task_id).unwrap();
            task.state.lock().unwrap().io_handles = Some(handles);
            // Count persistence, not notifications or a real runner connection.
            *task.terminal_notification_routed.lock().unwrap() = true;
            tasks.push(task);
        }
    });
    let start = Instant::now();
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut sinks = Vec::new();
    let mut consumers = Vec::new();
    phase("remote_start", REMOTE, || {
        for task in &tasks[..REMOTE] {
            let mut sink = TaskSink::new(&registry, task.clone()).unwrap();
            let ticks = ticks.clone();
            sink.set_progress_clock_for_test(Arc::new(move || {
                start + Duration::from_millis(ticks.load(Ordering::Relaxed))
            }));
            let mut consumer = exec::StreamConsumer::new();
            consumer
                .consume(StreamRecord::Accepted(Accepted::new(id(), 1)), &mut sink)
                .unwrap();
            consumer
                .consume(StreamRecord::Started(Started::new(0, 123, 1)), &mut sink)
                .unwrap();
            sinks.push(sink);
            consumers.push(consumer);
        }
    });
    phase("remote_stream_180s", REMOTE, || {
        // Ten chunks per second for three minutes, without wall-clock sleeps.
        for seq in 1..=SECONDS * 10 {
            ticks.store(seq * 100, Ordering::Relaxed);
            for (consumer, sink) in consumers.iter_mut().zip(&mut sinks) {
                consumer
                    .consume(
                        StreamRecord::Output(Output::new(
                            seq,
                            if seq % 2 == 0 {
                                OutputStream::Stdout
                            } else {
                                OutputStream::Stderr
                            },
                            BytePayload(b"streamed line\n".to_vec()),
                        )),
                        sink,
                    )
                    .unwrap();
            }
        }
    });
    phase("local_output", TASKS - REMOTE, || {
        for task in &tasks[REMOTE..] {
            fs::write(&task.paths.stdout, b"local output\n").unwrap();
            fs::write(&task.paths.exit, b"0\n").unwrap();
        }
    });
    phase("terminal", TASKS, || {
        for ((consumer, sink), task) in consumers.iter_mut().zip(&mut sinks).zip(&tasks) {
            consumer
                .consume(
                    StreamRecord::Terminal(TerminalRecord::new(
                        id(),
                        Outcome::Exit { code: 0 },
                        1,
                        0,
                        0,
                    )),
                    sink,
                )
                .unwrap();
            registry.remote_terminal(task, consumer.finish().unwrap(), None);
        }
        for task in &tasks[REMOTE..] {
            task.state.lock().unwrap().child_exit_observed = true;
            registry
                .finalize_from_marker(task, ExitMarker::Code(0), None)
                .unwrap();
        }
        assert!(tasks.iter().all(|task| task.is_terminal()));
    });
    phase("retained_120_watchdog_passes", TASKS, || {
        for _ in 0..120 {
            for task in &tasks {
                registry
                    .cleanup_terminal_task_temp_at(task, unix_millis())
                    .unwrap();
            }
        }
    });
    phase("cleanup", TASKS, || {
        for task in &tasks {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !registry
                .cleanup_terminal_task_temp_at(task, u64::MAX)
                .unwrap()
            {
                assert!(Instant::now() < deadline, "scratch cleanup stalled");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(task
                .state
                .lock()
                .unwrap()
                .metadata
                .sandbox_temp_dir
                .is_none());
        }
    });
    // Read outside measured phases: final outcome and cleanup must be durable.
    for task in &tasks {
        let saved = read_task(&task.paths.json).unwrap();
        assert!(saved.is_terminal());
        assert_eq!(saved.exit_code, Some(0));
        assert!(saved.sandbox_temp_dir.is_none());
    }
}
