mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use crate::context::AppContext;
    use crate::grep_executor::literal;
    use crate::grep_executor::{
        fallback_grep_with_limits, project_root, walk_coverage_note, FALLBACK_WALK_BUDGET,
    };
    use crate::search_index::{GrepResult, IndexStatus, PathFilters, WalkBound};

    /// 100 directories of 100 files, each file holding one `needle`, and a
    /// context for it with no index of any kind, so grep walks the folder.
    fn grep_io_fixture() -> (tempfile::TempDir, AppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for directory in 0..100 {
            let parent = root.join(format!("directory_{directory:03}"));
            std::fs::create_dir(&parent).unwrap();
            for file in 0..100 {
                std::fs::write(
                    parent.join(format!("file_{file:03}.rs")),
                    "pub fn needle() {}\n",
                )
                .unwrap();
            }
        }
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root),
                ..crate::config::Config::default()
            },
        );
        (dir, ctx)
    }

    /// A context whose text index has published, so grep takes candidates
    /// from the index rather than walking `root`. The call graph is disabled:
    /// grep does not use it, and building it for 10,000 files is slow.
    fn grep_io_data_layer_context(root: &Path, storage: &Path) -> AppContext {
        let mut config = crate::config::Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..crate::config::Config::default()
        };
        config.indexes.callgraph = false;
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.to_path_buf());
        let deadline = Instant::now() + Duration::from_secs(600);
        while crate::data_layer::text_scope::ready_view(&ctx).is_none() {
            assert!(
                Instant::now() < deadline,
                "the data layer never published the fixture's text index"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        ctx
    }

    /// The grep measurement on a data-layer context, checking that the data
    /// layer answered: a walk of the fixture reports `Fallback`.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn measured_data_layer_grep(ctx: &AppContext) -> (Duration, usize, usize) {
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(60));
        let (measurement, index_status) = measured_grep_with_status(ctx, budget);
        assert_eq!(index_status, "Ready", "the data layer did not answer");
        measurement
    }

    fn measured_grep(ctx: &AppContext) -> (Duration, usize, usize) {
        let budget = crate::bounded_io::Budget::after(Duration::from_secs(60));
        measured_grep_with_status(ctx, budget).0
    }

    /// The measurement and the reply's `index_status`.
    fn measured_grep_with_status(
        ctx: &AppContext,
        budget: crate::bounded_io::Budget,
    ) -> ((Duration, usize, usize), serde_json::Value) {
        let request = crate::protocol::RawRequest {
            id: "grep-io-measurement".into(),
            command: "grep".into(),
            session_id: None,
            lsp_hints: None,
            params: serde_json::json!({"pattern": "needle", "max_results": 10001}),
        };
        let started = Instant::now();
        let response = crate::bounded_io::with_budget(Some(budget.clone()), || {
            crate::commands::grep::handle_grep(&request, ctx)
        });
        let elapsed = started.elapsed();
        assert!(response.success, "{}", response.data);
        assert_eq!(
            response.data["files_searched"], 10_000,
            "the measurement must actually verify all fixture files"
        );
        assert_eq!(response.data["total_matches"], 10_000);
        let (spawns, round_trips) = budget.measurements();
        (
            (elapsed, spawns, round_trips),
            response.data["index_status"].clone(),
        )
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ten_thousand_file_grep_uses_directory_probes_not_per_file_helpers() {
        let (_fixture, ctx) = grep_io_fixture();
        let storage = tempfile::tempdir().unwrap();
        let indexed = grep_io_data_layer_context(&project_root(&ctx), storage.path());
        let pool = rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("aft-rayon-grep-test-{index}"))
            .num_threads(4)
            .build()
            .unwrap();
        let (_, spawns, round_trips) = pool.install(|| measured_data_layer_grep(&indexed));
        assert!(spawns > 0, "cold first-access probes must be counted");
        assert!(spawns <= 132 && round_trips <= 132, "100 directories plus fixed root work, not 10000 files: spawns={spawns} round_trips={round_trips}");
        let (_, spawns, round_trips) = pool.install(|| measured_grep(&ctx));
        assert!(spawns > 0);
        assert!(spawns <= 132 && round_trips <= 132, "the fallback walk must reuse its bounded helper: spawns={spawns} round_trips={round_trips}");
    }

    // Grep reads files on rayon workers, then stats each matched file on the
    // thread that sorts the results. Called from a thread outside any rayon
    // pool, the sorting thread reads no files itself. Each directory's access
    // probe (`bounded_io::probe_directory`) must then reach it from the
    // request's shared probe map, not from its own per-thread cache, or it
    // spawns a filesystem helper thread again for every directory. In the
    // pooled tests the sorting thread reads some directories itself, how many
    // depending on rayon scheduling, which is why they failed only sometimes.
    //
    // With the default 5 s access cap, the fallback walk probes all 100 fixture
    // directories inside its own helper, which spawns nothing. What is left is
    // fixed work on the project root (its admission stat and listing, and the
    // walk's helper): at most 32 helpers, the root allowance in the pooled
    // tests' bound of 132 = 100 directories + 32. A helper per directory or
    // per file here means a directory the request already probed was probed
    // again.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ten_thousand_file_grep_sorts_matches_without_per_directory_respawns() {
        let (_fixture, ctx) = grep_io_fixture();
        let (_, spawns, round_trips) =
            std::thread::scope(|scope| scope.spawn(|| measured_grep(&ctx)).join().unwrap());
        assert!(spawns > 0);
        assert!(spawns <= 32 && round_trips <= 32, "sorting on a thread that read no files must reuse the directory probes: spawns={spawns} round_trips={round_trips}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn ten_thousand_file_grep_keeps_root_probe_budget_after_queueing() {
        // The probe is held queued for QUEUE_HOLD, then held running for
        // RUNNING_HOLD: their sum exceeds the cap, so the probe fails if queue
        // time is charged to it. Each hold alone stays well below the cap,
        // because on a loaded runner the release and the real root access that
        // follows it can take seconds; a 1.5 s margin was not enough in CI.
        const ROOT_ACCESS_CAP: Duration = Duration::from_secs(12);
        const QUEUE_HOLD: Duration = Duration::from_secs(8);
        const RUNNING_HOLD: Duration = Duration::from_secs(6);
        let (_fixture, walked) = grep_io_fixture();
        let root = project_root(&walked);
        let storage = tempfile::tempdir().unwrap();
        let data_layer = grep_io_data_layer_context(&root, storage.path());
        let pool = rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("aft-rayon-grep-test-{index}"))
            .num_threads(4)
            .build()
            .unwrap();
        for (indexed, ctx) in [(true, &data_layer), (false, &walked)] {
            let queued = crate::bounded_io::stall_helper(&root);
            let running = crate::bounded_io::stall_running_helper(&root);
            let budget = crate::bounded_io::Budget::after(Duration::from_secs(60))
                .with_path_access_budget(&root, ROOT_ACCESS_CAP);
            let observed = budget.clone();
            std::thread::scope(|scope| {
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let pool = &pool;
                let ctx = &ctx;
                let caller = scope.spawn(move || {
                    let result = pool.install(|| measured_grep_with_status(ctx, budget));
                    let _ = done_tx.send(());
                    result
                });
                queued
                    .reached
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
                // The first helper is command admission's metadata(root), before
                // the root listing or any indexed/fallback candidate reads.
                let cold_measurements = observed.measurements();
                let before_start = done_rx.recv_timeout(QUEUE_HOLD);
                drop(queued);
                let started = running.reached.recv_timeout(Duration::from_secs(30));
                let before_release = done_rx.recv_timeout(RUNNING_HOLD);
                drop(running);
                let ((_, spawns, round_trips), index_status) = caller.join().unwrap();
                if indexed {
                    assert_eq!(index_status, "Ready", "the data layer did not answer");
                }
                assert_eq!(cold_measurements.1, 1);
                assert!(before_start.is_err());
                started.expect("the fixture-root metadata helper must start");
                assert!(before_release.is_err());
                // In the fallback pass this test's 12 s root cap outlasts the
                // 5 s default cap of each subdirectory probe, so the walk's
                // helper on the root cannot run those probes itself (see
                // `run_with_timeout`) and each of the 100 directories gets its
                // own helper. That is still one helper per directory.
                assert!(spawns <= 132 && round_trips <= 132,
                    "indexed={indexed}: directory probes, not per-file helpers: spawns={spawns} round_trips={round_trips}");
            });
        }
    }

    #[test]
    #[ignore = "manual five-run 10000-file grep wall-time and helper-count measurement"]
    fn grep_io_10k_measurement() {
        let (_fixture, walked) = grep_io_fixture();
        let storage = tempfile::tempdir().unwrap();
        let data_layer = grep_io_data_layer_context(&project_root(&walked), storage.path());
        let pool = rayon::ThreadPoolBuilder::new()
            .thread_name(|index| format!("aft-rayon-grep-test-{index}"))
            .num_threads(4)
            .build()
            .unwrap();
        for (indexed, ctx) in [(true, &data_layer), (false, &walked)] {
            let mut samples = Vec::new();
            for run in 0..5 {
                let (elapsed, spawns, round_trips) = pool.install(|| measured_grep(ctx));
                println!("grep_io_10k mode={} run={} elapsed_ms={:.3} thread_spawns={} helper_round_trips={}",
                    if indexed { "indexed" } else { "fallback" }, run + 1, elapsed.as_secs_f64() * 1000.0, spawns, round_trips);
                samples.push(elapsed);
            }
            samples.sort();
            println!(
                "grep_io_10k mode={} median_ms={:.3}",
                if indexed { "indexed" } else { "fallback" },
                samples[2].as_secs_f64() * 1000.0
            );
        }
    }

    /// A tempdir holding `count` small text files, none containing `absent_token`.
    fn walk_fixture(count: usize) -> tempfile::TempDir {
        let project = tempfile::tempdir().expect("project");
        for index in 0..count {
            std::fs::write(
                project.path().join(format!("file_{index}.txt")),
                format!("line {index} — nothing to see\n"),
            )
            .expect("fixture file");
        }
        project
    }

    /// The grep tool's reply text for `result`, as `handle_grep` builds it.
    fn grep_tool_text(result: &GrepResult, project_root: &Path) -> String {
        let mut text = crate::commands::grep::grep_page_output(
            result,
            &crate::commands::grep::GrepPageRequest {
                project_root,
                include: &[],
                file_roots: &[],
                offset: 0,
                explicit_page_rows: None,
                ignored_scope_note: None,
            },
        )
        .text;
        if let Some(note) = walk_coverage_note(result) {
            text.push_str("\n\n");
            text.push_str(&note);
        }
        text
    }

    #[test]
    fn complete_fallback_walk_while_index_builds_says_every_file_was_searched() {
        let project = walk_fixture(3);
        let result = fallback_grep_with_limits(
            project.path(),
            project.path(),
            project.path(),
            &literal("absent_token"),
            &PathFilters::default(),
            10,
            IndexStatus::Building,
            None,
            10,
            FALLBACK_WALK_BUDGET,
        );
        assert_eq!(result.walk_bound, None);
        assert!(!result.walk_truncated);
        assert_eq!(result.files_read_directly, 3);

        let text = grep_tool_text(&result, project.path());
        assert_eq!(
            text,
            "Found 0 matches in 0 files [index: building; searched all 3 files directly]"
        );
    }

    #[test]
    fn bounded_fallback_walk_while_index_builds_says_it_is_incomplete() {
        let project = walk_fixture(5);
        let result = fallback_grep_with_limits(
            project.path(),
            project.path(),
            project.path(),
            &literal("absent_token"),
            &PathFilters::default(),
            10,
            IndexStatus::Building,
            None,
            2,
            FALLBACK_WALK_BUDGET,
        );
        assert_eq!(result.walk_bound, Some(WalkBound::FileLimit(2)));
        assert!(result.walk_truncated);
        assert_eq!(result.files_read_directly, 2);

        let text = grep_tool_text(&result, project.path());
        assert!(
            text.starts_with(
                "Found 0 matches in 0 files [index: building; incomplete: checked 2 files before the walk's 2-file limit]"
            ),
            "{text}"
        );
        assert!(
            text.contains("(checked 2 of more than 2 files (index building);"),
            "{text}"
        );
        assert!(
            text.contains("retry once the index is ready"),
            "the note must name an exhaustive alternative: {text}"
        );
        assert!(!text.contains("searched all"), "{text}");
    }

    #[test]
    fn walk_time_budget_marks_the_walk_incomplete() {
        let project = walk_fixture(3);
        let result = fallback_grep_with_limits(
            project.path(),
            project.path(),
            project.path(),
            &literal("absent_token"),
            &PathFilters::default(),
            10,
            IndexStatus::Building,
            None,
            10,
            Duration::ZERO,
        );
        assert_eq!(result.walk_bound, Some(WalkBound::TimeBudget));
        let text = grep_tool_text(&result, project.path());
        assert!(
            text.contains("incomplete: checked 0 files before the walk's time budget ran out"),
            "{text}"
        );
        assert!(text.contains("ran out of its time budget"), "{text}");
    }
}
