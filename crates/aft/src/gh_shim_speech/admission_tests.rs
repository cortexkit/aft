use super::*;

async fn call_ticket(fixture: &Fixture) -> crate::gh_shim_ticket::ScopedTicket {
    let speech =
        crate::gh_shim_ticket::speech_for_ticket(fixture.params["ticket"].as_str().unwrap())
            .unwrap();
    let seed = speech.prepared.context.seed.clone();
    let root = fixture.service.store.path().parent().unwrap().to_path_buf();
    let session = fixture.session.clone();
    let project = seed.project_root.clone();
    // Ticket capture obtains authenticated bindings on a plain executor thread,
    // not while a Tokio runtime is entered on the calling thread.
    tokio::task::spawn_blocking(move || {
        std::thread::spawn(move || {
            crate::gh_shim_speech::service::with_launch(Some(seed), &root, || {
                crate::gh_shim_ticket::ScopedTicket::issue(&session, "foreground", &project)
            })
        })
        .join()
        .unwrap()
    })
    .await
    .unwrap()
}

async fn post_params(fixture: &Fixture, params: &Value, routes: &RouteScopes) -> RelayReply {
    crate::gh_shim_relay::relay(
        &fixture.plex,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params,
            first_party: true,
            route_scopes: routes,
        },
        &|_| {},
    )
    .await
}

#[tokio::test]
async fn foreground_calls_and_unpromoted_tasks_use_captured_guards() {
    let _lock = crate::test_env::process_env_lock();
    for codemode in [false, true] {
        let fixture = Fixture::new_with_options(
            codemode,
            LaunchOptions {
                foreground: true,
                ..Default::default()
            },
        )
        .await;
        assert!(
            fixture.startup_error.is_none(),
            "{:?}",
            fixture.startup_error
        );
        let guarded = Arc::new(Mutex::new(Vec::new()));
        let seen = guarded.clone();
        fixture.partners(|partners| {
            let reserve = partners.reserve.clone();
            partners.reserve = Arc::new(move |project, session, scope, body| {
                seen.lock().unwrap().push(body["arguments"].clone());
                reserve(project, session, scope, body)
            });
        });
        let warm =
            crate::gh_shim_ticket::speech_for_ticket(fixture.params["ticket"].as_str().unwrap())
                .unwrap();
        assert!(!warm.background.load(std::sync::atomic::Ordering::SeqCst));
        assert!(fixture
            .service
            .store
            .get(&fixture.incarnation)
            .unwrap()
            .ticket_digest
            .is_none());
        let answer = fixture.post(true, "unpromoted-guarded").await;
        assert!(answer.ok, "{:?}", answer.data);
        let ticket = call_ticket(&fixture).await;
        let mut params = fixture.params.clone();
        params["ticket"] = json!(ticket.value().unwrap());
        params["request_nonce"] = json!("call-guarded");
        let answer = post_params(&fixture, &params, &fixture.routes(true)).await;
        assert!(answer.ok, "{:?}", answer.data);
        let state = fixture.plex.state.lock().unwrap();
        assert_eq!(state.posts.len(), 2);
        let guarded = guarded.lock().unwrap();
        assert_eq!(guarded.len(), 2);
        assert!(guarded
            .iter()
            .all(|body| body["expect"]["app_handle_id"] == "app-original"));
        if codemode {
            assert!(guarded.iter().all(|body| !body["run_scope"].is_null()));
        }
    }
}

#[tokio::test]
async fn foreground_call_rejects_each_rebound_launch_identity_field() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let ticket = call_ticket(&fixture).await;
    let mut params = fixture.params.clone();
    params["ticket"] = json!(ticket.value().unwrap());
    for field in ["owner", "ref", "epoch", "agent"] {
        let mut stamp = fixture.head.clone();
        match field {
            "owner" => {
                stamp.owner = subc_protocol::Principal::Reserved {
                    module_id: "other-owner".into(),
                }
            }
            "ref" => stamp.scope_ref.push_str("-replacement"),
            "epoch" => stamp.scope_epoch += 1,
            "agent" => stamp.attributes.agent_id = Some("replacement-agent".into()),
            _ => unreachable!(),
        }
        params["request"]["metadata"]["agent_id"] = json!(stamp.attributes.agent_id);
        let mut routes = RouteScopes::default();
        routes.speech = Some(fixture.service.clone());
        routes.push_origin(
            fixture.launch.origin.clone().unwrap(),
            Some(fixture.head.clone()),
        );
        routes.push(&fixture.session, Some(stamp));
        let answer = post_params(&fixture, &params, &routes).await;
        assert_eq!(
            answer.data["refusal_code"], "task_gh_identity_conflict",
            "{field}: {:?}",
            answer.data
        );
        assert_eq!(answer.data["reason"], "request_mismatch");
    }
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn current_policy_withdrawal_is_enforced_by_the_production_relay() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let mut current = super::super::tests::fixture_manifest();
    current.manifest_version += 1;
    for rows in current.tiers.values_mut() {
        rows.retain(|row| row.tuple() != "issue comment");
    }
    current.canonicalization.remove("issue comment");
    let paths = StatePaths::from_root(fixture._dir.path().join("shim"));
    super::super::tests::write_signed_manifest(&paths, current, unix_seconds());
    load_manifest(&paths, unix_seconds()).unwrap();
    for bound in [false, true] {
        let answer = fixture.post(bound, "current-withdrawn").await;
        assert_eq!(
            answer.data["refusal_code"], "task_gh_policy_denied",
            "{:?}",
            answer.data
        );
        assert_eq!(answer.data["reason"], "classification_changed");
    }
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn production_relay_rejects_well_formed_frozen_mutation_mismatches() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    for (field, replacement) in [
        ("body", "different body"),
        ("action", "issue edit"),
        ("target", "99"),
    ] {
        let mut params = fixture.params.clone();
        params["request"][field] = json!(replacement);
        params["request_nonce"] = json!(format!("frozen-{field}"));
        let answer = post_params(&fixture, &params, &fixture.routes(true)).await;
        assert_eq!(
            answer.data["refusal_code"], "task_gh_policy_denied",
            "{field}: {:?}",
            answer.data
        );
        assert_eq!(answer.data["reason"], "canonicalization_changed");
    }
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn production_relay_rejects_malformed_and_oversized_invocations_before_send() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    for (invocation, reason) in [
        (
            json!({"args":[],"body_inputs":{},"unexpected":true}),
            "snapshot_invalid",
        ),
        (json!({"args":7,"body_inputs":{}}), "snapshot_invalid"),
        (
            json!({"args":["x".repeat(4 * 1024 * 1024 + 1)],"body_inputs":{}}),
            "payload_denied",
        ),
    ] {
        let mut params = fixture.params.clone();
        params["invocation"] = invocation;
        let answer = post_params(&fixture, &params, &fixture.routes(true)).await;
        assert_eq!(answer.data["reason"], reason, "{:?}", answer.data);
        assert!(!answer.ok);
    }
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn foreground_call_cannot_acquire_a_current_only_repository() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let ticket = call_ticket(&fixture).await;
    let mut current = super::super::tests::fixture_manifest();
    let agent = current.bindings.values().next().unwrap().clone();
    current.bindings.insert("new-owner/new-repo".into(), agent);
    current.manifest_version += 1;
    let paths = StatePaths::from_root(fixture._dir.path().join("shim"));
    super::super::tests::write_signed_manifest(&paths, current, unix_seconds());
    load_manifest(&paths, unix_seconds()).unwrap();
    let mut params = fixture.params.clone();
    params["ticket"] = json!(ticket.value().unwrap());
    params["request"]["repository"] = json!("new-owner/new-repo");
    let args = params["invocation"]["args"].as_array_mut().unwrap();
    let repo = args.iter().position(|arg| arg == "--repo").unwrap() + 1;
    args[repo] = json!("new-owner/new-repo");
    let answer = post_params(&fixture, &params, &fixture.routes(true)).await;
    assert_eq!(
        answer.data["refusal_code"], "task_gh_identity_conflict",
        "{:?}",
        answer.data
    );
    assert_eq!(answer.data["reason"], "binding_changed");
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn production_relay_refuses_different_argv_that_becomes_mechanical() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let mut params = fixture.params.clone();
    params["invocation"]["args"] = json!(["issue", "list"]);
    let answer = post_params(&fixture, &params, &fixture.routes(true)).await;
    assert_eq!(answer.data["refusal_code"], "task_gh_policy_denied");
    assert_eq!(answer.data["reason"], "classification_changed");
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn production_relay_requires_the_captured_binding_after_policy_mapping() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let mut record = fixture
        .service
        .store
        .get(&fixture.incarnation)
        .unwrap()
        .record;
    let repo = fixture.params["request"]["repository"].as_str().unwrap();
    assert!(record.bindings.remove(repo).is_some());
    let encoded = serde_json::to_string(&record).unwrap();
    fixture
        .service
        .store
        .connect()
        .unwrap()
        .execute(
            "UPDATE gh_speech_authorities SET record_json=?1,record_digest=?2 WHERE incarnation=?3",
            rusqlite::params![
                encoded,
                blake3::hash(encoded.as_bytes()).as_bytes(),
                fixture.incarnation
            ],
        )
        .unwrap();
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = reads.clone();
    fixture.partners(|partners| {
        let binding_read = partners.bindings.clone();
        partners.bindings = Arc::new(move |project, session| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            binding_read(project, session)
        });
    });
    let answer = fixture.post(true, "missing-captured-binding").await;
    assert_eq!(
        answer.data["refusal_code"], "task_gh_policy_denied",
        "{:?}",
        answer.data
    );
    assert_eq!(answer.data["reason"], "repository_denied");
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn existing_storage_and_project_aliases_keep_capture_and_path_spellings() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let supplied_project = fixture._dir.path().join("project");
    let original =
        crate::gh_shim_ticket::redeem(fixture.params["ticket"].as_str().unwrap()).unwrap();
    assert_eq!(
        original.project_root,
        supplied_project.display().to_string()
    );
    assert_eq!(
        fixture
            .service
            .store
            .get(&fixture.incarnation)
            .unwrap()
            .record
            .project_root,
        supplied_project.display().to_string()
    );
    let canonical_project = fs::canonicalize(&supplied_project).unwrap();
    let project_alias = fixture._dir.path().join("project-alias");
    let storage_alias = fixture._dir.path().join("storage-alias");
    std::os::unix::fs::symlink(&canonical_project, &project_alias).unwrap();
    std::os::unix::fs::symlink(
        fixture.service.store.path().parent().unwrap(),
        &storage_alias,
    )
    .unwrap();
    assert_ne!(project_alias, canonical_project);
    assert_eq!(fs::canonicalize(&project_alias).unwrap(), canonical_project);
    let speech =
        crate::gh_shim_ticket::speech_for_ticket(fixture.params["ticket"].as_str().unwrap())
            .unwrap();
    let template = speech.prepared.context.seed.clone();
    for (seed_project, ticket_project) in [
        (
            canonical_project.display().to_string(),
            project_alias.display().to_string(),
        ),
        (
            project_alias.display().to_string(),
            canonical_project.display().to_string(),
        ),
    ] {
        let mut seed = template.clone();
        seed.project_root = seed_project.clone();
        let root = storage_alias.clone();
        let session = fixture.session.clone();
        let supplied = ticket_project.clone();
        let (pending, call) = tokio::task::spawn_blocking(move || {
            std::thread::spawn(move || {
                let lifecycle = seed.lifecycle.clone();
                crate::gh_shim_speech::service::with_launch(Some(seed), &root, || {
                    crate::bash_background::registry::with_scope_spawn(lifecycle, || {
                        (
                            crate::gh_shim_ticket::PendingTicket::issue(&session, &supplied),
                            crate::gh_shim_ticket::ScopedTicket::issue(
                                &session,
                                "alias-call",
                                &supplied,
                            ),
                        )
                    })
                })
            })
            .join()
            .unwrap()
        })
        .await
        .unwrap();
        for raw in [pending.value().unwrap(), call.value().unwrap()] {
            let attached = crate::gh_shim_ticket::speech_for_ticket(raw)
                .expect("equivalent existing path aliases must retain launch context");
            assert!(attached.failure.lock().unwrap().is_none());
            assert!(attached.prepared.policy.is_some());
            assert_eq!(attached.prepared.context.seed.project_root, seed_project);
            assert_eq!(
                crate::gh_shim_ticket::redeem(raw).unwrap().project_root,
                ticket_project
            );
        }
        let attached = crate::gh_shim_ticket::speech_for_ticket(call.value().unwrap()).unwrap();
        assert_eq!(
            fixture
                .service
                .store
                .get(&attached.incarnation)
                .unwrap()
                .record
                .project_root,
            seed_project
        );
    }
}
