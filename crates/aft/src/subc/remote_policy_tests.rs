use super::*;
use crate::run_tool_call::{run_tool_call, ToolCallContext, ToolCallOutcome};
use sha2::{Digest, Sha256};

#[cfg(unix)]
#[path = "confine_tests.rs"]
mod confine_tests;

#[test]
fn confined_catalog_offers_only_root_readers() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "designer", 7, true);
    let answer = catalog(
        json!({"preset":"worker","params":{"confine":"root"}}),
        &route,
        &ctx,
    )
    .unwrap();
    let names: std::collections::BTreeSet<_> = answer["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["read", "grep", "glob", "aft_outline"]
            .into_iter()
            .collect()
    );
}

/// Run one read-only tool as the route's worker would, under its saved
/// confinement, and return the native response.
fn call(ctx: &AppContext, route: &RouteIdentity, name: &str, args: Value) -> Response {
    let _scope = crate::access_policy::scope_guard(Some(access_grants::policy_scope_key(route)));
    let _ask =
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Consent {
            once: vec![],
        }));
    let format =
        crate::subc_format::FormatContext::from_tool_call(name, &args, &route.project_root);
    let tool_ctx = ToolCallContext {
        project_root: route.project_root.clone(),
        session_id: Some(route.session.clone()),
        request_id: "confine-vector".into(),
        diagnostics_on_edit: false,
        preview: false,
        edit_slot_survives: None,
        report_registration_downgrade: false,
        standard_edit_grammar: true,
        disabled_tools: Some(route.disabled_tools.clone()),
        worker_session: true,
        host_tool_names: None,
    };
    with_call_policy(ctx, route, &tool_ctx.request_id, name, args, |args| {
        let ToolCallOutcome::Unary(result) = run_tool_call(
            name,
            args,
            &format,
            &tool_ctx,
            ctx,
            &|request, ctx| match request.command.as_str() {
                "read" => crate::commands::read::handle_read(&request, ctx),
                "grep" => crate::commands::grep::handle_grep(&request, ctx),
                "glob" => crate::commands::glob::handle_glob(&request, ctx),
                "outline" => crate::commands::outline::handle_outline(&request, ctx),
                _ => Response::success(&request.id, json!({"unexpected_dispatch": true})),
            },
            None,
            None,
        );
        result.response
    })
    .unwrap_or_else(|response| response)
}

/// A confined glob without `path` walks from the canonicalized project root.
/// On Windows that spelling keeps the `\\?\` prefix the read gate's root
/// lacks; the tree's own ignore files must apply all the same, at the root
/// and in a subdirectory.
#[test]
fn confined_glob_without_path_honors_the_trees_gitignore() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".gitignore"), "hidden.rs\n").unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("nested/.gitignore"), "deep_hidden.rs\n").unwrap();
    for file in [
        "visible.rs",
        "hidden.rs",
        "nested/deep_visible.rs",
        "nested/deep_hidden.rs",
    ] {
        std::fs::write(root.path().join(file), "fn token() {}\n").unwrap();
    }
    let ctx = context(root.path(), storage.path(), None);
    ctx.update_config(|config| config.restrict_to_project_root = false);
    let route = identity(root.path(), "designer", 7, true);
    catalog(
        json!({"preset":"worker", "params":{"confine":"root"}}),
        &route,
        &ctx,
    )
    .unwrap();
    let response = call(&ctx, &route, "glob", json!({"pattern": "**/*.rs"}));
    assert!(response.success, "{response:?}");
    let native = serde_json::to_string(&response).unwrap();
    for expected in ["visible.rs", "deep_visible.rs"] {
        assert!(native.contains(expected), "{expected} missing: {native}");
    }
    // `hidden.rs` also matches inside `deep_hidden.rs`.
    assert!(
        !native.contains("hidden.rs"),
        "an ignored file was listed: {native}"
    );
}

/// An unconfined answer is remembered, but saving `confine: "root"` for the
/// worker, whether a catalog fetch asks for it or it is carried forward from
/// an older epoch's saved policy, takes effect on the worker's very next call.
#[test]
fn remembered_unconfined_answer_cannot_outlive_a_confining_save() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let older = identity(root.path(), "designer", 7, true);
    let newer = identity(root.path(), "designer", 8, true);
    let unconfined_plan = || json!({"preset":"worker", "params":{"remote_exec":{"enabled":true}}});
    // The newer epoch saves an unconfined plan; both epochs read as unconfined
    // and are remembered so.
    catalog(unconfined_plan(), &newer, &ctx).unwrap();
    for route in [&older, &newer] {
        assert_eq!(confined(&ctx, route), Ok(false));
        assert_eq!(confinement_from_memory(&ctx, route), Some(Ok(false)));
    }

    // A confining fetch on the older epoch: its next call is confined,
    // answered from memory without reading aft.db.
    catalog(
        json!({"preset":"worker", "params":{"confine":"root"}}),
        &older,
        &ctx,
    )
    .unwrap();
    let reads = ctx.policy_confinement_reads_for_test();
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &older, "bash", None)
            .unwrap_err()
            .code,
        "tool_not_offered_confined"
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), reads);

    // The newer epoch's own row is still unconfined, so it reads (and is
    // remembered) as unconfined until its next fetch carries the older
    // epoch's fence forward onto that row.
    assert_eq!(confined(&ctx, &newer), Ok(false));
    assert_eq!(confinement_from_memory(&ctx, &newer), Some(Ok(false)));
    catalog(unconfined_plan(), &newer, &ctx).unwrap();
    let reads = ctx.policy_confinement_reads_for_test();
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &newer, "bash", None)
            .unwrap_err()
            .code,
        "tool_not_offered_confined"
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), reads);
}

/// A worker's later epoch with no saved row of its own reads the older
/// epoch's row. If a save of `confine: "root"` for the older epoch lands
/// between reading the later epoch as unconfined and remembering that answer,
/// the answer must not be remembered: the later epoch is confined from then
/// on, and its next call must read aft.db again and be confined.
#[test]
fn unconfined_read_racing_a_confining_save_is_not_remembered() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let older = identity(root.path(), "designer", 7, true);
    let newer = identity(root.path(), "designer", 8, true);
    let saving = older.clone();
    ctx.after_next_policy_read_for_test(move |ctx| {
        catalog(
            json!({"preset":"worker", "params":{"confine":"root"}}),
            &saving,
            ctx,
        )
        .unwrap();
    });
    // No row is saved yet, so the newer epoch reads as unconfined; the older
    // epoch's confining save then commits before that answer is remembered.
    assert_eq!(confined(&ctx, &newer), Ok(false));
    assert_eq!(confinement_from_memory(&ctx, &older), Some(Ok(true)));
    assert_eq!(
        confinement_from_memory(&ctx, &newer),
        None,
        "an unconfined answer read before a confining save was remembered"
    );
    let reads = ctx.policy_confinement_reads_for_test();
    assert_eq!(
        admit_confined(&ctx, &newer, "bash").unwrap_err().code,
        "tool_not_offered_confined"
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), reads + 1);
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &newer, "bash", None)
            .unwrap_err()
            .code,
        "tool_not_offered_confined"
    );
}

fn identity(root: &Path, session: &str, epoch: u64, scoped: bool) -> RouteIdentity {
    let scope=serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"worker","scope_epoch":epoch,"kind":"worker","owner_authorized":true})).unwrap();
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: session.into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: session.into(),
            principal_id: Some("reserved:broca".into()),
        },
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        run_scope: OnceLock::new(),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        scope: scoped.then_some(scope),
        made_tool_call: AtomicBool::new(false),
    }))
}

fn context(root: &Path, storage: &Path, connection: Option<PathBuf>) -> AppContext {
    context_with_user_remote(root, storage, connection, false)
}

fn context_with_user_remote(
    root: &Path,
    storage: &Path,
    connection: Option<PathBuf>,
    enabled: bool,
) -> AppContext {
    context_with_switch(root, storage, connection, enabled, true)
}

fn context_with_switch(
    root: &Path,
    storage: &Path,
    connection: Option<PathBuf>,
    enabled: bool,
    runon_enabled: bool,
) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.into());
    config.storage_dir = Some(storage.into());
    config.experimental_bash_background = true;
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    config.remote_exec.enabled = enabled;
    config.bash.runon_enabled = runon_enabled;
    config.semantic.subc_connection_file = connection;
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&storage.join("aft.db")).unwrap(),
    )));
    ctx
}

/// A context with no aft.db handle and persistence never configured. Tests
/// move its database runtime state to opening or failed when they need to.
fn context_without_database(root: &Path, storage: &Path) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.into());
    config.storage_dir = Some(storage.into());
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

#[test]
fn confine_lookup_without_configured_persistence_is_unconfined() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_without_database(root.path(), storage.path());
    let route = identity(root.path(), "ordinary-worker", 7, true);
    assert!(key(&route, Some("worker")).is_some());
    assert!(!confined(&ctx, &route).unwrap());
    assert!(!admit_confined(&ctx, &route, "bash").unwrap());
    assert!(!admit_confined(&ctx, &route, "read").unwrap());
}

#[test]
fn confine_lookup_refuses_while_database_opens_or_after_it_failed() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_without_database(root.path(), storage.path());
    let route = identity(root.path(), "designer", 7, true);
    ctx.mark_database_runtime_initializing_for_test();
    assert!(awaits_policy_store(&ctx, &route));
    assert!(!awaits_policy_store(
        &ctx,
        &identity(root.path(), "unscoped", 7, false)
    ));
    assert_eq!(
        confined(&ctx, &route).unwrap_err().code,
        "confine_policy_unavailable"
    );
    ctx.finish_database_runtime_error("database cannot open".into(), false);
    assert!(!awaits_policy_store(&ctx, &route));
    for name in ["read", "bash"] {
        assert_eq!(
            admit_confined(&ctx, &route, name).unwrap_err().code,
            "confine_policy_unavailable",
            "{name}"
        );
    }
}

#[test]
fn confine_lookup_takes_no_write_lock_and_copies_no_row() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    catalog(
        json!({"preset":"worker","params":{"confine":"root"}}),
        &identity(root.path(), "designer", 7, true),
        &ctx,
    )
    .unwrap();
    // A second process: nothing cached, its own connection, and no waiting
    // for a lock, so a write transaction here fails at once.
    let restarted = context(root.path(), storage.path(), None);
    restarted
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let writer = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let rebound = identity(root.path(), "designer", 8, true);
    assert!(confined(&restarted, &rebound).unwrap());
    assert_eq!(
        admit_confined(&restarted, &rebound, "bash")
            .unwrap_err()
            .code,
        "tool_not_offered_confined"
    );
    writer.execute_batch("ROLLBACK").unwrap();
    let copied: i64 = writer
        .query_row(
            "SELECT COUNT(*) FROM remote_exec_policies WHERE epoch='8'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(copied, 0, "a per-call lookup carried a row forward");
}

#[test]
fn confine_answer_is_kept_once_confined() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "designer", 7, true);
    let ordinary = identity(root.path(), "ordinary-worker", 7, true);
    catalog(
        json!({"preset":"worker","params":{"confine":"root"}}),
        &route,
        &ctx,
    )
    .unwrap();
    assert!(confined(&ctx, &route).unwrap());
    assert!(!confined(&ctx, &ordinary).unwrap());
    // With the store gone, only the remembered answer can keep the fence.
    ctx.clear_db();
    assert!(confined(&ctx, &route).unwrap());
    ctx.mark_database_runtime_initializing_for_test();
    assert!(!awaits_policy_store(&ctx, &route));
    assert!(awaits_policy_store(&ctx, &ordinary));
}

fn plan(name: &str) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/exec_remote/fixtures/plans");
    let jcs = std::fs::read(root.join(format!("{name}.jcs"))).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&jcs)),
        std::fs::read_to_string(root.join(format!("{name}.sha256"))).unwrap()
    );
    let value: Value = serde_json::from_slice(&jcs).unwrap();
    assert_eq!(
        value,
        serde_json::from_slice::<Value>(&std::fs::read(root.join(format!("{name}.json"))).unwrap())
            .unwrap()
    );
    value
}

fn fetch(plan: &Value) -> Value {
    json!({"op":"tool.catalog","preset":"worker","params":plan["tool_items"][0]["params"],"composition":plan["composition"]})
}

#[test]
fn worker_reply_deadlines_track_resolved_config_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "reply-deadline", 7, false);
    let mut previous = None;
    for (cap, expected) in [(60_000, 90_000), (300_000, 330_000)] {
        ctx.update_config(|config| config.bash.worker_wait_max_ms = cap);
        for preset in ["worker", "reader"] {
            let answer = catalog(json!({"preset":preset}), &route, &ctx).unwrap();
            for tool in answer["tools"].as_array().unwrap() {
                if preset == "worker"
                    && matches!(tool["name"].as_str(), Some("bash" | "bash_watch"))
                {
                    assert_eq!(tool["reply"], json!({"max_ms":expected}), "{tool}");
                } else {
                    assert!(tool.get("reply").is_none(), "{preset}: {tool}");
                }
            }
            if preset == "worker" {
                let digest = answer["catalog_digest"].clone();
                if let Some(previous) = previous.replace(digest.clone()) {
                    assert_ne!(previous, digest);
                }
                let digest_only =
                    catalog(json!({"preset":preset,"digest_only":true}), &route, &ctx).unwrap();
                assert_eq!(digest_only["catalog_digest"], digest);
            }
        }
    }
}

#[test]
fn head_reply_deadline_tracks_resolved_config_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "head-reply-deadline", 7, false);
    let mut previous = None;
    for (cap, expected) in [(60_000, 90_000), (300_000, 330_000)] {
        ctx.update_config(|config| config.bash.worker_wait_max_ms = cap);
        let answer = catalog(json!({"preset":"head"}), &route, &ctx).unwrap();
        for tool in answer["tools"].as_array().unwrap() {
            if tool["name"] == "bash" {
                assert_eq!(tool["reply"], json!({"max_ms":expected}), "{tool}");
            } else {
                assert!(tool.get("reply").is_none(), "{tool}");
            }
        }
        assert!(!answer["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "bash_watch"));
        let digest = answer["catalog_digest"].clone();
        if let Some(previous) = previous.replace(digest.clone()) {
            assert_ne!(previous, digest);
        }
        let digest_only =
            catalog(json!({"preset":"head","digest_only":true}), &route, &ctx).unwrap();
        assert_eq!(digest_only["catalog_digest"], digest);
    }
}

#[test]
fn oversized_head_cap_serves_catalog_without_reply_metadata() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "head-reply-oversized", 7, false);
    for cap in [86_400_000, u64::MAX] {
        ctx.update_config(|config| config.bash.worker_wait_max_ms = cap);
        let answer = catalog(json!({"preset":"head"}), &route, &ctx).unwrap();
        assert!(answer["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("reply").is_none()));
    }
}

#[test]
fn oversized_worker_cap_serves_catalog_without_reply_metadata() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    ctx.update_config(|config| config.bash.worker_wait_max_ms = 86_400_000);
    let route = identity(root.path(), "reply-oversized", 7, false);
    let answer = catalog(json!({"preset":"worker"}), &route, &ctx).unwrap();
    assert!(answer["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "bash_watch"));
    assert!(answer["tools"]
        .as_array()
        .unwrap()
        .iter()
        .all(|t| t.get("reply").is_none()));
}

#[cfg(unix)]
fn bash_catalog_entry(reply: &Value) -> &Value {
    reply["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bash")
        .unwrap()
}

#[cfg(unix)]
#[test]
fn sessions_without_remote_execution_omit_runon_and_guidance() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let worker = identity(root.path(), "disabled-worker", 7, true);
    for reply in [
        catalog(json!({"preset":"worker"}), &worker, &ctx).unwrap(),
        catalog(fetch(&plan("broca-worker-policy-disabled")), &worker, &ctx).unwrap(),
        catalog(
            json!({"preset":"head"}),
            &identity(root.path(), "head", 7, false),
            &ctx,
        )
        .unwrap(),
    ] {
        let bash = bash_catalog_entry(&reply);
        assert!(
            bash["input_schema"]["properties"].get("runon").is_none(),
            "{bash}"
        );
        assert!(
            !bash["description"].as_str().unwrap().contains("runon"),
            "{bash}"
        );
    }
}

#[cfg(unix)]
#[test]
fn safety_switch_hides_runon_without_disabling_the_persisted_old_prefix_plan() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_switch(root.path(), storage.path(), None, true, false);
    let worker = identity(root.path(), "old-worker", 7, true);
    let mut old = plan("broca-worker");
    old["tool_items"][0]["params"]["remote_exec"] =
        json!({"enabled":true,"commands":["cargo test"]});
    let reply = catalog(fetch(&old), &worker, &ctx).unwrap();
    let bash = bash_catalog_entry(&reply);
    assert!(bash["input_schema"]["properties"].get("runon").is_none());
    assert!(!bash["description"].as_str().unwrap().contains("runon"));
    let launch = lookup(&ctx, &source(&worker, Some("worker"))).unwrap();
    assert!(crate::exec_remote::policy::matches(
        launch.params.remote_exec.as_ref().unwrap(),
        "cargo test",
        false,
        false
    ));
}

#[cfg(unix)]
#[test]
fn enabled_worker_sessions_offer_runon_and_guidance_from_the_persisted_plan() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let worker = identity(root.path(), "enabled-worker", 7, true);
    let ctx = context(root.path(), storage.path(), None);
    let first = catalog(fetch(&plan("broca-worker")), &worker, &ctx).unwrap();
    drop(ctx);
    let restarted = context(root.path(), storage.path(), None);
    let refetch = catalog(json!({"preset":"worker"}), &worker, &restarted).unwrap();
    for reply in [first, refetch] {
        let bash = bash_catalog_entry(&reply);
        assert_eq!(
            bash["input_schema"]["properties"]["runon"]["type"],
            "string"
        );
        assert!(
            bash["description"]
                .as_str()
                .unwrap()
                .contains("When remote runs are available"),
            "{bash}"
        );
        assert!(bash["description"]
            .as_str()
            .unwrap()
            .contains("including chains and pipes"));
    }
}

#[cfg(unix)]
#[test]
fn exec_remote_catalog_params_survive_long_idle_rebind() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let worker = identity(root.path(), "long-lived-worker", 7, true);
    let ctx = context(root.path(), storage.path(), None);
    catalog(fetch(&plan("broca-worker")), &worker, &ctx).unwrap();
    ctx.db()
        .unwrap()
        .lock()
        .unwrap()
        .execute("UPDATE remote_exec_policies SET last_used=1", [])
        .unwrap();
    drop(ctx);

    let restarted = context(root.path(), storage.path(), None);
    let rebound = identity(root.path(), "long-lived-worker", 7, true);
    let launch = lookup(&restarted, &source(&rebound, Some("worker")))
        .expect("a live scope's catalog params must not expire while its worker is idle");
    let config = restarted.config();
    let routed = crate::bash_background::with_remote_policy(Some(launch), || {
        crate::bash_background::remote_for_runon(&config, "linux,4c", false, false, false)
    })
    .expect("the rebound worker must still have a remote runner");
    assert_eq!(routed.requested_vcpus, Some(4));
}

#[cfg(unix)]
#[test]
fn exec_remote_catalog_params_survive_root_reclaim_and_rebind() {
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().to_owned();
    let storage = tempfile::tempdir().unwrap();
    let worker = identity(&root_path, "rebound-worker", 7, true);
    let root_id = worker.root.clone();
    let ctx = Arc::new(context(&root_path, storage.path(), None));
    catalog(fetch(&plan("broca-worker")), &worker, &ctx).unwrap();
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root_id.clone(), Arc::clone(&ctx)));
    let mut live_roots = HashMap::from([(root_id.clone(), RootMeta::new(Instant::now()))]);
    let metrics = DispatchPathMetrics::new();
    root.close().unwrap();

    // Supply two observations of the missing directory so actor retirement
    // does not depend on background filesystem probes. Removing and recreating
    // the worktree does not end the worker scope.
    for generation in 1..=2 {
        let outcome = reap_idle_roots_with_presence(
            Instant::now(),
            &mut live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &executor,
            &metrics,
            &RootPresence::probe(generation, [&root_id]),
        );
        assert_eq!(
            outcome.forgotten_deleted_roots.len(),
            (generation - 1) as usize
        );
    }
    assert!(!executor.actor_registered(&root_id));
    assert!(live_roots.is_empty());
    drop(executor);
    drop(ctx);

    std::fs::create_dir(&root_path).unwrap();
    let restarted = context(&root_path, storage.path(), None);
    let rebound = identity(&root_path, "rebound-worker", 7, true);
    let launch = lookup(&restarted, &source(&rebound, Some("worker")))
        .expect("root reclamation must not erase a live scope's catalog params");
    let config = restarted.config();
    let routed = crate::bash_background::with_remote_policy(Some(launch), || {
        crate::bash_background::remote_for_runon(&config, "linux,4c", false, false, false)
    })
    .expect("the rebound worker must still have a remote runner without another catalog fetch");
    assert_eq!(routed.requested_vcpus, Some(4));
    let db = restarted.db().unwrap();
    let conn = db.lock().unwrap();
    let key = key(&rebound, Some("worker")).unwrap();
    assert_eq!(
        store::forget_scope(&conn, &key.owner, &key.scope_ref, key.epoch).unwrap(),
        1
    );
    assert!(store::lookup(&conn, &key, 1).unwrap().is_none());
    std::fs::remove_dir(&root_path).unwrap();
}

#[cfg(unix)]
#[test]
fn enabled_head_sessions_offer_runon_and_guidance_from_user_config() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_user_remote(
        root.path(),
        storage.path(),
        Some(root.path().join("connection.json")),
        true,
    );
    let head = identity(root.path(), "configured-head", 7, false);
    let reply = catalog(json!({"preset":"head"}), &head, &ctx).unwrap();
    let bash = bash_catalog_entry(&reply);
    assert_eq!(
        bash["input_schema"]["properties"]["runon"]["type"],
        "string"
    );
    assert!(bash["description"]
        .as_str()
        .unwrap()
        .contains("When remote runs are available"));
    // The user setting does not grant a worker a remote runner without its own plan.
    let worker = catalog(
        json!({"preset":"worker"}),
        &identity(root.path(), "unplanned-worker", 7, true),
        &ctx,
    )
    .unwrap();
    assert!(bash_catalog_entry(&worker)["input_schema"]["properties"]
        .get("runon")
        .is_none());
}

#[cfg(unix)]
#[test]
fn stamped_host_heads_keep_role_tools_and_user_remote_runner() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context_with_user_remote(root.path(), storage.path(), None, true);
    for harness in ["opencode", "pi"] {
        let mut plain = identity(root.path(), "host-head", 7, false);
        let data = Arc::get_mut(&mut plain.0).unwrap();
        data.harness = harness.into();
        data.role = tool_provider::RouteRole::Legacy;
        data.spawn_principal = AuthenticatedPrincipal::RouteBind {
            trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.path().into(),
            harness: harness.into(),
            session_id: "host-head".into(),
            principal_id: Some("direct".into()),
        };
        let mut stamped = identity(root.path(), "host-head", 7, false);
        let data = Arc::get_mut(&mut stamped.0).unwrap();
        data.harness = harness.into();
        data.role = plain.role;
        data.spawn_principal = plain.spawn_principal.clone();
        data.scope = Some(
            serde_json::from_value(json!({
                "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
                "ref": "host-head", "scope_epoch": 7, "kind": "head",
                "owner_authorized": true,
                "attributes": {"agent_id": "head-agent", "delegates": true}
            }))
            .unwrap(),
        );

        assert_eq!(
            tool_provider::resolve_caller_role(Some("head"), stamped.scope.is_some(), false)
                .unwrap(),
            tool_provider::CallerRole::Head
        );
        assert!(key(&stamped, Some("head")).is_none());
        let before = catalog(json!({"preset":"head"}), &plain, &ctx).unwrap();
        let after = catalog(json!({"preset":"head"}), &stamped, &ctx).unwrap();
        assert_eq!(
            after, before,
            "{harness}: stamping must not change the tool catalog"
        );
        assert_eq!(
            bash_catalog_entry(&after)["input_schema"]["properties"]["runon"]["type"],
            "string"
        );
        assert_eq!(stamped.trust, plain.trust);
        assert_eq!(
            stamped.consumer_elicitation_capable,
            plain.consumer_elicitation_capable
        );
        let shell = serde_json::from_value(json!({
            "name": "bash", "arguments": {"command": "true"},
            "preset": "head", "call_key": "head-shell"
        }))
        .unwrap();
        assert_eq!(
            tool_provider::admission_trusted(&stamped, &shell),
            tool_provider::admission_trusted(&plain, &shell)
        );

        let launch = lookup(&ctx, &source(&stamped, Some("head")))
            .expect("a stamped host head keeps the user-configured remote runner without worker catalog params");
        assert_eq!(launch.harness, harness);
        assert_eq!(launch.session, "host-head");
        let config = ctx.config();
        let routed = crate::bash_background::with_remote_policy(Some(launch), || {
            crate::bash_background::remote_for_runon(&config, "linux,4c", false, false, false)
        })
        .expect("a stamped head must not report that this session has no remote runner");
        assert_eq!(routed.requested_vcpus, Some(4));
    }
    ctx.update_config(|config| config.remote_exec.enabled = false);
    assert!(lookup(
        &ctx,
        &RemoteSource::Head {
            harness: "opencode".into(),
            session: "host-head".into()
        }
    )
    .is_none());
}

// Remote routing runs only on Unix; the plan fixtures carry Unix sibling paths,
// which a Windows decode rightly rejects as not absolute.
#[cfg(unix)]
#[test]
fn exec_remote_catalog_freezes_verbatim_plan_and_carries_rebound_epoch_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let first = identity(root.path(), "one", 7, true);
    let second = identity(root.path(), "two", 7, true);
    let enabled = plan("broca-worker");
    let disabled = plan("broca-worker-policy-disabled");
    catalog(fetch(&enabled), &first, &ctx).unwrap();
    catalog(fetch(&disabled), &second, &ctx).unwrap();
    assert!(
        lookup(&ctx, &RemoteSource::Worker(key(&first, Some("worker"))))
            .unwrap()
            .params
            .remote_exec
            .unwrap()
            .enabled
    );
    assert!(
        !lookup(&ctx, &RemoteSource::Worker(key(&second, Some("worker"))))
            .unwrap()
            .params
            .remote_exec
            .unwrap()
            .enabled
    );
    // A rebound epoch retains the same worker's catalog inputs. Another
    // principal or an unscoped call cannot borrow them.
    let new_epoch = identity(root.path(), "one", 8, true);
    let mut other_principal = identity(root.path(), "one", 7, true);
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut Arc::get_mut(&mut other_principal.0)
            .unwrap()
            .spawn_principal
    {
        *principal_id = Some("reserved:another-carrier".into());
    }
    assert!(lookup(
        &ctx,
        &RemoteSource::Worker(key(&other_principal, Some("worker")))
    )
    .is_none());
    let revived = lookup(&ctx, &RemoteSource::Worker(key(&new_epoch, Some("worker")))).unwrap();
    assert_eq!(
        serde_json::to_value(revived.params).unwrap(),
        serde_json::to_value(
            lookup(&ctx, &RemoteSource::Worker(key(&first, Some("worker"))))
                .unwrap()
                .params
        )
        .unwrap()
    );
    let old_epoch = identity(root.path(), "one", 6, true);
    assert!(lookup(&ctx, &RemoteSource::Worker(key(&old_epoch, Some("worker")))).is_none());
    let unscoped = identity(root.path(), "one", 7, false);
    assert!(lookup(&ctx, &RemoteSource::Worker(key(&unscoped, Some("worker")))).is_none());
    assert!(lookup(&ctx, &RemoteSource::Worker(key(&first, Some("head")))).is_none());
    assert!(lookup(
        &ctx,
        &RemoteSource::Worker(key(
            &identity(root.path(), "not-fetched", 7, true),
            Some("worker")
        ))
    )
    .is_none());
    // Freeze is immutable within one bind/scope identity.
    catalog(fetch(&disabled), &first, &ctx).unwrap();
    assert!(
        lookup(&ctx, &RemoteSource::Worker(key(&first, Some("worker"))))
            .unwrap()
            .params
            .remote_exec
            .unwrap()
            .enabled
    );
}

#[test]
fn exec_remote_catalog_validates_vocabulary_and_disables_malformed_routing_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let bind = identity(root.path(), "one", 7, true);
    for (name, values) in [
        ("behavior", vec!["autonomous", "interactive"]),
        ("tool_descs", vec!["concise", "full"]),
        ("scope", vec!["all", "readwrite"]),
        ("host", vec!["broca", "other-host"]),
    ] {
        for value in values {
            catalog(
                json!({"preset":"worker","params":{name:value}}),
                &bind,
                &ctx,
            )
            .unwrap();
        }
        let error =
            catalog(json!({"preset":"worker","params":{name:42}}), &bind, &ctx).unwrap_err();
        assert!(serde_json::to_string(&error).unwrap().contains(name));
    }
    for (preset, value, accepted) in [
        ("reader", "read", true),
        ("head", "all", true),
        ("head", "readwrite", true),
        ("worker", "read", false),
        ("reader", "all", false),
    ] {
        assert_eq!(
            catalog(
                json!({"preset":preset,"params":{"scope":value}}),
                &bind,
                &ctx
            )
            .is_ok(),
            accepted
        );
    }
    let error = catalog(
        json!({"preset":"worker","params":{"not_a_key":true}}),
        &bind,
        &ctx,
    )
    .unwrap_err();
    assert!(serde_json::to_string(&error).unwrap().contains("not_a_key"));
    for (n, malformed) in [
        json!({"remote_exec":{"enabled":"yes"}}),
        json!({"remote_exec":{"enabled":true,"unexpected":1}}),
        json!({"siblings":[4]}),
    ]
    .into_iter()
    .enumerate()
    {
        let bind = identity(root.path(), &format!("malformed-{n}"), 7, true);
        catalog(json!({"preset":"worker","params":malformed}), &bind, &ctx).unwrap();
        assert!(
            lookup(&ctx, &RemoteSource::Worker(key(&bind, Some("worker"))))
                .unwrap()
                .params
                .remote_exec
                .is_none()
        );
    }
}

#[test]
fn exec_remote_catalog_paramless_and_unscoped_parity() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let bind = identity(root.path(), "one", 7, false);
    for preset in ["head", "worker", "reader"] {
        let body = json!({"preset":preset});
        assert_eq!(
            serde_json::to_vec(&catalog(body.clone(), &bind, &ctx).unwrap()).unwrap(),
            serde_json::to_vec(
                &tool_provider::catalog(body, &[], crate::bash_background::powershell_available())
                    .unwrap()
            )
            .unwrap()
        );
    }
    // A session starter preflights its worker plan on an unscoped route. The
    // plan's params are validated and accepted there, the answer is the same
    // as a paramless fetch, and nothing is frozen without a scope.
    let planned_fetch = fetch(&plan("broca-worker"));
    let mut paramless_fetch = planned_fetch.clone();
    paramless_fetch.as_object_mut().unwrap().remove("params");
    let paramless = catalog(paramless_fetch, &bind, &ctx).unwrap();
    let planned = catalog(planned_fetch, &bind, &ctx).unwrap();
    assert_eq!(
        serde_json::to_vec(&planned).unwrap(),
        serde_json::to_vec(&paramless).unwrap()
    );
    assert!(lookup(&ctx, &RemoteSource::Worker(key(&bind, Some("worker")))).is_none());
    let error = catalog(
        json!({"preset":"worker","params":{"not_a_key":true}}),
        &bind,
        &ctx,
    )
    .unwrap_err();
    assert!(serde_json::to_string(&error).unwrap().contains("not_a_key"));
    let error = catalog(
        json!({"preset":"head","params":{"remote_exec":{"enabled":true}}}),
        &bind,
        &ctx,
    )
    .unwrap_err();
    assert!(serde_json::to_string(&error)
        .unwrap()
        .contains("remote_exec"));
}

// Remote dispatch runs only on Unix.
#[cfg(unix)]
#[tokio::test]
async fn exec_remote_catalog_route_fetch_then_call_after_restart_routes() {
    exercise_catalog_remote_bash(crate::exec_remote::wire_tests::Script::Utf8, None).await;
}

#[cfg(unix)]
#[tokio::test]
async fn runon_subc_executor_refusal_returns_error_without_local_spawn() {
    use crate::exec_remote::wire_tests::Script;
    for (script, reason) in [
        (Script::KnownRefused, "unreachable"),
        (Script::WorkspaceSetupRefused, "workspace_setup_failed"),
        (Script::Refused, "future_refusal"),
    ] {
        exercise_catalog_remote_bash(script, Some(reason)).await;
    }
}

#[cfg(unix)]
async fn exercise_catalog_remote_bash(
    script: crate::exec_remote::wire_tests::Script,
    refusal: Option<&str>,
) {
    let daemon = crate::exec_remote::wire_tests::daemon(script, "exec-remote/v1").await;
    let root = tempfile::tempdir().unwrap();
    init_remote_git_fixture(root.path());
    let marker = root.path().join("must-not-run-locally");
    let command = if refusal.is_some() {
        format!("printf local-proof > '{}'", marker.display())
    } else {
        "cargo test".into()
    };
    let storage = tempfile::tempdir().unwrap();
    let bind = identity(root.path(), "one", 7, true);
    let root_id = bind.root.clone();
    let ctx = context(root.path(), storage.path(), Some(daemon.connection.clone()));
    ctx.app()
        .set_subc_connection_file(daemon.connection.clone());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root_id.clone(), Arc::new(ctx)));
    let (writer, mut replies) = mpsc::channel(8);
    let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 7, vec![]).unwrap();
    submit_provider_read(
        &writer,
        &frame,
        bind.clone(),
        &executor,
        &Arc::default(),
        &Arc::new(DispatchPathMetrics::new()),
        "tool.catalog",
        fetch(&plan("broca-worker")),
    )
    .await
    .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Response);
    // Simulate losing every in-memory route/context. The new call route opens
    // no catalog fetch; only the original persisted policy can route it.
    drop(executor);
    let ctx = context(root.path(), storage.path(), None);
    ctx.app()
        .set_subc_connection_file(daemon.connection.clone());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root_id.clone(), Arc::new(ctx)));
    let routes = HashMap::from([(route_key(42, 1), bind)]);
    // The frame loop reads the worker's saved confinement off the loop before
    // admission; this route saved none.
    let confinement = confined(
        &executor.actor_context(&root_id).unwrap(),
        &routes[&route_key(42, 1)],
    );
    assert_eq!(confinement, Ok(false));
    let call = Frame::build(
        FrameType::Request,
        control_flags(),
        42,
        1,
        8,
        serde_json::to_vec(
            &json!({"name":"bash","preset":"worker","arguments":{"command":command,"runon":"linux"}}),
        )
        .unwrap(),
    )
    .unwrap();
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    handle_tool_call(
        &writer,
        &call,
        None,
        Some(confinement),
        PhaseTrace::new(Instant::now()),
        &routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        &executor,
        &Arc::default(),
        &Arc::new(PendingDeferredSetups::default()),
        &Arc::new(Notify::new()),
        &PersistentCancelSignal::new(),
        &bash_tx,
        &touch_tx,
        &Arc::new(DispatchPathMetrics::new()),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut 1,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        &mut HashMap::new(),
        |request, ctx| crate::commands::bash::handle(&request, ctx),
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
    )
    .await
    .unwrap();
    let done = tokio::time::timeout(Duration::from_secs(15), bash_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let response = done.response_for_test();
    if let Some(reason) = refusal {
        assert!(
            !marker.exists(),
            "subc runon spawned locally after {reason}"
        );
        assert!(!response.success, "{response:?}");
        assert_eq!(response.data["code"], "remote_unavailable", "{response:?}");
        assert_eq!(response.data["message"], format!(
            "runon refused: remote refused: {reason}; command was not run; retry, or omit runon to run locally"
        ));
    } else {
        assert!(response.success, "{response:?}");
        assert!(
            response.data["output"]
                .as_str()
                .is_some_and(|s| s.starts_with("ran remotely on ck-motor\n")),
            "{response:?}"
        );
    }
    let log = daemon.log.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
    let request = &log
        .iter()
        .find(|(_, b)| b["method"] == "exec.run")
        .unwrap()
        .1;
    assert_eq!(
        request["params"]["siblings"],
        plan("broca-worker")["tool_items"][0]["params"]["siblings"]
    );
}

#[tokio::test]
async fn exec_remote_module_loop_captures_its_authenticated_daemon_endpoint() {
    let ctx = test_support::test_ctx();
    let app = ctx.app();
    let executor = Arc::new(Executor::new());
    let dir = tempfile::tempdir().unwrap();
    let endpoint = dir.path().join("module-connection.json");
    let (mut peer, module) = tokio::io::duplex(65536);
    let (read, write) = tokio::io::split(module);
    let peer_task = async {
        let hello = subc_transport::read_frame(&mut peer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let ack = Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: vec![],
                subc_capabilities: vec![],
                storage: None,
                machine_id: None,
            })
            .unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(&mut peer, &ack).await.unwrap();
        let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, vec![]).unwrap();
        subc_transport::write_frame(&mut peer, &goodbye)
            .await
            .unwrap();
    };
    let module_task = run_module_loop(
        read,
        write,
        &endpoint,
        app.clone(),
        executor,
        |req, _| Response::success(req.id, json!({})),
        None,
        false,
        1024 * 1024,
        None,
        dir.path(),
        None,
        None,
    );
    let (result, _) = tokio::join!(module_task, peer_task);
    result.unwrap();
    assert_eq!(app.subc_connection_file(), Some(endpoint));
}

#[cfg(unix)]
#[tokio::test]
async fn exec_remote_scope_drain_detaches_but_explicit_cancel_kills() {
    for cancelled in [false, true] {
        let daemon = crate::exec_remote::wire_tests::daemon(
            crate::exec_remote::wire_tests::Script::Cancel,
            "exec-remote/v1",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        init_remote_git_fixture(dir.path());
        let ctx = context(dir.path(), dir.path(), Some(daemon.connection.clone()));
        let registry = ctx.bash_background().clone();
        let task = registry
            .spawn_remote(
                crate::bash_background::RemoteLaunch {
                    explicit_runon: false,
                    params: Default::default(),
                    connection_file: Some(daemon.connection.clone()),
                    harness: "runner".into(),
                    session: "session".into(),
                    requested_platform: None,
                    requested_vcpus: None,
                    requested_network: false,
                },
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "cargo test",
                crate::bash_background::registry::resolve_posix_shell(),
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                dir.path().into(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(dir.path().into()),
            )
            .unwrap();
        registry.begin_wait_mode_session("session", &task);
        bash::detach_held_bash_in_background(
            drain::BashDetachTarget {
                run_scoped: false,
                registry: registry.clone(),
                task_id: task.clone(),
                session_id: "session".into(),
                wait_mode: true,
                worker_session: true,
                server_completion: true,
                late_results: false,
                request_id: "remote-drain".into(),
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                format_context: Default::default(),
            },
            cancelled,
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while registry.active_wait_session_count() != 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status = registry
            .observed_status(&task, "session", 0)
            .unwrap()
            .info
            .status;
        assert_eq!(
            status,
            if cancelled {
                crate::bash_background::BgTaskStatus::Killed
            } else {
                crate::bash_background::BgTaskStatus::Running
            }
        );
        let sent = daemon
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, b)| b["method"] == "exec.cancel");
        assert_eq!(sent, cancelled);
        if !cancelled {
            tokio::task::spawn_blocking(move || registry.kill(&task, "session").unwrap())
                .await
                .unwrap();
        }
    }
}

#[cfg(unix)]
fn init_remote_git_fixture(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    let git = |args: &[&str]| {
        let mut command = std::process::Command::new("git");
        let output = crate::test_env::apply_hermetic_git_env(command.current_dir(root))
            .args(args)
            .output()
            .expect("run git fixture command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&["config", "user.name", "AFT Test"]);
    git(&["config", "user.email", "aft-test@example.invalid"]);
    std::fs::write(root.join("tracked.txt"), "fixture\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["commit", "--quiet", "-m", "fixture"]);
}

fn bash_properties(answer: &Value, tool: &str) -> Vec<String> {
    answer["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == tool)
        .unwrap_or_else(|| panic!("{tool} is served: {answer}"))["input_schema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[cfg(unix)]
#[test]
fn worker_catalog_offers_runon_only_with_an_enabled_frozen_plan() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let enabled = catalog(
        fetch(&plan("broca-worker")),
        &identity(root.path(), "enabled", 7, true),
        &ctx,
    )
    .unwrap();
    assert!(bash_properties(&enabled, "bash").contains(&"runon".to_string()));
    // The PowerShell tool never offers it: the runner runs bash.
    if crate::bash_background::powershell_available() {
        assert!(!bash_properties(&enabled, "powershell").contains(&"runon".to_string()));
    }
    let disabled = catalog(
        fetch(&plan("broca-worker-policy-disabled")),
        &identity(root.path(), "disabled", 7, true),
        &ctx,
    )
    .unwrap();
    assert!(!bash_properties(&disabled, "bash").contains(&"runon".to_string()));
    let paramless = catalog(
        json!({"op":"tool.catalog","preset":"worker"}),
        &identity(root.path(), "paramless", 7, true),
        &ctx,
    )
    .unwrap();
    assert!(!bash_properties(&paramless, "bash").contains(&"runon".to_string()));
    // The same enabled plan on an unscoped preflight freezes nothing, so
    // nothing could honour the argument and it is not offered.
    let unscoped = catalog(
        fetch(&plan("broca-worker")),
        &identity(root.path(), "unscoped", 7, false),
        &ctx,
    )
    .unwrap();
    assert!(!bash_properties(&unscoped, "bash").contains(&"runon".to_string()));
    // A project that turned remote runs off is never offered them.
    let mut off = crate::config::Config::default();
    off.project_root = Some(root.path().into());
    off.storage_dir = Some(storage.path().into());
    off.sandbox.profile.head = crate::config::SandboxMode::Off;
    off.remote_exec.project_off = true;
    let off_ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), off);
    off_ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&storage.path().join("aft.db")).unwrap(),
    )));
    let project_off = catalog(
        fetch(&plan("broca-worker")),
        &identity(root.path(), "project-off", 7, true),
        &off_ctx,
    )
    .unwrap();
    assert!(!bash_properties(&project_off, "bash").contains(&"runon".to_string()));
}

#[test]
fn new_plan_shape_decodes_and_its_default_demand_reaches_the_session() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let bind = identity(root.path(), "new-shape", 7, true);
    let answer = catalog(
        json!({"op":"tool.catalog","preset":"worker","params":{"remote_exec":{"enabled":true,"default_demand":"linux,4c"}}}),
        &bind,
        &ctx,
    )
    .unwrap();
    // A frozen plan is portable, but only Unix hosts can execute remote bash.
    // Decoding and retaining the policy must not advertise an unusable argument.
    #[cfg(unix)]
    assert!(bash_properties(&answer, "bash").contains(&"runon".to_string()));
    #[cfg(not(unix))]
    assert!(!bash_properties(&answer, "bash").contains(&"runon".to_string()));
    let policy = lookup(&ctx, &RemoteSource::Worker(key(&bind, Some("worker"))))
        .unwrap()
        .params
        .remote_exec
        .unwrap();
    assert!(policy.enabled);
    assert_eq!(policy.default_demand.as_deref(), Some("linux,4c"));
}

#[test]
fn head_sessions_take_remote_runs_from_the_user_config_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let head = RemoteSource::Head {
        harness: "opencode".into(),
        session: "head".into(),
    };
    let ctx = context(root.path(), storage.path(), None);
    assert!(lookup(&ctx, &head).is_none());
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().into());
    config.remote_exec.enabled = true;
    config.remote_exec.default_demand = Some("linux".into());
    let enabled = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let launch = lookup(&enabled, &head).unwrap();
    assert_eq!(launch.harness, "opencode");
    assert_eq!(launch.session, "head");
    let policy = launch.params.remote_exec.unwrap();
    assert!(policy.enabled);
    assert_eq!(policy.default_demand.as_deref(), Some("linux"));
    // A worker never inherits the head's user config.
    assert!(lookup(&enabled, &RemoteSource::Worker(None)).is_none());
    assert!(lookup(&enabled, &RemoteSource::None).is_none());
}

#[cfg(unix)]
#[test]
fn remote_enabled_catalog_is_rendered_once_after_validation() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let worker = identity(root.path(), "single-render-worker", 7, true);
    let ctx = context(root.path(), storage.path(), None);
    let before = tool_provider::CATALOG_RENDERS.get();
    let reply = catalog(fetch(&plan("broca-worker")), &worker, &ctx).unwrap();
    assert_eq!(tool_provider::CATALOG_RENDERS.get() - before, 1);
    assert!(bash_catalog_entry(&reply)["input_schema"]["properties"]
        .get("runon")
        .is_some());
    let bad = json!({"preset":"worker", "params": {"remote_exec": plan("broca-worker")["remote_exec"]}, "system_text":{"preset":"reader"}});
    assert!(catalog(
        bad,
        &identity(root.path(), "invalid-catalog", 7, true),
        &ctx
    )
    .is_err());
    assert_eq!(tool_provider::CATALOG_RENDERS.get() - before, 1);
}
