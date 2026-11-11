use super::*;
use crate::bounded_io::regression_tests::BlockDirectory;
#[cfg(unix)]
use crate::bounded_io::tests as io_tests;
use crate::bounded_io::*;
#[cfg(unix)]
use std::sync::mpsc::channel;
use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

#[test]
fn reused_helper_starts_each_job_with_clean_counters_and_hooks() {
    use crate::commands::read::{
        IMAGE_PROCESSING_DELAY, LEGACY_HASHLINE_READ, LEGACY_READ_RENDER_BYTES,
    };
    use crate::hashline::snapshot::{SOURCE_AFTER_READ, SOURCE_READS};
    use crate::path_identity::CANONICALIZE_CALLS;
    use crate::search_index::{
        audit_record, audit_work, AuditWork, AUDIT_CANCEL_ELAPSED, AUDIT_IO_PROBE,
    };

    let pool = Arc::new(IsolatedPool::with_test_scope(1, test_helper_scope));
    let nested_pool = pool.clone();
    let first = pool
        .run(Path::new("first"), None, move |_| {
            SOURCE_READS.with(|count| count.set(count.get() + 1));
            LEGACY_READ_RENDER_BYTES.with(|count| count.set(count.get() + 7));
            LEGACY_HASHLINE_READ.with(|flag| flag.set(true));
            IMAGE_PROCESSING_DELAY.with(|delay| delay.set(Duration::from_secs(1)));
            SOURCE_AFTER_READ
                .with(|hook| *hook.borrow_mut() = Some(Box::new(|| panic!("old hook"))));
            CANONICALIZE_CALLS.with(|calls| *calls.borrow_mut() = Some(vec![PathBuf::from("old")]));
            AUDIT_IO_PROBE
                .with(|probe| *probe.borrow_mut() = Some(Box::new(|| panic!("old probe"))));
            AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.set(Some(Duration::from_secs(1))));
            audit_record(|work| {
                work.cancel_probes += 1;
                work.corpus_opens += 1;
                work.sort_stats += 1;
                work.sort_canonicalizes += 1;
                work.watcher_global += 1;
                work.watcher_head += 1;
                work.filter_keys += 1;
                work.presence_checks += 1;
            });
            dirty_helper_job_state();
            let thread = std::thread::current().id();
            nested_pool.run(Path::new("nested"), None, move |_| {
                assert_eq!(std::thread::current().id(), thread);
                assert_eq!(SOURCE_READS.with(|count| count.get()), 1);
                assert_eq!(LEGACY_READ_RENDER_BYTES.with(|count| count.get()), 7);
                assert_eq!(audit_work().filter_keys, 1);
                Ok(())
            })?;
            Ok(std::thread::current().id())
        })
        .unwrap();
    let second = pool
        .run(Path::new("second"), None, |_| {
            Ok((
                std::thread::current().id(),
                SOURCE_READS.with(|count| count.get()),
                LEGACY_READ_RENDER_BYTES.with(|count| count.get()),
                LEGACY_HASHLINE_READ.with(|flag| flag.get()),
                IMAGE_PROCESSING_DELAY.with(|delay| delay.get()),
                SOURCE_AFTER_READ.with(|hook| hook.borrow().is_none()),
                helper_job_state().0,
                audit_work(),
                helper_job_state().1,
                CANONICALIZE_CALLS.with(|calls| calls.borrow().is_none()),
                AUDIT_IO_PROBE.with(|probe| probe.borrow().is_none()),
                AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.get()),
            ))
        })
        .unwrap();
    assert_eq!(first, second.0, "both jobs must use the same helper");
    assert_eq!((second.1, second.2), (0, 0), "per-job counters leaked");
    assert!(!second.3, "legacy read flag leaked");
    assert_eq!(second.4, Duration::ZERO, "image delay leaked");
    assert!(second.5, "source-read hook leaked");
    assert!(second.6, "directory cache leaked");
    assert_eq!(second.7, AuditWork::default(), "grep work counters leaked");
    assert!(!second.8, "mutation admission leaked");
    assert!(second.9, "canonicalization collector leaked");
    assert!(second.10, "grep I/O probe leaked");
    assert_eq!(second.11, None, "grep cancellation clock leaked");
}

#[test]
fn helper_job_scope_restores_state_after_unwind() {
    use crate::commands::read::LEGACY_READ_RENDER_BYTES;
    use crate::hashline::snapshot::SOURCE_READS;

    let previous_reads = SOURCE_READS.with(|count| count.replace(11));
    let previous_render = LEGACY_READ_RENDER_BYTES.with(|count| count.replace(13));
    let unwound = std::panic::catch_unwind(|| {
        let _state = test_helper_scope();
        assert_eq!(SOURCE_READS.with(|count| count.get()), 0);
        assert_eq!(LEGACY_READ_RENDER_BYTES.with(|count| count.get()), 0);
        SOURCE_READS.with(|count| count.set(1));
        LEGACY_READ_RENDER_BYTES.with(|count| count.set(7));
        panic!("unwind a helper job");
    });
    let reads = SOURCE_READS.with(|count| count.replace(previous_reads));
    let render = LEGACY_READ_RENDER_BYTES.with(|count| count.replace(previous_render));
    assert!(unwound.is_err());
    assert_eq!((reads, render), (11, 13));
}

#[test]
fn delete_does_not_enumerate_project_root_parent() {
    let dir = tempfile::tempdir().unwrap();
    let ancestor = std::fs::canonicalize(dir.path()).unwrap();
    let root = ancestor.join("project");
    std::fs::create_dir(&root).unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.restrict_to_project_root = true;
    config.storage_dir = Some(ancestor.join("store"));
    config.backup.enabled = Some(false);
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let blocked = BlockDirectory::new(&ancestor);
    // Separate requests exercise both the initial root resolution and its memo.
    for name in ["cold", "cached"] {
        let target = root.join(name);
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("file.txt"), "delete me").unwrap();
        let request: RawRequest = serde_json::from_value(serde_json::json!({
            "id": name, "command": "delete_file", "files": [target], "recursive": true
        }))
        .unwrap();
        let response = crate::commands::delete_file::handle_delete_file(&request, &ctx);
        assert!(response.success, "{name}: {:?}", response.data);
        assert!(!target.exists());
    }
    assert_eq!(blocked.1.load(Ordering::Relaxed), 0);
}

#[cfg(unix)]
fn delete_context(root: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    // Unrestricted validation hands the target path itself to the first
    // helper (the access-policy identity check), so the test gate can key on it.
    config.restrict_to_project_root = false;
    config.storage_dir = Some(root.join(".store"));
    config.backup.enabled = Some(false);
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

/// Runs a recursive delete of a fresh directory while its first path-check
/// helper is held unscheduled for `stall`, then releases it.
#[cfg(unix)]
fn delete_with_unscheduled_helper(
    budget: Option<Budget>,
    stall: Duration,
) -> (crate::protocol::Response, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let target = root.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("file.txt"), "delete me").unwrap();
    let ctx = delete_context(&root);
    let gate = stall_helper(&target);
    let request: RawRequest = serde_json::from_value(serde_json::json!({
        "id": "late-helper", "command": "delete_file", "files": [target], "recursive": true
    }))
    .unwrap();
    let (result_tx, result_rx) = channel();
    let caller = std::thread::spawn(move || {
        let response = with_budget(budget, || {
            crate::commands::delete_file::handle_delete_file(&request, &ctx)
        });
        result_tx.send(response).unwrap();
    });
    gate.reached.recv_timeout(IO_BUDGET).unwrap();
    std::thread::sleep(stall);
    drop(gate);
    let response = result_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    caller.join().unwrap();
    let deleted = !target.exists();
    drop(dir);
    assert_eq!(deleted, response.success, "{:?}", response.data);
    (response, target)
}

// A tool-call delete runs under a long request budget. A helper scheduled
// after the per-access cap has still done nothing, so the delete must wait
// for it rather than refuse with read_io_queue_timeout.
#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
#[test]
fn tool_call_delete_waits_for_a_late_helper_instead_of_refusing() {
    let budget =
        Budget::after(Duration::from_secs(25)).with_access_budget(Duration::from_millis(200));
    let (response, _) = delete_with_unscheduled_helper(Some(budget), Duration::from_millis(800));
    assert!(response.success, "{:?}", response.data);
}

// A native `delete_file` has no caller budget. Hold its helper past the
// five-second access cap, the delay observed on a loaded Windows runner.
#[cfg(unix)]
#[test]
fn native_delete_waits_for_a_helper_scheduled_after_the_access_cap() {
    let (response, _) =
        delete_with_unscheduled_helper(None, IO_BUDGET + Duration::from_millis(500));
    assert!(response.success, "{:?}", response.data);
}

/// A fixed, in-process read/grep/edit workload, excluding context construction.
/// Process CPU includes helper creation and teardown, not just filesystem calls.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "manual performance measurement"]
fn probe_pool_workload() {
    fn cpu_seconds() -> f64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
            0
        );
        let usage = unsafe { usage.assume_init() };
        (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
    }
    let fixture = tempfile::tempdir().unwrap();
    let roots: Vec<_> = (0..3)
        .map(|n| fixture.path().join(format!("root-{n}")))
        .collect();
    let contexts: Vec<_> = roots
        .iter()
        .map(|root| {
            for directory in 0..4 {
                let path = root.join(format!("src-{directory}"));
                std::fs::create_dir_all(&path).unwrap();
                for file in 0..8 {
                    std::fs::write(path.join(format!("file-{file}.txt")), "needle alpha\n")
                        .unwrap();
                }
            }
            delete_context(root)
        })
        .collect();
    let spawns = THREAD_SPAWNS.load(Ordering::Relaxed);
    let cpu = cpu_seconds();
    let wall = Instant::now();
    let mut totals = [[0usize; 4]; 3];
    for iteration in 0..100 {
        for (root, ctx) in roots.iter().zip(&contexts) {
            let file = root.join("src-0/file-0.txt");
            for (kind, params) in [
                serde_json::json!({"command": "read", "file": file}),
                serde_json::json!({"command": "grep", "path": root, "pattern": "needle"}),
                serde_json::json!({"command": "edit_match", "file": file, "match": if iteration % 2 == 0 {"alpha"} else {"beta"}, "replacement": if iteration % 2 == 0 {"beta"} else {"alpha"}}),
            ].into_iter().enumerate() {
                let mut params = params;
                params["id"] = serde_json::json!("probe-load");
                let request: RawRequest = serde_json::from_value(params).unwrap();
                let budget = Budget::after(Duration::from_secs(25));
                let response = with_budget(Some(budget.clone()), || command(&request, ctx, |req, ctx| match kind {
                    0 => crate::commands::read::handle_read(req, ctx),
                    1 => crate::commands::grep::handle_grep(req, ctx),
                    _ => crate::commands::edit_match::handle_edit_match(req, ctx),
                }));
                assert!(response.success, "{}: {:?}", request.command, response);
                totals[kind][0] += 1;
                totals[kind][1] += budget.measurements.directory_probes.load(Ordering::Relaxed);
                totals[kind][2] += budget.measurements.traversal_probes.load(Ordering::Relaxed);
                totals[kind][3] += budget.measurements.round_trips.load(Ordering::Relaxed);
            }
        }
    }
    let wall = wall.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu;
    let spawns = THREAD_SPAWNS.load(Ordering::Relaxed) - spawns;
    let probes: usize = totals.iter().map(|row| row[3]).sum();
    println!("PROBE_LOAD calls/directory/traversal/dispatch={totals:?} wall_s={wall:.6} cpu_s={cpu:.6} spawns={spawns} threads_per_s={:.3} cpu_us_per_probe={:.3}", spawns as f64 / wall, cpu * 1_000_000.0 / probes as f64);
}

#[cfg(unix)]
#[test]
fn macos_first_touch_timeout_never_mutates_after_probe_release() {
    let dir = tempfile::tempdir().unwrap();
    let protected = dir.path().join("protected");
    std::fs::create_dir(&protected).unwrap();
    // Exercise aliased roots on every Unix host, not only macOS's /var.
    let root = dir.path().join("alias");
    std::os::unix::fs::symlink(&protected, &root).unwrap();
    let target = root.join("file.txt");
    std::fs::write(&target, "before").unwrap();
    let fifo = dir.path().join("answer");
    io_tests::fifo(&fifo);
    let blocker = io_tests::block_directory_probe(&root, &fifo);
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let request: RawRequest = serde_json::from_value(
        serde_json::json!({"id":"write","command":"write","file":target,"content":"after"}),
    )
    .unwrap();
    let started = Instant::now();
    // The access cap starts only after the helper is scheduled. A short
    // whole-request budget can expire during unrelated preflight I/O and
    // never reach the blocked directory, especially on a loaded test host.
    // Preflight probes the canonical directory, not the request's alias.
    let budget = Budget::after(Duration::from_secs(30))
        .with_path_access_budget(&root.canonicalize().unwrap(), Duration::from_millis(100));
    let response = with_budget(Some(budget.clone()), || {
        crate::commands::write::handle_write(&request, &ctx)
    });
    assert_eq!(response.data["code"], "read_blocked", "{response:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the blocked probe must hit its per-access cap, not the five-second default"
    );
    assert_eq!(blocker.fired.load(Ordering::Relaxed), 1);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
    io_tests::release_fifo(&fifo);
    blocker
        .finished
        .recv_timeout(Duration::from_secs(2))
        .expect("the released helper finishes its read-only probe");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
    assert!(!response.success);
}

#[test]
fn audit1432_skip_disclosure_does_not_mutate_error_or_cancelled_replies() {
    with_budget(Some(Budget::after(Duration::from_secs(30))), || {
        note_large_read_skip(Path::new("large.log"));
        for code in ["test_error", "request_cancelled"] {
            let mut response = Response::error("omission-error", code, "original failure");
            let original = serde_json::to_value(&response).unwrap();
            disclose_large_read_skips(&mut response);
            assert_eq!(serde_json::to_value(response).unwrap(), original, "{code}");
        }
    });
}

#[test]
fn audit1432_zero_result_search_keeps_no_results_and_independent_honesty_notes() {
    with_budget(Some(Budget::after(Duration::from_secs(30))), || {
        note_large_read_skip(Path::new("large.log"));
        let mut response = Response::success(
            "empty-search",
            serde_json::json!({
                "text":"", "results":[], "more_available":true, "engine_capped":true,
                "fully_degraded":true,
                "results_list_envelope":{"shown":0,"total":{"kind":"exact","value":0},"unit":"results","causes":[],"narrow":["query"]}
            }),
        );
        disclose_large_read_skips(&mut response);
        let text = crate::subc_format::format_response("search", &response, false);
        for expected in [
            "No results.",
            "more results available",
            "enumeration capped",
            "answered by a filesystem scan",
            "large.log",
            crate::large_read::WHOLE_READ_SKIP_REASON,
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
    });
}

#[test]
fn audit1432_round2_nonempty_search_keeps_independent_honesty_with_skips() {
    with_budget(Some(Budget::after(Duration::from_secs(30))), || {
        note_large_read_skip(Path::new("large.log"));
        let mut response = Response::success(
            "rows-search",
            serde_json::json!({"text":"retained row", "more_available":true, "engine_capped":true, "fully_degraded":true}),
        );
        disclose_large_read_skips(&mut response);
        let text = crate::subc_format::format_response("search", &response, false);
        for expected in [
            "retained row",
            "more results available",
            "enumeration capped",
            "answered by a filesystem scan",
            "large.log",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
    });
}

fn round2_context(root: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.restrict_to_project_root = false;
    config.storage_dir = Some(root.join(".store"));
    config.backup.enabled = Some(false);
    AppContext::new(Box::new(crate::language::StubProvider), config)
}

fn round2_nonsearch_reply(
    command_name: &str,
    body: serde_json::Value,
    floors: &[PathBuf],
) -> Response {
    let root = tempfile::tempdir().unwrap();
    let ctx = round2_context(root.path());
    let req: RawRequest =
        serde_json::from_value(serde_json::json!({"id":"main-contract","command":command_name}))
            .unwrap();
    crate::context::confinement::with_floor_disclosure(|| {
        command(&req, &ctx, |_, _| {
            for path in floors {
                crate::context::confinement::capture().record_matched_floor(path);
            }
            Response::success("main-contract", body)
        })
    })
}

#[test]
fn audit1432_round2_outline_directory_reply_keeps_main_skip_contract_bytes() {
    let skips = (0..23).rev().map(|at| serde_json::json!({"file":format!("unsupported-{at:02}.odd"),"reason":"unsupported_language"})).collect::<Vec<_>>();
    let body = serde_json::json!({"text":"directory outline\nstructure footer", "structure_footer":"structure footer", "discovered":true,"complete":true,"skipped_files":skips});
    let expected = Response::success("main-contract", body.clone());
    let response = round2_nonsearch_reply("outline", body, &[]);
    assert_eq!(
        serde_json::to_vec(&response).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(
        crate::subc_format::format_response("outline", &response, false),
        crate::subc_format::format_response("outline", &expected, false)
    );
}

#[test]
fn audit1432_round2_glob_floor_only_reply_keeps_main_skip_contract_bytes() {
    let floors = (0..23)
        .map(|at| PathBuf::from(format!("denied-{at:02}.log")))
        .collect::<Vec<_>>();
    let mut expected_body = serde_json::json!({"text":"visible.rs", "files":["visible.rs"]});
    let response = round2_nonsearch_reply("glob", expected_body.clone(), &floors);
    expected_body["skipped_files"] = serde_json::json!(floors
        .iter()
        .map(|path| serde_json::json!({"file":path,"reason":"denied_floor"}))
        .collect::<Vec<_>>());
    expected_body["complete"] = serde_json::json!(false);
    let expected = Response::success("main-contract", expected_body);
    assert_eq!(
        serde_json::to_vec(&response).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(
        crate::subc_format::format_response("glob", &response, false),
        crate::subc_format::format_response("glob", &expected, false)
    );
}

#[test]
fn audit1432_round2_nested_dead_code_reply_keeps_main_skip_contract_bytes() {
    let skips = (0..23)
        .rev()
        .map(
            |at| serde_json::json!({"file":format!("large-{at:02}.rs"),"reason":"graph_too_large"}),
        )
        .collect::<Vec<_>>();
    let body = serde_json::json!({"summary":{"dead_code":{"complete":false,"skipped_files_count":23,"skipped_files":skips}}});
    let expected = Response::success("main-contract", body.clone());
    let response = round2_nonsearch_reply("inspect", body, &[]);
    assert_eq!(
        serde_json::to_vec(&response).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
}

#[test]
fn audit1432_grep_without_producer_text_still_renders_named_skips() {
    with_budget(Some(Budget::after(Duration::from_secs(30))), || {
        note_large_read_skip(Path::new("large.log"));
        let mut response = Response::success("textless-grep", serde_json::json!({"matches":[]}));
        disclose_large_read_skips(&mut response);
        response.data.as_object_mut().unwrap().remove("text");
        let text = crate::subc_format::format_response("grep", &response, false);
        assert!(text.contains("large.log"), "{text}");
        assert!(
            text.contains(crate::large_read::WHOLE_READ_SKIP_REASON),
            "{text}"
        );
    });
}

#[test]
fn audit1432_walk_gap_precedes_the_entire_skipped_files_block() {
    with_budget(Some(Budget::after(Duration::from_secs(30))), || {
        let budget = current().unwrap();
        budget.walk_partial.store(true, Ordering::Release);
        budget
            .walk_time_limit_reached
            .store(true, Ordering::Release);
        for at in 0..23 {
            note_large_read_skip(Path::new(&format!("large-{at:02}.log")));
        }
        let response = with_skip_disclosure(|| {
            finish_response(Response::success(
                "walk-and-skips",
                serde_json::json!({"text":"rows"}),
            ))
        });
        let text = response.data["text"].as_str().unwrap();
        assert!(
            text.find(WALK_PARTIAL_GAP).unwrap()
                < text
                    .find(crate::large_read::WHOLE_READ_SKIP_REASON)
                    .unwrap(),
            "{text}"
        );
        assert!(
            text.ends_with("shown 20 of 23 files (cap) · narrow: path"),
            "{text}"
        );
    });
}

#[test]
fn audit1432_skip_collector_is_order_independent_and_deduplicates_paths_with_both_reasons() {
    let collect = |command_name: &str, reverse: bool| {
        let root = tempfile::tempdir().unwrap();
        let ctx = round2_context(root.path());
        let req: RawRequest =
            serde_json::from_value(serde_json::json!({"id":"mixed-order","command":command_name}))
                .unwrap();
        with_skip_disclosure(|| {
            let work = || {
                for at in 0..23 {
                    let path = PathBuf::from(format!("file-{at:02}.log"));
                    crate::context::confinement::capture().record_matched_floor(&path);
                    note_large_read_skip(&path);
                }
                Response::success("mixed-order", serde_json::json!({"text":"rows"}))
            };
            if reverse {
                command(&req, &ctx, |_, _| {
                    crate::context::confinement::with_floor_disclosure(work)
                })
            } else {
                crate::context::confinement::with_floor_disclosure(|| {
                    command(&req, &ctx, |_, _| work())
                })
            }
        })
    };
    let first = collect("grep", false);
    for command in ["grep", "semantic_search"] {
        for reverse in [false, true] {
            assert_eq!(
                serde_json::to_vec(&first).unwrap(),
                serde_json::to_vec(&collect(command, reverse)).unwrap(),
                "{command}, reverse={reverse}"
            );
        }
    }
    let files = first.data["skipped_files"].as_array().unwrap();
    assert_eq!(files.len(), 20);
    assert_eq!(first.data["skipped_files_remaining"], 3);
    assert_eq!(first.data["skipped_files_list_envelope"]["shown"], 20);
    assert_eq!(
        first.data["skipped_files_list_envelope"]["total"]["value"],
        23
    );
    assert_eq!(files[0]["file"], "file-00.log");
    let reasons = files[0]["reasons"].as_array().unwrap();
    assert!(reasons.contains(&serde_json::json!("denied_floor")));
    assert!(reasons.contains(&serde_json::json!(
        crate::large_read::WHOLE_READ_SKIP_REASON
    )));
    assert!(first.data["text"]
        .as_str()
        .unwrap()
        .ends_with("shown 20 of 23 files (cap) · narrow: path"));
}
