use super::*;
use subc_protocol::scope::{ScopeKind, ScopeStamp};

fn stamp() -> ScopeStamp {
    ScopeStamp::new(Principal::Direct, "session-scope", 4, ScopeKind::Head)
}

fn canonical_root(path: &Path) -> PathBuf {
    ProjectRootId::from_path(path).unwrap().into_path_buf()
}

// Exercise admission and completion, not a second implementation of root selection.
fn bind(root: &Path, scope: Option<ScopeStamp>, expected_project_root: &Path) -> Vec<u8> {
    bind_observed(Some(root), scope, expected_project_root, |_, _| {})
}

fn bind_observed(
    root: Option<&Path>,
    scope: Option<ScopeStamp>,
    expected_project_root: &Path,
    after_ack: impl FnOnce(&RouteIdentity, &AppContext),
) -> Vec<u8> {
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
            let request = {
                let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
                    route_channel: 7,
                    epoch: 1,
                    target: RouteTarget::ToolProvider {
                        module_id: "aft".into(),
                    },
                    identity: subc_protocol::BindIdentity::new("opencode", "folder-session"),
                    principal: Some(Principal::Direct),
                    consumer_capabilities: None,
                    admission_facts: Default::default(),
                    scope: scope.clone(),
                    role_versions: None,
                    call_mode: None,
                })
                .unwrap();
                if let Some(root) = root {
                    bind["identity"]["project_root"] = serde_json::json!(root);
                }
                bind
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
                // Exercise config loading in the selected project, but keep
                // rootless fixture files in temporary storage rather than /.
                Some(&test_support::bind_config_without_indexes(
                    if expected_project_root.as_os_str() == "/" {
                        storage.path()
                    } else {
                        expected_project_root
                    },
                )),
                usize::MAX,
                None,
                storage.path(),
            )
            .await
            .unwrap();
            if expected_project_root.as_os_str() == "/" {
                // No actor means no configured context or index can own /.
                assert_eq!(executor.actor_count(), 0);
                assert!(executor.actor_entries().is_empty());
                assert_eq!(app.watcher_count(), 0);
                assert!(completion_rx.try_recv().is_err());
                let ack = writer_rx.recv().await.expect("project-free bind ACK");
                assert_eq!(ack.header.ty, FrameType::Response);
                assert_eq!((ack.header.channel, ack.header.corr), (0, 91));
                return ack.body.clone();
            }
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
            after_ack(identity, &ctx);
            ack.body.clone()
        })
    });
    let _ = lines;
    reply
}

#[test]
fn scope_workdir_bind_without_project_fields_configures_the_stamped_project() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let reply = bind_observed(
        None,
        Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
        &root,
        |_, _| {},
    );
    assert_eq!(reply, br#"{"op":"route.bind"}"#);
}

#[test]
fn scope_workdir_legacy_identity_root_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let decoy = root.join("decoy");
    std::fs::create_dir(&decoy).unwrap();
    bind(
        &decoy,
        Some(stamp().with_workdir(Some(root.to_string_lossy().into_owned()))),
        &root,
    );
}

#[test]
fn scope_workdir_missing_stamp_never_uses_legacy_identity_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    for scope in [None, Some(stamp())] {
        assert_eq!(
            bind(&root, scope, Path::new("/")),
            br#"{"op":"route.bind"}"#
        );
    }
}
#[test]
fn scope_workdir_stamp_selects_project_config_instead_of_bind_config() {
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
        Some(&bind_folder),
        Some(stamp().with_workdir(Some(project.to_string_lossy().into_owned()))),
        &project,
        |identity, _| {
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
    bind_observed(
        Some(&bind_folder),
        Some(scope),
        &project,
        |identity, ctx| {
            ctx.set_db(Arc::new(StdMutex::new(crate::db::open(&db_path).unwrap())));
            let key = remote_policy::key(identity, Some("worker")).unwrap();
            let db = ctx.db().unwrap();
            let conn = db.lock().unwrap();
            let recovered = remote_exec::lookup(&conn, &key, 2)
                .unwrap()
                .expect("stamped route must find the legacy root-keyed catalog row");
            assert_eq!(recovered.host.as_deref(), Some("saved-host"));
            assert_eq!(recovered.siblings, ["saved-sibling"]);
        },
    );
}

#[test]
fn scope_workdir_bind_reply_is_byte_for_byte_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = canonical_root(dir.path());
    let other = root.join("other");
    std::fs::create_dir(&other).unwrap();
    for (scope, expected) in [
        (None, Path::new("/")),
        (Some(stamp()), Path::new("/")),
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
        let reply = bind(&root, scope, expected);
        assert_eq!(reply, br#"{"op":"route.bind"}"#);
    }
}

#[test]
fn scope_workdir_slash_bind_creates_no_project_actor_index_or_watcher() {
    let reply = bind(Path::new("/"), None, Path::new("/"));
    assert_eq!(reply, br#"{"op":"route.bind"}"#);
}

#[cfg(unix)]
#[test]
fn scope_workdir_alias_of_slash_is_not_a_project() {
    assert_eq!(
        resolve_route_project_root(Some(&stamp().with_workdir(Some("/.".into())))).unwrap(),
        None
    );
}

#[test]
fn scope_workdir_project_required_sentence_and_detail_are_pinned() {
    let (errors, lines) = crate::logging::capture_log_lines(|| {
        vec![
            project_folder_refusal(PROJECT_REQUIRED_CODE, None, route_key(7, 1), 20),
            project_folder_refusal(PROJECT_REQUIRED_CODE, Some("/"), route_key(7, 1), 21),
            project_folder_refusal("invalid_workdir", Some("."), route_key(7, 1), 22),
        ]
    });
    for error in &errors {
        assert_eq!(
            error.message.as_bytes(),
            b"This connection has no project folder, so AFT cannot run project tools on it."
        );
        assert_eq!(
            error.detail.as_ref().unwrap()["requirement"],
            "absolute_non_root_project_directory"
        );
    }
    assert_eq!(errors[0].code, "project_required");
    assert_eq!(
        errors[0].detail.as_ref().unwrap()["cause"],
        "project_folder_missing"
    );
    assert_eq!(errors[0].detail.as_ref().unwrap()["source"], "none");
    assert!(errors[0].detail.as_ref().unwrap()["folder"].is_null());
    assert_eq!(errors[1].code, "project_required");
    assert_eq!(
        errors[1].detail.as_ref().unwrap()["cause"],
        "filesystem_root"
    );
    assert_eq!(
        errors[1].detail.as_ref().unwrap()["source"],
        "scope_workdir"
    );
    assert_eq!(errors[1].detail.as_ref().unwrap()["folder"], "/");
    assert_eq!(errors[2].code, "invalid_workdir");
    assert_eq!(
        errors[2].detail.as_ref().unwrap()["cause"],
        "invalid_workdir"
    );
    assert_eq!(
        errors[2].detail.as_ref().unwrap()["source"],
        "scope_workdir"
    );
    assert_eq!(errors[2].detail.as_ref().unwrap()["folder"], ".");
    let warnings: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("subc project folder refused:"))
        .collect();
    assert_eq!(warnings.len(), 3, "{lines:?}");
    for (line, cause) in warnings.iter().zip([
        "project_folder_missing",
        "filesystem_root",
        "invalid_workdir",
    ]) {
        assert!(line.starts_with("WARN "), "{line}");
        assert!(line.contains("detail=") && line.contains(cause), "{line}");
    }
}
