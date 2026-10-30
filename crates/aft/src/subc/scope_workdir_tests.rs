use super::*;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

thread_local! {
    static COMPARISON_OBSERVER: std::cell::RefCell<Option<oneshot::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn take_comparison_observer() -> Option<oneshot::Sender<()>> {
    COMPARISON_OBSERVER.with(|observer| observer.borrow_mut().take())
}

fn observe_next_comparison() -> oneshot::Receiver<()> {
    let (tx, rx) = oneshot::channel();
    COMPARISON_OBSERVER.with(|observer| assert!(observer.borrow_mut().replace(tx).is_none()));
    rx
}

static TEST_SERIAL: StdMutex<()> = StdMutex::new(());

struct DiagnosticTest {
    _serial: std::sync::MutexGuard<'static, ()>,
}

fn serial() -> DiagnosticTest {
    DiagnosticTest {
        _serial: TEST_SERIAL
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    }
}

fn wait_for_diagnostics_idle() {
    let diagnostics = &*SCOPE_WORKDIR_DIAGNOSTICS;
    let (in_flight, _) = diagnostics
        .idle
        .wait_timeout_while(
            diagnostics
                .in_flight
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            Duration::from_secs(10),
            |in_flight| !in_flight.is_empty(),
        )
        .unwrap_or_else(|error| error.into_inner());
    if !std::thread::panicking() {
        assert!(
            in_flight.is_empty(),
            "filesystem diagnostics did not finish: {in_flight:?}"
        );
    }
}

impl Drop for DiagnosticTest {
    fn drop(&mut self) {
        wait_for_diagnostics_idle();
    }
}

fn stamp() -> ScopeStamp {
    ScopeStamp::new(Principal::Direct, "session-scope", 4, ScopeKind::Head)
}

fn canonical_root(path: &Path) -> PathBuf {
    ProjectRootId::from_path(path).unwrap().into_path_buf()
}

// Exercise admission and completion, not a second implementation of root selection.
fn bind(
    root: &Path,
    scope: Option<ScopeStamp>,
    expected_project_root: &Path,
) -> (Vec<String>, Vec<u8>) {
    bind_observed(root, scope, expected_project_root, |_, _, _| {})
}

fn bind_observed(
    root: &Path,
    scope: Option<ScopeStamp>,
    expected_project_root: &Path,
    after_ack: impl FnOnce(&mut Option<oneshot::Receiver<()>>, &RouteIdentity, &AppContext),
) -> (Vec<String>, Vec<u8>) {
    // The bind resolves its storage root through the process environment,
    // where AFT_STORAGE_DIR overrides the configured root. A test that sets
    // it holds this lock; binding meanwhile would open that test's storage
    // (and wait on its live databases) before acknowledging.
    let _env = crate::test_env::process_env_lock();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (reply, lines) = crate::logging::capture_log_lines(|| {
        runtime.block_on(async {
            let app = App::default_shared();
            let storage = tempfile::tempdir().unwrap();
            let executor = Arc::new(Executor::new());
            let metrics = Arc::new(DispatchPathMetrics::new());
            let (writer_tx, mut writer_rx) = mpsc::channel(8);
            let (completion_tx, mut completion_rx) = mpsc::channel(8);
            let a0 = A0Egress::new(writer_tx.clone(), completion_tx.clone());
            let (lossy_tx, _lossy_rx) = mpsc::channel(8);
            let (reliable_tx, _reliable_rx) = mpsc::unbounded_channel();
            let senders = PushSenders {
                lossy_tx,
                reliable_tx,
                lossy_overflow: Arc::new(push::LossyOverflow::default()),
                lossy_seq: Arc::new(AtomicU64::new(0)),
                fleet_status_client: FleetStatusClient::channel(1).0,
            };
            let request = ModuleControlRequest::RouteBind {
                route_channel: 7,
                epoch: 1,
                target: RouteTarget::ToolProvider {
                    module_id: "aft".into(),
                },
                identity: subc_protocol::BindIdentity::new(root, "opencode", "folder-session"),
                principal: Some(Principal::Direct),
                consumer_capabilities: None,
                admission_facts: Default::default(),
                scope: scope.clone(),
                role_versions: None,
                call_mode: None,
            };
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                91,
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
            let mut pending = HashMap::new();
            let mut installed = HashMap::new();
            let mut routes = HashMap::new();
            let mut observed = scope
                .as_ref()
                .and_then(|stamp| stamp.workdir.as_ref())
                .map(|_| observe_next_comparison());
            handle_control_request(
                &writer_tx,
                &writer_tx,
                &a0,
                &frame,
                &app,
                &executor,
                &Arc::new(standing::StandingActor::new(app.clone(), executor.clone())),
                &mut HashMap::new(),
                &mut pending,
                &mut installed,
                &mut routes,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &Arc::new(StdMutex::new(HashMap::new())),
                &mut PendingSubcResponses::default(),
                &mut RetryBuffer::new(),
                &mut HashMap::new(),
                &Arc::new(Notify::new()),
                &completion_tx,
                &metrics,
                None,
                &HealthRollupCache::new(),
                &senders,
                |request, ctx| {
                    let configured = crate::commands::configure::handle_configure(&request, ctx);
                    if !configured.success {
                        return configured;
                    }
                    Response::success(request.id, request.params)
                },
                // The bind identity folder may not exist (stamp-only binds), so the
                // user config file goes in the project folder the route resolves to.
                Some(&test_support::bind_config_without_indexes(
                    expected_project_root,
                )),
                usize::MAX,
                None,
                storage.path(),
            )
            .await
            .unwrap();
            let completion = tokio::time::timeout(Duration::from_secs(30), completion_rx.recv())
                .await
                .unwrap()
                .unwrap();
            let expected_root = ProjectRootId::from_path(expected_project_root).unwrap();
            let ControlCompletion::ConfigureReady(completion) = completion else {
                panic!("configure completion expected");
            };
            assert!(completion.configure_response.success);
            assert_eq!(completion.bind_root_id, expected_root);
            assert_eq!(completion.identity.root, expected_root);
            assert_eq!(completion.identity.project_root, expected_project_root);
            assert_eq!(completion.identity.scope, scope);
            assert_eq!(completion.identity.trust, BindTrust::FirstParty);
            match &completion.identity.spawn_principal {
                AuthenticatedPrincipal::RouteBind {
                    project_root,
                    session_id,
                    ..
                } => {
                    assert_eq!(project_root, expected_project_root);
                    assert_eq!(session_id, "folder-session");
                }
                other => panic!("unexpected spawn principal: {other:?}"),
            }
            assert_eq!(
                completion.configure_response.data["project_root"],
                expected_project_root.to_string_lossy().as_ref()
            );
            assert_eq!(executor.actor_count(), 1);
            assert_eq!(executor.actor_entries()[0].0, expected_root);
            handle_route_bind_completion(
                &writer_tx,
                completion,
                false,
                &a0,
                &mut routes,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut pending,
                &mut installed,
                &executor,
                &Arc::new(Notify::new()),
                &metrics,
                None,
            )
            .await
            .unwrap();
            assert!(routes.is_empty(), "route must wait for ACK write");
            let mut ack = writer_rx.recv().await.expect("successful bind reply");
            assert!(ack.begin_ack_write());
            ack.finish_ack_write(true);
            let ControlCompletion::AckWritten(completion) = completion_rx.recv().await.unwrap()
            else {
                panic!("written ACK expected");
            };
            handle_route_bind_completion(
                &writer_tx,
                completion,
                true,
                &a0,
                &mut routes,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut pending,
                &mut installed,
                &executor,
                &Arc::new(Notify::new()),
                &metrics,
                None,
            )
            .await
            .unwrap();
            assert_eq!(routes[&route_key(7, 1)].root, expected_root);
            assert_eq!(ack.header.ty, FrameType::Response);
            assert_eq!(
                (ack.header.channel, ack.header.epoch, ack.header.corr),
                (0, 0, 91)
            );
            let identity = &routes[&route_key(7, 1)];
            let ctx = executor.actor_context(&identity.root).unwrap();
            after_ack(&mut observed, identity, &ctx);
            if let Some(observed) = observed {
                tokio::time::timeout(Duration::from_secs(8), observed)
                    .await
                    .unwrap()
                    .unwrap();
            }
            ack.body.clone()
        })
    });
    let key_changes: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("subc route project key changed:"))
        .collect();
    if root.display().to_string() != expected_project_root.display().to_string() {
        assert_eq!(key_changes.len(), 1, "{key_changes:?}");
        assert!(key_changes[0].starts_with("INFO "), "{key_changes:?}");
        for value in [root, expected_project_root] {
            assert!(
                key_changes[0].contains(&value.display().to_string()),
                "{key_changes:?}"
            );
        }
    } else {
        assert!(key_changes.is_empty(), "{key_changes:?}");
    }
    let lines = lines
        .into_iter()
        .filter(|line| line.contains("subc scope workdir"))
        .collect();
    (lines, reply)
}

#[cfg(unix)]
#[test]
fn scope_workdir_held_lookup_does_not_delay_bind_ack() {
    let _serial = serial();
    // Taken before the clock starts: waiting for another test to release the
    // environment is not bind latency. `bind_observed` takes it again.
    let _env = crate::test_env::process_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let workdir = root.join("held-workdir");
    std::fs::create_dir(&workdir).unwrap();
    let held = crate::bounded_io::stall_running_helper(&workdir);
    let started = Instant::now();
    let (lines, reply) = bind_observed(
        &root,
        Some(stamp().with_workdir(Some(workdir.to_string_lossy().into_owned()))),
        &workdir,
        |observed, _, _| {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "diagnostic delayed bind ACK by {:?}",
                started.elapsed()
            );
            held.reached
                .recv_timeout(Duration::from_secs(1))
                .expect("workdir lookup reached the held helper");
            assert!(
                matches!(
                    observed.as_mut().unwrap().try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ),
                "comparison finished while its lookup was held"
            );
            drop(held);
        },
    );
    assert_eq!(reply, br#"{"op":"route.bind"}"#);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("subc scope workdir differs:"),
        "{lines:?}"
    );
}

#[cfg(unix)]
#[test]
fn scope_workdir_expired_lookup_logs_skipped_without_guessing() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let workdir = root.join("timed-out-workdir");
    std::fs::create_dir(&workdir).unwrap();
    let held = crate::bounded_io::stall_running_helper(&workdir);
    let (lines, reply) = bind_observed(
        &root,
        Some(stamp().with_workdir(Some(workdir.to_string_lossy().into_owned()))),
        &workdir,
        |_, _, _| {
            held.reached
                .recv_timeout(Duration::from_secs(1))
                .expect("workdir lookup reached the held helper");
        },
    );
    drop(held);
    assert_eq!(reply, br#"{"op":"route.bind"}"#);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("subc scope workdir comparison skipped:"),
        "{lines:?}"
    );
    assert!(!lines[0].contains("workdir differs:"), "{lines:?}");
}

#[cfg(unix)]
#[test]
fn scope_workdir_repeated_binds_keep_one_stuck_diagnostic_in_flight() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let workdir = root.join("stuck-workdir");
    std::fs::create_dir(&workdir).unwrap();
    let scope = stamp().with_workdir(Some(workdir.to_string_lossy().into_owned()));
    let held = crate::bounded_io::stall_running_helper(&workdir);
    let (lines, _) = bind_observed(&root, Some(scope.clone()), &workdir, |_, _, _| {
        held.reached.recv_timeout(Duration::from_secs(1)).unwrap();
    });
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("comparison skipped:"), "{lines:?}");
    for _ in 0..5 {
        assert_eq!(SCOPE_WORKDIR_DIAGNOSTICS.in_flight.lock().unwrap().len(), 1);
        let (lines, reply) = bind(&root, Some(scope.clone()), &workdir);
        assert_eq!(reply, br#"{"op":"route.bind"}"#);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("reason=workdir_already_in_flight"),
            "{lines:?}"
        );
    }
    assert_eq!(SCOPE_WORKDIR_DIAGNOSTICS.in_flight.lock().unwrap().len(), 1);
    drop(held);
    wait_for_diagnostics_idle();
    let (lines, _) = bind(&root, Some(scope), &workdir);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("workdir differs:"), "{lines:?}");
}

#[cfg(unix)]
#[test]
fn scope_workdir_full_diagnostic_capacity_preserves_shared_file_reads() {
    const CHILD_ENV: &str = "AFT_SCOPE_WORKDIR_READ_POOL_CHILD";
    const TEST: &str = "subc::scope_workdir_tests::scope_workdir_full_diagnostic_capacity_preserves_shared_file_reads";
    if std::env::var_os(CHILD_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        // The read pool is process-wide. Reexec only this measurement so unrelated
        // parallel tests can neither occupy its workers nor be starved by it.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("start isolated read-pool measurement");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "isolated read-pool measurement failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("test result: ok. 1 passed;"),
            "isolated read-pool measurement did not run exactly one test:\n{stdout}\n{stderr}"
        );
        return;
    }
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let healthy_file = root.join("healthy.txt");
    std::fs::write(&healthy_file, b"healthy shared read").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (_, lines) = crate::logging::capture_log_lines(|| {
        runtime.block_on(async {
            // Leave only two shared read workers free, so routing the two held
            // diagnostics back to that pool would starve an actual cold file read.
            let shared_capacity = crate::bounded_io::helper_health()["pool_size"]
                .as_u64()
                .unwrap() as usize;
            let mut busy_reads = Vec::new();
            for index in 0..shared_capacity - MAX_SCOPE_WORKDIR_DIAGNOSTICS {
                let path = root.join(format!("busy-read-{index}"));
                let held = crate::bounded_io::stall_running_helper(&path);
                let worker = std::thread::spawn(move || {
                    let budget = crate::bounded_io::Budget::after(Duration::from_secs(30))
                        .with_access_budget(Duration::from_secs(30));
                    crate::bounded_io::with_budget(Some(budget), || {
                        crate::bounded_io::run(&path, None, |_| Ok(()))
                    })
                });
                held.reached.recv_timeout(Duration::from_secs(2)).unwrap();
                busy_reads.push((held, worker));
            }
            let identity = subc_protocol::BindIdentity::new(&root, "opencode", "capacity-session");
            let mut held_diagnostics = Vec::new();
            let mut observed = Vec::new();
            for index in 0..MAX_SCOPE_WORKDIR_DIAGNOSTICS {
                let path = root.join(format!("held-workdir-{index}"));
                let held = crate::bounded_io::stall_running_helper(&path);
                observed.push(observe_next_comparison());
                let scope = stamp().with_workdir(Some(path.to_string_lossy().into_owned()));
                schedule_scope_workdir_comparison(&identity, Some(&scope));
                held.reached.recv_timeout(Duration::from_secs(1)).unwrap();
                held_diagnostics.push(held);
            }
            assert_eq!(
                SCOPE_WORKDIR_DIAGNOSTICS.in_flight.lock().unwrap().len(),
                MAX_SCOPE_WORKDIR_DIAGNOSTICS
            );
            let scope = stamp().with_workdir(Some(
                root.join("another-workdir").to_string_lossy().into_owned(),
            ));
            schedule_scope_workdir_comparison(&identity, Some(&scope));
            let duplicate = stamp().with_workdir(Some(
                root.join("held-workdir-0").to_string_lossy().into_owned(),
            ));
            schedule_scope_workdir_comparison(&identity, Some(&duplicate));
            let bytes = crate::bounded_io::with_budget(
                Some(crate::bounded_io::Budget::after(Duration::from_secs(1))),
                || crate::bounded_io::read(&healthy_file, None),
            )
            .expect("healthy shared-pool file read was starved by diagnostics");
            assert_eq!(bytes, b"healthy shared read");
            assert_eq!(
                SCOPE_WORKDIR_DIAGNOSTICS.in_flight.lock().unwrap().len(),
                MAX_SCOPE_WORKDIR_DIAGNOSTICS
            );
            drop(held_diagnostics);
            for (held, worker) in busy_reads {
                drop(held);
                worker.join().unwrap().unwrap();
            }
            for observer in observed {
                tokio::time::timeout(Duration::from_secs(8), observer)
                    .await
                    .unwrap()
                    .unwrap();
            }
        })
    });
    let skipped: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("subc scope workdir comparison skipped:"))
        .collect();
    assert_eq!(skipped.len(), 2, "{lines:?}");
    assert!(
        skipped
            .iter()
            .any(|line| line.contains("reason=diagnostic_capacity_exhausted")),
        "{lines:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|line| line.contains("reason=workdir_already_in_flight")),
        "{lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("subc scope workdir differs:"))
            .count(),
        MAX_SCOPE_WORKDIR_DIAGNOSTICS,
        "{lines:?}"
    );
}

#[test]
fn scope_workdir_matching_is_silent_and_preserves_project_root() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let (lines, _) = bind(
        &root,
        Some(
            stamp()
                .with_workdir(Some(root.join(".").to_string_lossy().into_owned()))
                .with_project_id(Some("project-match".into())),
        ),
        &root,
    );
    assert!(lines.is_empty(), "{lines:?}");
    #[cfg(unix)]
    {
        let alias = root.join("folder-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let (lines, _) = bind(
            &alias,
            Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
            &root,
        );
        assert!(lines.is_empty(), "{lines:?}");
    }
}

#[test]
fn scope_workdir_mismatch_logs_once_and_binds_stamped_project() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let folder = root.join("session-folder");
    std::fs::create_dir(&folder).unwrap();
    let expected_root = ProjectRootId::from_path(&root).unwrap();
    let expected_folder = expected_root.as_path().join("session-folder");
    let (lines, _) = bind(
        &root,
        Some(
            stamp()
                .with_workdir(Some(folder.to_string_lossy().into_owned()))
                .with_project_id(Some("project-mismatch".into())),
        ),
        &folder,
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(
        line.starts_with("INFO subc scope workdir differs:"),
        "{line}"
    );
    for value in [
        expected_root.as_path().to_str().unwrap(),
        expected_folder.to_str().unwrap(),
        "folder-session",
        "project-mismatch",
    ] {
        assert!(line.contains(value), "missing {value}: {line}");
    }
}

#[test]
fn scope_workdir_old_stamp_is_silent_and_preserves_project_root() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let old_stamp: ScopeStamp = serde_json::from_str(
        r#"{"owner":{"kind":"direct"},"ref":"session-scope","scope_epoch":4,"kind":"head","owner_authorized":false}"#,
    ).unwrap();
    assert!(old_stamp.workdir.is_none());
    assert!(old_stamp.project_id.is_none());
    assert_eq!(serde_json::to_vec(&old_stamp).unwrap(), br#"{"owner":{"kind":"direct"},"ref":"session-scope","scope_epoch":4,"kind":"head","owner_authorized":false}"#);
    let (lines, old_reply) = bind(&root, Some(old_stamp), &root);
    let (unscoped_lines, unscoped_reply) = bind(&root, None, &root);
    assert!(lines.is_empty(), "{lines:?}");
    assert!(unscoped_lines.is_empty(), "{unscoped_lines:?}");
    assert_eq!(old_reply, unscoped_reply);
    let literal_root = root.join(".");
    let (lines, _) = bind(&literal_root, None, &literal_root);
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn scope_workdir_stamp_binds_without_a_valid_bind_identity_folder() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let missing_bind_root = root.join("absent-bind-folder");
    let (lines, _) = bind(
        &missing_bind_root,
        Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
        &root,
    );
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn scope_workdir_stamp_selects_project_config_instead_of_bind_config() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let project = root.join("stamped-project");
    let bind_folder = root.join("bind-folder");
    for (folder, tool) in [(&project, "aft_search"), (&bind_folder, "aft_zoom")] {
        let config_dir = folder.join(".cortexkit");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("aft.jsonc"),
            json!({"harnesses": {"opencode": {"disabled_tools": [tool]}}}).to_string(),
        )
        .unwrap();
    }
    bind_observed(
        &bind_folder,
        Some(stamp().with_workdir(Some(project.to_string_lossy().into_owned()))),
        &project,
        |_, identity, _| {
            assert!(identity
                .disabled_tools
                .iter()
                .any(|tool| tool == "aft_search"));
            assert!(!identity
                .disabled_tools
                .iter()
                .any(|tool| tool == "aft_zoom"));
        },
    );
}

#[test]
fn scope_workdir_stamp_recovers_legacy_catalog_state_for_same_path() {
    use crate::db::remote_exec::{self, PolicyKey};
    use crate::exec_remote::FrozenParams;

    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let project = root.join("legacy-project");
    let bind_folder = root.join("pool-folder");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&bind_folder).unwrap();
    let db_path = root.join("aft.db");
    // Seed the literal path key written before scope workdirs selected projects.
    // The daemon's project id deliberately differs and must not become the key.
    let legacy_key = PolicyKey {
        project_root: project.display().to_string(),
        harness: "opencode".into(),
        session: "folder-session".into(),
        principal: "direct".into(),
        owner: serde_json::to_string(&Principal::Direct).unwrap(),
        scope_ref: "session-scope".into(),
        epoch: 4,
        preset: "worker".into(),
    };
    let policy = FrozenParams {
        siblings: vec!["saved-sibling".into()],
        host: Some("saved-host".into()),
        ..Default::default()
    };
    let conn = crate::db::open(&db_path).unwrap();
    remote_exec::freeze(&conn, &legacy_key, &policy, 1, || {}).unwrap();
    drop(conn);

    let scope = ScopeStamp::new(Principal::Direct, "session-scope", 4, ScopeKind::Worker)
        .with_workdir(Some(project.to_string_lossy().into_owned()))
        .with_project_id(Some("daemon-project-id-not-a-path".into()));
    bind_observed(&bind_folder, Some(scope), &project, |_, identity, ctx| {
        ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&db_path).unwrap())));
        let key = remote_policy::key(identity, Some("worker")).unwrap();
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        let recovered = remote_exec::lookup(&conn, &key, 2)
            .unwrap()
            .expect("stamped route must find the legacy root-keyed catalog row");
        assert_eq!(recovered.host.as_deref(), Some("saved-host"));
        assert_eq!(recovered.siblings, ["saved-sibling"]);
    });
}

#[test]
fn scope_workdir_bind_reply_is_byte_for_byte_unchanged() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let other = root.join("other");
    std::fs::create_dir(&other).unwrap();
    for (scope, expected) in [
        (None, &root),
        (Some(stamp()), &root),
        (
            Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
            &root,
        ),
        (
            Some(
                stamp()
                    .with_workdir(Some(other.to_string_lossy().into_owned()))
                    .with_project_id(Some("project-other".into())),
            ),
            &other,
        ),
    ] {
        let (_, reply) = bind(&root, scope, expected);
        assert_eq!(reply, br#"{"op":"route.bind"}"#);
    }
}
