fn view_projection_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    InspectJob,
    crate::views::assembly::AssemblyRequest,
) {
    view_projection_fixture_with_files(&[
        ("main.ts", "import { used } from './target';\nexport function main() { used(); }\nmain();\n"),
        ("target.ts", "export function used() {}\nexport function dead() {}\n"),
    ])
}

fn view_projection_fixture_with_files(files: &[(&str, &str)]) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    InspectJob,
    crate::views::assembly::AssemblyRequest,
) {
    let _ = env_logger::builder().is_test(true).try_init();
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(project.path()).unwrap();
    for (file, source) in files {
        write_projection_cache_file(&root.join(file), source);
    }
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(&root));
        assert!(command.args(args).status().unwrap().success());
    }
    let family = crate::search_index::artifact_cache_key(&root);
    crate::root_cache::configure_artifact_access(&root, &family, false);
    let inspect_dir = storage.path().join("inspect");
    let callgraph_dir = callgraph_store_dir_from_inspect_dir(&inspect_dir, &root).unwrap();
    let files = crate::callgraph::walk_project_files(&root).collect::<Vec<_>>();
    drop(CallGraphStore::cold_build_with_lease(callgraph_dir, root.clone(), &files).unwrap());
    let mut job = snapshot_job(&root, &inspect_dir, true);
    job.scope_files = files;
    let config = Arc::make_mut(&mut job.config);
    config.storage_dir = Some(storage.path().to_path_buf());
    config.views.enabled = true;
    job.callgraph_writer = true;
    let request = crate::views::assembly::AssemblyRequest {
        storage: storage.path().to_path_buf(),
        project_root: root.clone(),
        family,
        scope: crate::path_identity::project_scope_key(&root),
        desired_head: crate::views::assembly::head_tree_fingerprint(
            &crate::alias::head_tree_entries(&root).unwrap(),
        ),
        changed_paths: Default::default(),
        semantic_keys: Default::default(),
        require_semantic: false,
        allow_blob_put: true,
        callgraph: true,
    };
    (project, storage, job, request)
}

#[test]
fn own_accuracy_view_keeps_platform_branches_and_handle_guard_live() {
    let (_project, _storage, mut job, request) = view_projection_fixture_with_files(&[
        ("Cargo.toml", "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        ("src/main.rs", r#"
mod read; mod edit; mod db { pub mod file_identity; }
fn main() { platform::copy(); read::read_exact(); edit::atomic_write_source(); }
mod platform {
    #[cfg(unix)] pub fn copy() {}
    #[cfg(windows)] pub fn copy() { verify(); }
    pub fn verify() { leaf(); }
    pub fn leaf() {}
    pub fn unused() {}
}
"#),
        ("src/read.rs", include_str!("../commands/file_read_exact.rs")),
        ("src/edit.rs", include_str!("../edit.rs")),
        ("src/db/file_identity.rs", "pub(crate) fn is_live_sqlite_handle() {}"),
    ]);
    crate::views::assembly::publish_checkout(&request).unwrap();
    job.callgraph_snapshot = build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files);
    assert!(job.callgraph_snapshot.is_some());
    let result = crate::inspect::scanners::dead_code::run_dead_code_scan(&job);
    let aggregate = result.outcome.unwrap().aggregate;
    for (file, symbol) in [
        ("src/main.rs", "verify"),
        ("src/main.rs", "leaf"),
        ("src/db/file_identity.rs", "is_live_sqlite_handle"),
        ("src/edit.rs", "link_count"),
        ("src/edit.rs", "copy_platform_security"),
        ("src/edit.rs", "platform_acls_match"),
        ("src/edit.rs", "extended_acl_text"),
        ("src/edit.rs", "clear_extended_acl"),
        ("src/edit.rs", "copy_protected"),
    ] {
        assert!(!aggregate["items"].as_array().unwrap().iter().any(|item| item["file"] == file && item["symbol"] == symbol), "{aggregate:#}");
    }
    assert!(aggregate["items"].as_array().unwrap().iter().any(|item| item["file"] == "src/main.rs" && item["symbol"] == "unused"), "{aggregate:#}");
}

#[test]
fn inspect_view_pending_publication_race_and_dispatch_are_root_local() {
    let manager = InspectManager::new();
    let root = PathBuf::from("pending-checkout");
    let other = PathBuf::from("uninspected-checkout");
    let observed_epoch = manager.view_publication_epoch(&root);
    assert!(!manager.note_view_publication(&root));
    manager.note_view_pending(&root, observed_epoch);
    assert!(manager.deferred_view_refresh_ready(&root), "a pending result recorded after publication must not lose its wakeup");
    assert!(!manager.note_view_publication(&other));
    assert!(!manager.deferred_view_refresh_ready(&other));
    manager.view_refresh_dispatched(&root, InspectCategory::Complexity);
    assert!(manager.deferred_view_refresh_ready(&root));
    manager.view_refresh_dispatched(&root, InspectCategory::DeadCode);
    assert!(!manager.deferred_view_refresh_ready(&root));
    assert!(!manager.note_view_publication(&root), "accepted refresh must consume the owed demand");
}

/// The publication hook names the checkout by its configured cache root (raw
/// `fs::canonicalize`, a `\\?\` verbatim path on Windows) while background jobs
/// carry the normalized root. Differently spelled names for one checkout must
/// share one owed-refresh entry, or the dispatched refresh never clears it.
#[test]
fn inspect_view_pending_refresh_keys_ignore_root_spelling() {
    let manager = InspectManager::new();
    let job_root = std::env::temp_dir().join("aft-view-key-checkout");
    // Only Windows adds a second spelling, so the vector is mutated there alone.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut spellings = vec![job_root.join("nested").join("..")];
    #[cfg(windows)]
    spellings.push(PathBuf::from(format!(r"\\?\{}", job_root.display())));
    for publication_root in spellings {
        assert_ne!(publication_root, job_root);
        let epoch = manager.view_publication_epoch(&job_root);
        manager.note_view_pending(&job_root, epoch);
        assert!(manager.note_view_publication(&publication_root));
        assert!(manager.deferred_view_refresh_ready(&publication_root));
        manager.view_refresh_dispatched(&job_root, InspectCategory::DeadCode);
        assert!(
            !manager.deferred_view_refresh_ready(&publication_root),
            "dispatch under {job_root:?} must clear the refresh owed under {publication_root:?}"
        );
    }
}

#[test]
fn inspect_view_pending_unavailability_reuses_request_files() {
    let (_project, _storage, mut job, _request) = view_projection_fixture();
    for directory in 0..10 {
        for file in 0..128 {
            let path = job.project_root.join(format!("dir_{directory}/file_{file}.ts"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "export function pending() {}\n").unwrap();
        }
    }
    eprintln!("IO_MEASURE_REQUEST_WALK_BEGIN");
    job.scope_files = scope_files(&job.project_root, &JobScope::for_project(job.project_root.clone()));
    let stats = job.scope_files.iter().map(|path| {
        let metadata = std::fs::metadata(path).unwrap();
        (path.clone(), metadata.len(), metadata.modified().unwrap())
    }).collect::<Vec<_>>();
    eprintln!("IO_MEASURE_REQUEST_WALK_END files={}", stats.len());
    assert_eq!(stats.len(), 1282);
    let snapshot = InspectSnapshot::new(
        job.project_root.clone(), job.inspect_dir.clone(), job.config.clone(), job.symbol_cache.clone(),
    );
    let manager = InspectManager::new();
    SCOPE_FILE_WALKS.with(|walks| walks.set(0));
    eprintln!("IO_MEASURE_REASON_BEGIN");
    let unavailable = manager.checkout_view_unavailability(&snapshot, &job.scope_files);
    eprintln!("IO_MEASURE_REASON_END");
    assert_eq!(unavailable.kind(), "view_pending");
    let walks = SCOPE_FILE_WALKS.with(std::cell::Cell::get);
    eprintln!("inspect_view_pending_reason extra_project_walks={walks}");
    assert_eq!(walks, 0, "reporting an unavailable view must reuse the request inventory");
}

#[test]
fn views_tier2_published_plane_never_refreshes_legacy() {
    let (_project, _storage, job, request) = view_projection_fixture();
    crate::views::assembly::publish_checkout(&request).unwrap();
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    let snapshot = build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files);
    assert!(snapshot.is_some(), "published view must project");
    assert_eq!(
        LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get),
        before,
        "views-on Tier-2 refreshed the legacy store"
    );
}

/// Reports the phase the verification worker is entering. It is shared, so a
/// thread-local observer installed during a phase can report steps itself.
type PhaseReport = Arc<dyn Fn(&str) + Send + Sync>;

fn run_checkout_verification_with_deadline(
    timeout: Duration,
    verify: impl FnOnce(&PhaseReport) + Send + 'static,
) {
    run_checkout_verification_watchdog(timeout, DeadlineStart::Spawn, verify);
}

/// When the watchdog's deadline starts. The real test bounds everything from
/// the moment the worker is spawned. The watchdog's own self-tests start the
/// clock later, under a generous setup guard, so a slow runner cannot turn
/// "timed out during <phase>" into an earlier phase, and a short deadline
/// keeps them fast: `FirstPhase` at the worker's first report, `AtPhase` once
/// the named phase is reported.
#[derive(Clone, Copy)]
enum DeadlineStart {
    Spawn,
    FirstPhase,
    AtPhase(&'static str),
}

/// Phases the verification worker has entered, in order. Consecutive reports
/// of the same phase (one per source file read, for example) share one row.
/// The deadline covers every phase, so the phase named at a timeout is only
/// the one still running: these offsets show which phase used up the time.
#[derive(Default)]
struct PhaseTimeline {
    rows: Vec<(String, Duration, Duration, usize)>,
}

impl PhaseTimeline {
    fn enter(&mut self, phase: &str, at: Duration) {
        match self.rows.last_mut() {
            Some((last, _, latest, count)) if last == phase => {
                *latest = at;
                *count += 1;
            }
            _ => self.rows.push((phase.to_owned(), at, at, 1)),
        }
    }

    fn current(&self) -> &str {
        self.rows
            .last()
            .map_or("starting worker", |(phase, ..)| phase.as_str())
    }

    fn render(&self) -> String {
        self.rows
            .iter()
            .map(|(phase, first, latest, count)| {
                if *count == 1 {
                    format!("{phase}@{:.1}s", first.as_secs_f64())
                } else {
                    format!(
                        "{phase}@{:.1}s (x{count}, last@{:.1}s)",
                        first.as_secs_f64(),
                        latest.as_secs_f64()
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn run_checkout_verification_watchdog(
    timeout: Duration,
    start: DeadlineStart,
    verify: impl FnOnce(&PhaseReport) + Send + 'static,
) {
    enum Progress {
        Phase(String, Instant),
        Finished(std::thread::Result<()>),
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let mut deadline = started + timeout;
    std::thread::Builder::new()
        .name("inspect-warm-verification".into())
        .spawn(move || {
            // Keep the fixture, thread-local IO counters, and assertions on one
            // thread. Report completion only after fixture cleanup has finished.
            let report: PhaseReport = {
                let tx = tx.clone();
                Arc::new(move |phase: &str| {
                    let _ = tx.send(Progress::Phase(phase.to_owned(), Instant::now()));
                })
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                verify(&report);
            }));
            let _ = tx.send(Progress::Finished(result));
        })
        .expect("spawn inspect warm verification worker");

    let mut timeline = PhaseTimeline::default();
    if !matches!(start, DeadlineStart::Spawn) {
        // A generous hang guard for setup; the deadline under test begins
        // only once the worker reports the phase that starts it.
        let setup_deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = setup_deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(Progress::Phase(phase, at)) => {
                    timeline.enter(&phase, at - started);
                    let starts_deadline = match start {
                        DeadlineStart::AtPhase(name) => phase == name,
                        _ => true,
                    };
                    if starts_deadline {
                        break;
                    }
                }
                Ok(Progress::Finished(Ok(()))) => return,
                Ok(Progress::Finished(Err(panic))) => std::panic::resume_unwind(panic),
                Err(error) => panic!(
                    "inspect warm verification never reached the phase that starts its deadline \
                     ({error}); timeline: {}",
                    timeline.render()
                ),
            }
        }
        deadline = Instant::now() + timeout;
    }
    loop {
        // One absolute deadline includes setup, verification, and cleanup;
        // progress cannot extend it. Do not join a worker that may be blocked.
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Progress::Phase(next, at)) if !remaining.is_zero() => {
                timeline.enter(&next, at - started);
            }
            Ok(Progress::Finished(Ok(()))) => {
                eprintln!(
                    "inspect warm verification finished in {:.1}s; timeline: {}",
                    started.elapsed().as_secs_f64(),
                    timeline.render()
                );
                return;
            }
            Ok(Progress::Finished(Err(panic))) => std::panic::resume_unwind(panic),
            Ok(Progress::Phase(..)) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!(
                    "inspect warm verification timed out after {timeout:?} during {}; timeline: {}",
                    timeline.current(),
                    timeline.render()
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "inspect warm verification worker disconnected during {}",
                    timeline.current()
                );
            }
        }
    }
}

#[test]
fn checkout_verification_deadline_reports_last_phase() {
    let (release, blocked) = std::sync::mpsc::channel();
    let result = std::panic::catch_unwind(|| {
        run_checkout_verification_watchdog(
            Duration::from_secs(1),
            DeadlineStart::FirstPhase,
            move |phase| {
                phase("injected blocked operation");
                blocked.recv_timeout(Duration::from_secs(10)).unwrap();
            },
        );
    });
    // Release the simulated stall even when the assertion below fails.
    let _ = release.send(());
    let panic = result.expect_err("a blocked verification must fail before the libtest timeout");
    let message = panic.downcast_ref::<String>().unwrap();
    // The timeline after the phase name records when each phase began, which
    // varies from run to run; only its shape is fixed.
    assert!(
        message.starts_with(
            "inspect warm verification timed out after 1s during injected blocked operation; \
             timeline: injected blocked operation@"
        ),
        "{message}"
    );
}

#[test]
fn checkout_verification_deadline_preserves_assertion_panics() {
    let result = std::panic::catch_unwind(|| {
        run_checkout_verification_with_deadline(Duration::from_secs(10), |_| {
            panic!("verification assertion failed");
        });
    });
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"verification assertion failed")
    );
}

#[test]
fn inspect_checkout_view_warm_verification_does_not_rehash_corpus() {
    run_checkout_verification_with_deadline(Duration::from_secs(180), verify_checkout_view_corpus);
}

fn verify_checkout_view_corpus(phase: &PhaseReport) {
    // Every count below follows from the corpus size, and none of them needs a
    // large corpus. Keep it small: on a loaded Windows CI runner this fixture
    // (file writes, `git add`, a cold publication) runs about ten times slower
    // than on an idle machine, and a 3000-file corpus took about 20 seconds idle.
    const CORPUS_FILES: usize = 300;
    // IO counters and refusal reports are thread-local. Invalidation tickets
    // are keyed by this private project directory, which has no watcher, so
    // other libtest threads cannot move this fixture's ticket or counters.
    phase("building initial checkout and callgraph fixture");
    let (_project, _storage, mut job, mut request) = view_projection_fixture();
    phase(&format!("writing {CORPUS_FILES} source files"));
    for i in 0..CORPUS_FILES {
        write_projection_cache_file(
            &job.project_root.join(format!("source_{i}.ts")),
            &format!("export function source_{i}() {{ return {i}; }}\n"),
        );
    }
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "large fixture",
        ],
    ] {
        phase(if args[0] == "add" {
            "git add corpus"
        } else {
            "git commit corpus"
        });
        let mut git = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(git.current_dir(&job.project_root));
        assert!(git.args(args).status().unwrap().success());
    }
    phase("reading HEAD tree");
    request.desired_head = crate::views::assembly::head_tree_fingerprint(
        &crate::alias::head_tree_entries(&job.project_root).unwrap(),
    );
    phase("publishing checkout view");
    publish_checkout_reporting_steps(&request, phase);
    phase("walking published source files");
    job.scope_files = crate::callgraph::walk_project_files(&job.project_root).collect();
    phase("creating inspect snapshot and manager");
    let snapshot = InspectSnapshot::new_with_capabilities(
        job.project_root.clone(),
        job.inspect_dir.clone(),
        job.config.clone(),
        job.symbol_cache.clone(),
        false,
        false,
    );
    let manager = InspectManager::new();
    phase("collecting source stats");
    let stats = job
        .scope_files
        .iter()
        .map(|path| {
            let metadata = std::fs::metadata(path).unwrap();
            (path.clone(), metadata.len(), metadata.modified().unwrap())
        })
        .collect::<Vec<_>>();
    crate::views::read::take_verification_io();
    phase("cold checkout verification");
    let cold_view = manager.current_checkout_view(&snapshot, &job.scope_files, Some(&stats));
    assert!(
        cold_view.is_some(),
        "cold checkout view refused: {:?}; root={:?} desired_head={} observed_files={}",
        take_checkout_view_refusal(),
        job.project_root,
        request.desired_head,
        stats.len()
    );
    let cold = crate::views::read::take_verification_io();
    phase("warm checkout verification with known stats");
    assert!(
        manager
            .current_checkout_view(&snapshot, &job.scope_files, Some(&stats))
            .is_some(),
        "warm checkout view refused: {:?}",
        take_checkout_view_refusal()
    );
    let warm = crate::views::read::take_verification_io();
    eprintln!(
        "inspect_view_verification fixture_files={} cold={cold:?} warm={warm:?}",
        job.scope_files.len()
    );
    assert_eq!(
        warm.files_read, 0,
        "unchanged view verification reread source files: {warm:?}"
    );
    assert_eq!(
        warm.bytes_hashed, 0,
        "unchanged view verification rehashed source bytes: {warm:?}"
    );
    assert_eq!(
        warm.files_statd, 0,
        "blocking inspection must reuse the root stats it already collected"
    );
    assert_eq!(
        cold.files_read,
        CORPUS_FILES + 2,
        "first verification must actually read the entire source set"
    );
    phase("standalone checkout verification");
    assert!(
        manager.current_checkout_view(&snapshot, &job.scope_files, None).is_some(),
        "standalone checkout view refused: {:?}",
        take_checkout_view_refusal()
    );
    let standalone = crate::views::read::take_verification_io();
    eprintln!("inspect_view_verification standalone={standalone:?}");
    assert_eq!(standalone.files_statd, CORPUS_FILES + 2);
    assert_eq!(standalone.files_read, 0);
    // A watcher event must defeat the memo even if an external writer preserved
    // both size and mtime. The known root stats deliberately remain unchanged.
    phase("editing source while preserving size and mtime");
    let edited = job.project_root.join("source_0.ts");
    let modified = std::fs::metadata(&edited).unwrap().modified().unwrap();
    std::fs::write(&edited, "export function source_0() { return 9; }\n").unwrap();
    // Windows requires write access to change a file's timestamps.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&edited)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    phase("invalidating verification memo");
    crate::cache_freshness::invalidate_verify_memo(&job.project_root);
    phase("checkout verification after invalidation");
    assert!(manager
        .current_checkout_view(&snapshot, &job.scope_files, Some(&stats))
        .is_none());
    assert!(crate::views::read::take_verification_io().files_read > 0);
    phase("dropping fixture and manager");
}

/// `publish_checkout`, reporting each publication step to the watchdog. A
/// stall then names the step it happened in rather than the whole publication.
fn publish_checkout_reporting_steps(
    request: &crate::views::assembly::AssemblyRequest,
    phase: &PhaseReport,
) {
    publish_checkout_reporting_steps_with_hold(request, phase, None);
}

/// As [`publish_checkout_reporting_steps`]. With `hold`, publication stops
/// right after the named step until the receiver gets a message (or two minutes
/// passes), which simulates a stall in the step that follows it.
fn publish_checkout_reporting_steps_with_hold(
    request: &crate::views::assembly::AssemblyRequest,
    phase: &PhaseReport,
    hold: Option<(&'static str, std::sync::mpsc::Receiver<()>)>,
) {
    // Steps report as they finish, so a stall is in the step after the last one.
    let _observer = crate::views::observe_publication_phases_for_test({
        let phase = Arc::clone(phase);
        move |step| {
            phase(&format!("publishing checkout view: after {step}"));
            if let Some((held_step, release)) = &hold {
                if step == *held_step {
                    let _ = release.recv_timeout(Duration::from_secs(120));
                }
            }
        }
    });
    let mut prepared = crate::views::assembly::prepare_checkout(request, &mut |stage| {
        phase(&format!("publishing checkout view: {stage}"));
        Ok(())
    })
    .unwrap();
    phase("publishing checkout view: committing generation pointer");
    prepared.commit().unwrap();
}

#[test]
fn checkout_verification_deadline_names_the_stalled_publication_step() {
    let (project, storage, _job, request) = view_projection_fixture();
    let (release, held) = std::sync::mpsc::channel();
    let result = std::panic::catch_unwind(|| {
        // The short deadline starts only when the held step reports, so a slow
        // runner spends its time under the 60 s setup guard, not the deadline,
        // and a passing run waits one second rather than a fixed long timeout.
        run_checkout_verification_watchdog(
            Duration::from_secs(1),
            DeadlineStart::AtPhase("publishing checkout view: after derived_clone"),
            move |phase| {
                // The worker outlives the timeout, so it owns the fixture
                // folders and removes them once the released publication ends.
                let _fixture = (project, storage);
                phase("publishing checkout view");
                publish_checkout_reporting_steps_with_hold(
                    &request,
                    phase,
                    Some(("derived_clone", held)),
                );
            },
        );
    });
    // Release the simulated stall even when the assertion below fails.
    let _ = release.send(());
    let panic = result.expect_err("a held publication must time out");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(
        message.starts_with(
            "inspect warm verification timed out after 1s during \
             publishing checkout view: after derived_clone; timeline: \
             publishing checkout view@"
        ),
        "{message}"
    );
    assert!(
        message.contains("publishing checkout view: working_tree_read@"),
        "{message}"
    );
}

#[test]
fn inspect_checkout_view_lookup_never_creates_a_view_folder() {
    let (_project, storage, job, _request) = view_projection_fixture();
    let view_dir = storage
        .path()
        .join("views")
        .join(crate::path_identity::project_scope_key(&job.project_root));
    let _ = std::fs::remove_dir_all(&view_dir);
    let snapshot = InspectSnapshot::new_with_capabilities(
        job.project_root.clone(),
        job.inspect_dir.clone(),
        job.config.clone(),
        job.symbol_cache.clone(),
        false,
        false,
    );
    assert!(InspectManager::new()
        .current_checkout_view(&snapshot, &job.scope_files, None)
        .is_none());
    // A folder made by a reader has no binding and no registry member; it
    // would be left behind as an orphan once the checkout is gone.
    assert!(
        !view_dir.exists(),
        "a projection lookup created {view_dir:?}"
    );
}

#[test]
fn inspect_checkout_view_refusal_diagnostics_name_compared_values() {
    let (_project, storage, job, request) = view_projection_fixture();
    let mut snapshot = InspectSnapshot::new_with_capabilities(
        job.project_root.clone(),
        job.inspect_dir.clone(),
        job.config.clone(),
        job.symbol_cache.clone(),
        false,
        false,
    );
    let manager = InspectManager::new();
    let refusal = |snapshot: &InspectSnapshot, stats: Option<&[(PathBuf, u64, SystemTime)]>| {
        let files = stats.map(|stats| stats.iter().map(|(path, _, _)| path.clone()).collect::<Vec<_>>()).unwrap_or_else(|| job.scope_files.clone());
        assert!(manager.current_checkout_view(snapshot, &files, stats).is_none());
        take_checkout_view_refusal().expect("every checkout refusal must name its branch")
    };
    let unpublished = refusal(&snapshot, None);
    assert!(
        unpublished.contains("projection_unpublished:")
            && unpublished.contains("current_generation=None"),
        "{unpublished}"
    );
    crate::views::assembly::publish_checkout(&request).unwrap();

    Arc::make_mut(&mut snapshot.config).views.enabled = false;
    let disabled = refusal(&snapshot, None);
    assert!(
        disabled.contains("config_gate: views.enabled=false indexes.callgraph=true"),
        "{disabled}"
    );
    Arc::make_mut(&mut snapshot.config).views.enabled = true;

    crate::views::cache_head_fingerprint(job.project_root.clone(), "different-head".to_string());
    let head = refusal(&snapshot, None);
    assert!(
        head.contains("projection_head_mismatch:")
            && head.contains(&request.desired_head)
            && head.contains("cached_head=different-head"),
        "{head}"
    );
    crate::views::cache_head_fingerprint(job.project_root.clone(), request.desired_head.clone());

    // Windows separators are escaped by Debug. Exercise escaping on Unix too,
    // where a backslash is a legal character in a filename.
    let outside = storage.path().join(if cfg!(windows) {
        "outside.ts"
    } else {
        "outside\\fixture.ts"
    });
    let stats = [(outside.clone(), 0, UNIX_EPOCH)];
    let root = refusal(&snapshot, Some(&stats));
    // The diagnostics print Debug paths, not raw path strings. Match their
    // escaping without changing any path used by the production comparison.
    assert!(
        root.contains("coverage_root_mismatch:")
            && root.contains(&format!("path={outside:?}"))
            && root.contains(&format!("root={:?}", job.project_root)),
        "{root}"
    );

    let added = job.project_root.join("untracked.ts");
    write_projection_cache_file(&added, "export function untracked() {}\n");
    let metadata = std::fs::metadata(&added).unwrap();
    let stats = [(added.clone(), metadata.len(), metadata.modified().unwrap())];
    let coverage = refusal(&snapshot, Some(&stats));
    assert!(
        coverage.contains("coverage_manifest_missing:")
            && coverage.contains(&format!("path={added:?}"))
            && coverage.contains("manifest_files=2 observed_files=1"),
        "{coverage}"
    );

    // Supply only the published paths so the untracked-file coverage guard
    // does not hide the source-key comparison we want to diagnose next.
    write_projection_cache_file(
        &job.project_root.join("target.ts"),
        "export function changed() {}\n",
    );
    let stats = job
        .scope_files
        .iter()
        .map(|path| {
            let metadata = std::fs::metadata(path).unwrap();
            (path.clone(), metadata.len(), metadata.modified().unwrap())
        })
        .collect::<Vec<_>>();
    let source = refusal(&snapshot, Some(&stats));
    assert!(
        source.contains("verification_source_key_mismatch:")
            && source.contains("target.ts")
            && source.contains("expected_key=")
            && source.contains("actual_key="),
        "{source}"
    );
}

#[test]
fn views_tier2_pending_plane_never_falls_back_to_legacy() {
    let (_project, _storage, job, _request) = view_projection_fixture();
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(
        build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).is_none(),
        "an unpublished view is pending even when legacy is ready"
    );
    assert_eq!(LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get), before);
}

#[test]
fn views_tier2_and_legacy_dead_code_verdicts_match() {
    let (_project, _storage, mut job, request) = view_projection_fixture();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    for source in [
        "export function used() {}\nexport function dead() {}\n",
        "export function used() { dead(); }\nexport function dead() {}\n",
        "export function used() {}\nexport function dead() {}\n",
        "export function used() { alternate(); }\nexport function alternate() {}\n",
        "export function used() {}\nexport function dead() {}\n",
    ] {
        write_projection_cache_file(&job.project_root.join("target.ts"), source);
        crate::views::assembly::publish_checkout(&request).unwrap();
        // Publication makes the immutable generation current before its
        // detached checkpoint closes the last writer; wait for readability.
        crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());
        Arc::make_mut(&mut job.config).views.enabled = true;
        let view =
            build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).unwrap();
        Arc::make_mut(&mut job.config).views.enabled = false;
        let legacy =
            build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).unwrap();
        job.callgraph_snapshot = Some(legacy);
        let contributions = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
            .outcome
            .unwrap()
            .contributions;
        assert!(!contributions.is_empty());
        let legacy_verdict = roll_up_dead_code_contributions(&job, &contributions, None);
        job.callgraph_snapshot = Some(view);
        let view_verdict = roll_up_dead_code_contributions(&job, &contributions, None);
        assert_eq!(
            view_verdict, legacy_verdict,
            "same contribution set after source transition: {source}"
        );
    }
}

#[test]
fn views_navigation_shared_reader_preserves_pinned_generation() {
    let (_project, _storage, _job, mut request) = view_projection_fixture();
    let first = crate::views::assembly::publish_checkout(&request)
        .unwrap()
        .generation
        .unwrap();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    let pin = Some(Arc::new(
        crate::pins::QueryPin::acquire(view.view_dir(), &first).unwrap(),
    ));
    write_projection_cache_file(
        &request.project_root.join("target.ts"),
        "export function used() { return 2; }\n",
    );
    request.changed_paths.insert(b"target.ts".to_vec());
    let second = crate::views::assembly::publish_checkout(&request)
        .unwrap()
        .generation
        .unwrap();
    assert_ne!(first, second);
    let before = ReadonlyCallGraphStore::open_manifest_view(
        request.project_root.clone(),
        request.family.clone(),
        view.view_dir().to_path_buf(),
        &first,
        pin.clone(),
    )
    .unwrap();
    let after = crate::views::read::open_published_callgraph(
        request.project_root,
        request.family,
        view.view_dir().to_path_buf(),
        &first,
        pin,
    )
    .unwrap();
    assert_eq!(before.sqlite_path(), after.sqlite_path());
    assert_eq!(after.sqlite_path(), view.derived_path(&first).unwrap());
}

#[test]
fn views_tier2_head_mismatch_is_pending() {
    let (_project, _storage, job, request) = view_projection_fixture();
    crate::views::assembly::publish_checkout(&request).unwrap();
    write_projection_cache_file(
        &request.project_root.join("target.ts"),
        "export function used() { return 2; }\n",
    );
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.name=AFT",
            "-c",
            "user.email=aft@example.com",
            "commit",
            "--quiet",
            "-m",
            "changed",
        ],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(&request.project_root));
        assert!(command.args(args).status().unwrap().success());
    }
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(build_tier2_callgraph_snapshot_with_refresh(&job, true, &job.scope_files).is_none());
    assert_eq!(LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get), before);
}

#[test]
fn views_tier2_watcher_edit_is_pending_until_publication() {
    let (_project, _storage, job, mut request) = view_projection_fixture();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    crate::views::assembly::publish_checkout(&request).unwrap();
    crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());
    let path = request.project_root.join("target.ts");
    write_projection_cache_file(&path, "export function used() { return 2; }\n");
    let before = LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get);
    assert!(
        build_tier2_callgraph_snapshot_with_refresh(&job, true, std::slice::from_ref(&path))
            .is_none()
    );
    request.changed_paths.insert(b"target.ts".to_vec());
    crate::views::assembly::publish_checkout(&request).unwrap();
    // Publication schedules a detached checkpoint whose keeper close can briefly
    // contend with a zero-wait reader. Test content freshness after it settles.
    crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());
    assert!(build_tier2_callgraph_snapshot_with_refresh(&job, true, &[path]).is_some());
    assert_eq!(LEGACY_VIEW_REFRESHES.with(std::cell::Cell::get), before);
}

#[test]
fn views_projection_adapter_refuses_legacy_reader() {
    let (_project, _storage, job, _request) = view_projection_fixture();
    let dir = callgraph_store_dir_from_inspect_dir(&job.inspect_dir, &job.project_root).unwrap();
    let legacy = CallGraphStore::open_readonly(dir, job.project_root.clone())
        .unwrap()
        .unwrap();
    let error = crate::callgraph_store::project_dead_code_snapshot_from_view(&legacy).unwrap_err();
    assert!(error.to_string().contains("requires a view reader"));
}

#[test]
#[ignore = "manual read latency benchmark requires AFT_HUNT_ROOT and AFT_HUNT_STORAGE for completed drill artifacts"]
fn views_profile_navigation_reads_on_drill_artifacts() {
    let root = PathBuf::from(std::env::var_os("AFT_HUNT_ROOT").expect("AFT_HUNT_ROOT"));
    let storage = PathBuf::from(std::env::var_os("AFT_HUNT_STORAGE").expect("AFT_HUNT_STORAGE"));
    let family = crate::search_index::artifact_cache_key(&root);
    let view =
        crate::views::ViewStore::open(&storage, &crate::path_identity::project_scope_key(&root))
            .unwrap();
    let generation = view.current_generation().unwrap().unwrap();
    let pin = Some(Arc::new(
        crate::pins::QueryPin::acquire(view.view_dir(), &generation).unwrap(),
    ));
    let legacy =
        CallGraphStore::open_readonly(storage.join("callgraph").join(&family), root.clone())
            .unwrap()
            .unwrap();
    let path = Path::new("packages/app/src/settings/timeline-detail.tsx");
    let symbol = "TimelineDetailControl";
    let mut rows = Vec::new();
    for use_view in [false, true] {
        let mut opens = Duration::ZERO;
        let mut callers = Duration::ZERO;
        let mut impact = Duration::ZERO;
        let mut caller_count = 0;
        let mut impact_count = 0;
        for _ in 0..10 {
            let start = Instant::now();
            let selected = use_view.then(|| {
                crate::views::read::open_published_callgraph(
                    root.clone(),
                    family.clone(),
                    view.view_dir().to_path_buf(),
                    &generation,
                    pin.clone(),
                )
                .unwrap()
            });
            opens += start.elapsed();
            let reader = selected.as_ref().unwrap_or(&legacy);
            let start = Instant::now();
            caller_count = reader.callers_of(path, symbol, 3).unwrap().callers.len();
            callers += start.elapsed();
            let start = Instant::now();
            impact_count = reader.impact_of(path, symbol, 3).unwrap().callers.len();
            impact += start.elapsed();
        }
        assert!(caller_count > 0 && impact_count > 0);
        eprintln!("navigation_read source={} queries=10 open_us={} callers_us={} impact_us={} callers={} impacted={}", if use_view { "view_reopened" } else { "legacy_retained" }, opens.as_micros()/10, callers.as_micros()/10, impact.as_micros()/10, caller_count, impact_count);
        rows.push((caller_count, impact_count));
    }
    assert_eq!(rows[0], rows[1]);
}

#[test]
fn views_tier2_keyless_generation_reports_callgraph_disabled_not_empty() {
    let (_project, _storage, job, mut request) = view_projection_fixture();
    // Publish the current generation without call graph data, as a session
    // with the call graph off does.
    request.callgraph = false;
    crate::views::assembly::publish_checkout(&request).unwrap();
    // Both with and without paths to verify against the generation: the
    // path check alone already rejects a keyless entry, so the projection
    // with no refresh paths is the one the generation-wide guard protects.
    for refresh_paths in [&job.scope_files[..], &[]] {
        assert!(
            build_tier2_callgraph_snapshot_with_refresh(&job, true, refresh_paths).is_none(),
            "a generation without call graph data must not project an empty graph"
        );
    }
    let aggregate =
        crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_for_job(&job);
    assert_eq!(aggregate["callgraph_available"], false);
    assert_eq!(
        aggregate["callgraph_unavailable_reason"],
        crate::views::read::CALLGRAPH_DISABLED
    );
    assert_eq!(aggregate["notes"][1], "callgraph_disabled");
    // With the index itself off, the reason names that configuration setting.
    let mut off = job.clone();
    Arc::make_mut(&mut off.config).indexes.callgraph = false;
    let aggregate =
        crate::inspect::scanners::dead_code::callgraph_unavailable_aggregate_for_job(&off);
    assert_eq!(
        aggregate["callgraph_unavailable_reason"],
        "call graph is disabled (indexes.callgraph=false)"
    );
}

#[test]
fn views_keyless_callgraph_generation_preserves_unused_exports_but_names_dead_code_gap() {
    let (_project, _storage, mut job, mut request) = view_projection_fixture();
    request.callgraph = false;
    crate::views::assembly::publish_checkout(&request).unwrap();
    let view = crate::views::ViewStore::open(&request.storage, &request.scope).unwrap();
    crate::views::wait_for_derived_checkpoint_for_test(view.view_dir());

    // Unused exports is an independent import/export analysis, not a call-graph
    // projection. A generation without graph keys must preserve its real findings
    // while dead-code reachability remains unknown, never a zero-findings result.
    job.category = InspectCategory::UnusedExports;
    let scan = crate::inspect::scanners::unused_exports::run_unused_exports_scan(&job)
        .outcome
        .unwrap();
    let unused = roll_up_unused_exports_contributions(&job, &scan.contributions, None);
    assert_eq!(unused["count"], 2, "{unused:#}");
    // Item order is not part of this contract (it differs across platforms);
    // what matters is that both real findings survive.
    let mut symbols = unused["items"]
        .as_array()
        .expect("unused export items")
        .iter()
        .map(|item| item["symbol"].as_str().unwrap_or_default().to_string())
        .collect::<Vec<_>>();
    symbols.sort();
    assert_eq!(symbols, ["dead", "main"], "{unused:#}");

    job.category = InspectCategory::DeadCode;
    // No refresh-path check: this specifically exercises the generation guard,
    // rather than an unrelated mismatch between source bytes and missing keys.
    job.callgraph_snapshot = build_tier2_callgraph_snapshot_with_refresh(&job, true, &[]);
    let dead = crate::inspect::scanners::dead_code::run_dead_code_scan(&job)
        .outcome
        .unwrap()
        .aggregate;
    assert_eq!(dead["callgraph_available"], false, "{dead:#}");
    assert_eq!(
        dead["callgraph_unavailable_reason"],
        crate::views::read::CALLGRAPH_DISABLED,
        "{dead:#}"
    );
    assert!(
        dead.get("count").is_none(),
        "unknown dead code is not zero findings: {dead:#}"
    );
    assert_eq!(
        roll_up_dead_code_contributions(&job, &scan.contributions, None),
        dead
    );
}
