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
            launch: access_grants::launch_key(route, "worker"),
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
        head_catalog_key: None,
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
/// during that read, even the racing call must observe the saved fence.
#[test]
fn unconfined_read_racing_a_confining_save_is_not_remembered() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let older = identity(root.path(), "designer", 7, true);
    let newer = identity(root.path(), "designer", 8, true);
    let delayed = identity(root.path(), "designer", 9, true);
    let db = ctx.db().unwrap();
    let delayed_key = key(&delayed, Some("worker")).unwrap();
    let generation = ctx.policy_confinement_generation();
    assert!(!store::confined(&db.lock().unwrap(), &delayed_key).unwrap());
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
    let read = CarriedConfinement::read(&ctx, &newer);
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &newer, "bash", Some(&read))
            .unwrap_err()
            .code,
        "tool_not_offered_confined"
    );
    assert_eq!(read.read, Ok(true));
    assert_eq!(ctx.policy_confinement_reads_for_test(), 2);
    assert_eq!(confinement_from_memory(&ctx, &older), Some(Ok(true)));
    assert_eq!(confinement_from_memory(&ctx, &newer), Some(Ok(true)));
    let reads = ctx.policy_confinement_reads_for_test();
    assert_eq!(
        admit_confined(&ctx, &newer, "bash").unwrap_err().code,
        "tool_not_offered_confined"
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), reads);
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &newer, "bash", None)
            .unwrap_err()
            .code,
        "tool_not_offered_confined"
    );

    // The other read finished before the save, but has not been memoized yet.
    // Even with read retries, memoization must reject a stale generation: a
    // save can land after the retry's generation check and before remembering.
    ctx.remember_unconfined_policy_key(delayed_key, generation, &db);
    assert_eq!(
        confinement_from_memory(&ctx, &delayed),
        None,
        "an unconfined answer read before a confining save was remembered"
    );
    assert_eq!(confined(&ctx, &delayed), Ok(true));
}

#[test]
fn confine_bash_admission_racing_a_confining_save_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let older = identity(root.path(), "designer", 7, true);
    let newer = identity(root.path(), "designer", 8, true);
    ctx.after_next_policy_read_for_test(move |ctx| {
        catalog(
            json!({"preset":"worker", "params":{"confine":"root"}}),
            &older,
            ctx,
        )
        .unwrap();
    });
    assert_eq!(
        admit_confined(&ctx, &newer, "bash").unwrap_err().code,
        "tool_not_offered_confined"
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), 2);
}

#[test]
fn confine_lookup_fails_closed_when_generation_keeps_changing() {
    fn invalidate_next_read(ctx: &AppContext, key: store::PolicyKey) {
        ctx.after_next_policy_read_for_test(move |ctx| {
            ctx.remember_confined_policy_key(key.clone());
            invalidate_next_read(ctx, key);
        });
    }

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let older = identity(root.path(), "designer", 7, true);
    let newer = identity(root.path(), "designer", 8, true);
    invalidate_next_read(&ctx, key(&older, Some("worker")).unwrap());
    let read = CarriedConfinement::read(&ctx, &newer);
    assert_eq!(
        read.read.as_ref().unwrap_err().code,
        "confine_policy_unavailable"
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), 3);
    assert_eq!(confinement_from_memory(&ctx, &newer), None);
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &newer, "bash", Some(&read))
            .unwrap_err()
            .code,
        "confine_policy_unavailable"
    );
}

#[test]
fn confine_lookup_never_waits_for_the_database_handle_slot_on_the_frame_loop() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context(root.path(), storage.path(), None));
    let route = identity(root.path(), "designer", 7, true);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let app = ctx.app();
    let holder = std::thread::spawn(move || {
        app.with_database_slot_locked_for_test(|| {
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let started = Instant::now();
    let answer = confinement_from_memory(&ctx, &route);
    let elapsed = started.elapsed();

    let (answer_tx, answer_rx) = mpsc::channel();
    let reading = Arc::clone(&ctx);
    let background = std::thread::spawn(move || {
        answer_tx.send(confined(&reading, &route)).unwrap();
    });
    let before_release = answer_rx.recv_timeout(Duration::from_millis(50));
    let _ = release_tx.send(());
    holder.join().unwrap();
    background.join().unwrap();

    assert!(
        elapsed < Duration::from_secs(1),
        "frame loop waited {elapsed:?}"
    );
    assert_eq!(answer, None);
    assert_eq!(before_release, Err(mpsc::RecvTimeoutError::Timeout));
    assert_eq!(
        answer_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        Ok(false)
    );
    assert_eq!(ctx.policy_confinement_reads_for_test(), 1);
}

fn admission_frame() -> super::super::DecodedFrame {
    admission_frame_for("bash", json!({"command": "true"}))
}

fn admission_frame_for(name: &str, arguments: Value) -> super::super::DecodedFrame {
    use super::super::*;
    DecodedFrame {
        parsed_body: Default::default(),
        confinement: None,
        launch_restore: None,
        codemode_policies: None,
        frame: Frame::build(
            FrameType::Request,
            control_flags(),
            43,
            1,
            7,
            serde_json::to_vec(&json!({
                "name": name, "arguments": arguments, "call_key": "busy-db", "preset": "worker"
            }))
            .unwrap(),
        )
        .unwrap(),
        phase_trace: PhaseTrace::new(Instant::now()),
    }
}

#[tokio::test]
async fn unconfined_call_keeps_policy_answer_when_database_slot_becomes_busy() {
    use super::super::{persistence::DatabaseWaits, route_key};
    use std::sync::mpsc;

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context(root.path(), storage.path(), None));
    let route = identity(root.path(), "designer", 7, true);
    assert_eq!(confined(&ctx, &route), Ok(false));
    let executor = crate::executor::Executor::new();
    assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
    let routes = HashMap::from([(route_key(43, 1), route.clone())]);
    let mut waits = DatabaseWaits::default();
    let decoded = waits
        .read_confinement(admission_frame(), &routes, &executor)
        .err()
        .expect("a memoized policy needs no background read");

    // The slot can become busy after the memory probe but before admission.
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let app = ctx.app();
    let holder = std::thread::spawn(move || {
        app.with_database_slot_locked_for_test(|| {
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let started = Instant::now();
    assert_eq!(confinement_from_memory(&ctx, &route), None);
    let admitted = admit_confined_on_frame_loop(&ctx, &route, "bash", decoded.confinement.as_ref());
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "frame loop blocked: {elapsed:?}"
    );
    assert_eq!(admitted, Ok(false));
    catalog(
        json!({"preset":"worker", "params":{"confine":"root"}}),
        &route,
        &ctx,
    )
    .unwrap();
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &route, "bash", decoded.confinement.as_ref())
            .unwrap_err()
            .code,
        "tool_not_offered_confined",
        "a newly saved fence must override the carried unconfined answer"
    );
}

#[tokio::test]
async fn uncached_calls_wait_for_busy_database_slot_and_keep_saved_confinement() {
    use super::super::{persistence::DatabaseWaits, route_key};
    use std::sync::mpsc;

    for (has_database, saved_confined) in [(false, false), (true, false), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let route = identity(root.path(), "designer", 7, true);
        if saved_confined {
            catalog(
                json!({"preset":"worker", "params":{"confine":"root"}}),
                &route,
                &context(root.path(), storage.path(), None),
            )
            .unwrap();
        }
        let ctx = Arc::new(if has_database {
            context(root.path(), storage.path(), None)
        } else {
            context_without_database(root.path(), storage.path())
        });
        let executor = crate::executor::Executor::new();
        assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
        let routes = HashMap::from([(route_key(43, 1), route.clone())]);
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let app = ctx.app();
        let holder = std::thread::spawn(move || {
            app.with_database_slot_locked_for_test(|| {
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            });
        });
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut waits = DatabaseWaits::default();
        let started = Instant::now();
        assert!(waits
            .read_confinement(admission_frame(), &routes, &executor)
            .is_ok());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), waits.next())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let decoded = tokio::time::timeout(Duration::from_secs(5), waits.next())
            .await
            .unwrap();
        assert_eq!(
            decoded.confinement.as_ref().map(|read| &read.read),
            Some(&Ok(saved_confined))
        );
        let admitted =
            admit_confined_on_frame_loop(&ctx, &route, "bash", decoded.confinement.as_ref());
        if saved_confined {
            assert_eq!(admitted.unwrap_err().code, "tool_not_offered_confined");
        } else {
            assert_eq!(admitted, Ok(false));
        }
    }
}

#[tokio::test]
async fn keyed_call_waits_for_slow_policy_store_before_confinement_admission() {
    use super::super::{persistence::DatabaseWaits, route_key};

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context_without_database(root.path(), storage.path()));
    ctx.mark_database_runtime_initializing_for_test();
    let route = identity(root.path(), "designer", 7, true);
    let executor = crate::executor::Executor::new();
    assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
    let routes = HashMap::from([(route_key(43, 1), route.clone())]);
    let mut waits = DatabaseWaits::default();
    // Keyed calls bypass DatabaseWaits::defer because the ledger owns their
    // persistence admission. Their confinement lookup still needs the open.
    assert!(waits
        .read_confinement(admission_frame(), &routes, &executor)
        .is_ok());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), waits.next())
            .await
            .is_err()
    );
    ctx.set_db(Arc::new(std::sync::Mutex::new(
        crate::db::open(&storage.path().join("aft.db")).unwrap(),
    )));
    ctx.finish_database_runtime(Ok(()));
    let decoded = tokio::time::timeout(Duration::from_secs(5), waits.next())
        .await
        .unwrap();
    assert_eq!(
        decoded.confinement.as_ref().map(|read| &read.read),
        Some(&Ok(false))
    );
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &route, "bash", decoded.confinement.as_ref()),
        Ok(false)
    );
}

#[tokio::test]
async fn policy_read_wait_budget_exhaustion_fails_closed() {
    use super::super::{persistence::DatabaseWaits, route_key};
    use std::sync::mpsc;

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context(root.path(), storage.path(), None));
    let route = identity(root.path(), "designer", 7, true);
    let executor = crate::executor::Executor::new();
    assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
    let routes = HashMap::from([(route_key(43, 1), route.clone())]);
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let app = ctx.app();
    let holder = std::thread::spawn(move || {
        app.with_database_slot_locked_for_test(|| {
            held_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut frame = admission_frame();
    // An already-expired budget avoids a ten-second sleep in this regression.
    frame.phase_trace = super::super::PhaseTrace::new(Instant::now() - Duration::from_secs(11));
    let mut waits = DatabaseWaits::default();
    let started = Instant::now();
    let decoded = waits
        .read_confinement(frame, &routes, &executor)
        .err()
        .expect("an exhausted budget must refuse without spawning a read");
    let admitted = admit_confined_on_frame_loop(&ctx, &route, "bash", decoded.confinement.as_ref());
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "a busy slot exceeded the policy wait budget"
    );
    assert_eq!(admitted.unwrap_err().code, "confine_policy_unavailable");
}

/// Force the blocking read past its memory fast path with an empty handle
/// slot. The hook releases the slot only after that probe returned None, so
/// neither scheduler timing nor a sleep decides which refusal branch is tested.
fn hold_empty_policy_slot_until_memory_miss(ctx: &Arc<AppContext>) -> std::thread::JoinHandle<()> {
    use std::sync::mpsc;

    assert!(ctx.db().is_none());
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (released_tx, released_rx) = mpsc::channel();
    let app = ctx.app();
    let holder = std::thread::spawn(move || {
        app.with_database_slot_locked_for_test(|| {
            held_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        released_tx.send(()).unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    ctx.after_next_policy_memory_miss_for_test(move || {
        release_tx.send(()).unwrap();
        released_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    holder
}

#[test]
fn failed_policy_store_direct_read_fails_closed_after_inconclusive_probe() {
    for busy in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let ctx = Arc::new(context_without_database(root.path(), storage.path()));
        ctx.mark_database_runtime_initializing_for_test();
        ctx.finish_database_runtime_error("configured store cannot open".into(), busy);
        assert!(!ctx.database_runtime_unconfigured());
        let route = identity(root.path(), "designer", 7, true);
        let holder = hold_empty_policy_slot_until_memory_miss(&ctx);
        let read = confined(&ctx, &route);
        holder.join().unwrap();
        assert_eq!(read.unwrap_err().code, "confine_policy_unavailable");
    }
}

#[tokio::test]
async fn failed_policy_store_frame_read_fails_closed_after_off_loop_wait() {
    use super::super::{persistence::DatabaseWaits, route_key};

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context_without_database(root.path(), storage.path()));
    ctx.mark_database_runtime_initializing_for_test();
    ctx.finish_database_runtime_error("configured store cannot open".into(), false);
    assert!(!ctx.database_runtime_unconfigured());
    let route = identity(root.path(), "designer", 7, true);
    let executor = crate::executor::Executor::new();
    assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
    let routes = HashMap::from([(route_key(43, 1), route.clone())]);
    let holder = hold_empty_policy_slot_until_memory_miss(&ctx);
    let frame = admission_frame_for("read", json!({"path":"input.txt"}));
    let mut waits = DatabaseWaits::default();
    assert!(waits.read_confinement(frame, &routes, &executor).is_ok());
    let decoded = tokio::time::timeout(Duration::from_secs(5), waits.next())
        .await
        .unwrap();
    holder.join().unwrap();
    // Check the off-loop answer itself: the frame loop's independent memory
    // refusal must not hide a blocking read that incorrectly returned false.
    assert_eq!(
        decoded.confinement.as_ref().map(|read| &read.read),
        Some(&Err(policy_unavailable()))
    );
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &route, "read", decoded.confinement.as_ref())
            .unwrap_err()
            .code,
        "confine_policy_unavailable"
    );
}

#[tokio::test]
async fn opening_policy_store_past_wait_budget_fails_closed() {
    use super::super::{persistence::DatabaseWaits, route_key, PhaseTrace};

    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context_without_database(root.path(), storage.path()));
    ctx.mark_database_runtime_initializing_for_test();
    assert!(!ctx.database_runtime_unconfigured());
    let route = identity(root.path(), "designer", 7, true);
    let executor = crate::executor::Executor::new();
    assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
    let routes = HashMap::from([(route_key(43, 1), route.clone())]);
    let mut frame = admission_frame_for("grep", json!({"pattern":"token"}));
    frame.phase_trace = PhaseTrace::new(Instant::now() - Duration::from_secs(11));
    let mut waits = DatabaseWaits::default();
    assert!(waits.defer(frame, &routes, &executor).is_ok());
    let expired = tokio::time::timeout(Duration::from_secs(1), waits.next())
        .await
        .unwrap();
    assert!(ctx.database_runtime_initializing());
    assert!(expired.confinement.is_none());

    // Once the opening wait expires, a contended empty slot must still be
    // classified as unavailable, not as a never-configured store.
    let holder = hold_empty_policy_slot_until_memory_miss(&ctx);
    let reading = Arc::clone(&ctx);
    let worker = route.clone();
    let read = tokio::task::spawn_blocking(move || confined(&reading, &worker))
        .await
        .unwrap();
    holder.join().unwrap();
    assert_eq!(read, Err(policy_unavailable()));

    let decoded = waits
        .read_confinement(expired, &routes, &executor)
        .err()
        .expect("the opening wait already exhausted the read budget");
    assert_eq!(
        decoded.confinement.as_ref().map(|read| &read.read),
        Some(&Err(policy_unavailable()))
    );
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &route, "grep", decoded.confinement.as_ref())
            .unwrap_err()
            .code,
        "confine_policy_unavailable"
    );
}

#[tokio::test]
async fn unconfigured_policy_store_direct_and_frame_reads_remain_unconfined() {
    use super::super::{persistence::DatabaseWaits, route_key};

    let root = tempfile::tempdir().unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().into()),
            storage_dir: None,
            ..Default::default()
        },
    ));
    assert!(ctx.database_runtime_unconfigured());
    let route = identity(root.path(), "designer", 7, true);
    let holder = hold_empty_policy_slot_until_memory_miss(&ctx);
    let read = confined(&ctx, &route);
    holder.join().unwrap();
    assert_eq!(read, Ok(false));

    let executor = crate::executor::Executor::new();
    assert!(executor.register_actor(route.root.clone(), Arc::clone(&ctx)));
    let routes = HashMap::from([(route_key(43, 1), route.clone())]);
    let holder = hold_empty_policy_slot_until_memory_miss(&ctx);
    let mut waits = DatabaseWaits::default();
    assert!(waits
        .read_confinement(
            admission_frame_for("read", json!({"path":"input.txt"})),
            &routes,
            &executor
        )
        .is_ok());
    let decoded = tokio::time::timeout(Duration::from_secs(5), waits.next())
        .await
        .unwrap();
    holder.join().unwrap();
    assert_eq!(
        decoded.confinement.as_ref().map(|read| &read.read),
        Some(&Ok(false))
    );
    assert_eq!(
        admit_confined_on_frame_loop(&ctx, &route, "read", decoded.confinement.as_ref()),
        Ok(false)
    );
}

fn identity(root: &Path, session: &str, epoch: u64, scoped: bool) -> RouteIdentity {
    let scope=serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"worker","scope_epoch":epoch,"kind":"worker","owner_authorized":true})).unwrap();
    RouteIdentity(Arc::new(RouteIdentityData {
        output_scope: Default::default(),
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
        held_reply: false,
        held_update: false,
        call_keepalive: false,
        call_yield: false,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        run_scope: OnceLock::new(),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        scope: scoped.then_some(scope),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
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

#[tokio::test]
async fn provider_catalog_fails_closed_when_worker_confinement_cannot_be_read() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = Arc::new(context_without_database(root.path(), storage.path()));
    ctx.mark_database_runtime_initializing_for_test();
    let route = identity(root.path(), "designer", 7, true);
    let executor = Arc::new(Executor::new());
    executor.register_actor(route.root.clone(), ctx);
    let (writer, mut replies) = mpsc::channel(8);
    let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 7, vec![]).unwrap();
    submit_provider_read(
        &writer,
        &frame,
        route,
        &executor,
        &Arc::default(),
        &Arc::new(DispatchPathMetrics::new()),
        "tool.catalog",
        json!({"preset":"worker"}),
        PhaseTrace::new(Instant::now()),
        test_support::test_request_meta(),
    )
    .await
    .unwrap();
    let reply = tokio::time::timeout(Duration::from_millis(500), replies.recv())
        .await
        .expect("an exhausted database-open wait must not be repeated by the provider")
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["code"], "confine_policy_unavailable", "{body}");
    assert!(
        body.get("tools").is_none(),
        "unreadable confinement must never offer a catalog"
    );
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
    let root =
        Path::new(crate::test_support::manifest_dir()).join("src/exec_remote/fixtures/plans");
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
    let mut request = json!({"op":"tool.catalog","preset":"worker","params":plan["tool_items"][0]["params"],"composition":plan["composition"]});
    // Wire vectors name illustrative checkouts on another machine. Tests that
    // export siblings must supply real, readable checkout roots of their own.
    request["params"]["siblings"] = json!([]);
    request
}

#[cfg(unix)]
mod catalog_runon_log_tests {
    use super::*;

    struct Case {
        _root: tempfile::TempDir,
        _storage: tempfile::TempDir,
        ctx: AppContext,
        route: RouteIdentity,
        request: Value,
    }

    impl Case {
        fn worker(params: Value) -> Self {
            let root = tempfile::tempdir().unwrap();
            let storage = tempfile::tempdir().unwrap();
            let ctx = context(root.path(), storage.path(), None);
            let route = identity(root.path(), "catalog-diagnostic", 7, true);
            let request = json!({"preset":"worker", "params":params});
            catalog(request.clone(), &route, &ctx).unwrap();
            Self {
                _root: root,
                _storage: storage,
                ctx,
                route,
                request,
            }
        }

        fn enabled() -> Self {
            Self::worker(json!({"remote_exec":{"enabled":true}}))
        }

        fn before_diagnostics(&self) -> Value {
            let preset = self.request["preset"].as_str().unwrap_or("head");
            // Preserve the pre-diagnostic predicate as an independent snapshot
            // oracle: logging must not change tools, descriptions or digests.
            let offered = cfg!(unix)
                && self.ctx.config().bash.runon_enabled
                && !self.ctx.config().remote_exec.project_off
                && matches!(self.route.trust, BindTrust::FirstParty)
                && lookup(&self.ctx, &source(&self.route, Some(preset))).is_some_and(|launch| {
                    launch
                        .params
                        .remote_exec
                        .as_ref()
                        .is_some_and(|policy| policy.enabled)
                });
            let mut request = self.request.clone();
            if let Some(params) = request.get_mut("params").and_then(Value::as_object_mut) {
                params.remove("remote_exec");
                params.remove("confine");
            }
            let (request, preset) = tool_provider::validate_catalog_request(request).unwrap();
            let render = if confined(&self.ctx, &self.route).unwrap() {
                tool_provider::catalog_for_confined_request
            } else {
                tool_provider::catalog_for_request
            };
            render(
                request,
                preset,
                &self.route.disabled_tools,
                crate::bash_background::powershell_available(),
                offered,
                self.ctx.config().revision_reads,
                self.ctx.config().bash.worker_wait_max_ms,
            )
            .unwrap()
        }

        fn check(
            &self,
            first_failed: &str,
            remote_exec_param: &str,
            key_status: &str,
            offered: bool,
        ) -> String {
            let before = self.before_diagnostics();
            let (answer, lines) = crate::logging::capture_log_lines(|| {
                catalog(self.request.clone(), &self.route, &self.ctx).unwrap()
            });
            assert_eq!(
                serde_json::to_vec(&answer["tools"]).unwrap(),
                serde_json::to_vec(&before["tools"]).unwrap(),
                "catalog tools changed after adding diagnostics"
            );
            assert_eq!(answer, before, "agent-visible catalog changed");
            let diagnostics: Vec<_> = lines
                .iter()
                .filter(|line| line.contains("catalog runon:"))
                .collect();
            assert_eq!(diagnostics.len(), 1, "captured logs: {lines:?}");
            let line = diagnostics[0];
            assert!(line.starts_with("INFO "), "{line}");
            for field in [
                "session=\"catalog-diagnostic\"".to_owned(),
                format!(
                    "preset={}",
                    self.request["preset"].as_str().unwrap_or("head")
                ),
                format!("first_failed={first_failed}"),
                format!("remote_exec_param={remote_exec_param}"),
                format!("key={key_status}"),
                format!("offered={offered}"),
                format!("runon_enabled={}", self.ctx.config().bash.runon_enabled),
                format!("project_off={}", self.ctx.config().remote_exec.project_off),
                format!(
                    "bind={}",
                    if matches!(self.route.trust, BindTrust::FirstParty) {
                        "first_party"
                    } else {
                        "other"
                    }
                ),
            ] {
                assert!(
                    line.split_whitespace().any(|word| word == field),
                    "missing {field}: {line}"
                );
            }
            assert!(
                !line.contains(&self.route.project_root.display().to_string()),
                "{line}"
            );
            if let Some(tools) = answer["tools"].as_array() {
                let bash = tools.iter().find(|tool| tool["name"] == "bash");
                assert!(
                    line.contains(&format!("bash_in_catalog={}", bash.is_some())),
                    "{line}"
                );
                assert_eq!(
                    bash.is_some_and(|bash| bash["input_schema"]["properties"]
                        .get("runon")
                        .is_some()),
                    offered,
                    "logged offer disagrees with served schema: {line}"
                );
            }
            line.to_string()
        }
    }

    #[test]
    fn preset_gate() {
        let mut case = Case::enabled();
        case.request = json!({"preset":"reader"});
        case.ctx
            .update_config(|config| config.remote_exec.enabled = true);
        case.check("preset", "absent", "missing:preset", false);
    }

    #[test]
    fn absent_remote_exec_gate() {
        Case::worker(json!({})).check("remote_exec", "absent", "complete", false);
    }

    #[test]
    fn disabled_remote_exec_gate() {
        Case::worker(json!({"remote_exec":{"enabled":false}})).check(
            "remote_exec",
            "disabled",
            "complete",
            false,
        );
    }

    #[test]
    fn missing_scope_key_gate() {
        let mut case = Case::enabled();
        Arc::get_mut(&mut case.route.0).unwrap().scope = None;
        case.check("key", "enabled", "missing:scope", false);
    }

    #[test]
    fn missing_principal_key_gate() {
        let mut case = Case::enabled();
        let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
            &mut Arc::get_mut(&mut case.route.0).unwrap().spawn_principal
        else {
            panic!("route bind")
        };
        *principal_id = None;
        case.check("key", "enabled", "missing:principal", false);
    }

    #[test]
    fn missing_scope_and_principal_key_gate() {
        let mut case = Case::enabled();
        let route = Arc::get_mut(&mut case.route.0).unwrap();
        route.scope = None;
        let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut route.spawn_principal
        else {
            panic!("route bind")
        };
        *principal_id = None;
        case.check("key", "enabled", "missing:scope,principal", false);
    }

    #[test]
    fn bind_gate() {
        let mut case = Case::enabled();
        Arc::get_mut(&mut case.route.0).unwrap().trust = BindTrust::Untrusted;
        case.check("bind", "enabled", "complete", false);
    }

    #[test]
    fn runon_enabled_gate() {
        let case = Case::enabled();
        case.ctx
            .update_config(|config| config.bash.runon_enabled = false);
        case.check("runon_enabled", "enabled", "complete", false);
    }

    #[test]
    fn project_off_gate() {
        let case = Case::enabled();
        case.ctx
            .update_config(|config| config.remote_exec.project_off = true);
        case.check("project_off", "enabled", "complete", false);
    }

    #[test]
    fn bash_disabled_gate() {
        let mut case = Case::enabled();
        Arc::get_mut(&mut case.route.0).unwrap().disabled_tools = Arc::new(vec!["bash".into()]);
        case.check("bash_in_catalog", "enabled", "complete", false);
    }

    #[test]
    fn bash_confined_gate() {
        Case::worker(json!({"remote_exec":{"enabled":true}, "confine":"root"})).check(
            "bash_in_catalog",
            "enabled",
            "complete",
            false,
        );
    }

    #[test]
    fn enabled_worker_and_paramless_refetch() {
        let mut case = Case::enabled();
        case.check("none", "enabled", "complete", true);
        case.request = json!({"preset":"worker"});
        case.check("none", "absent", "complete", true);
    }

    #[test]
    fn head_uses_user_policy_not_worker_key_or_params() {
        let mut case = Case::enabled();
        Arc::get_mut(&mut case.route.0).unwrap().scope = None;
        case.request = json!({});
        case.ctx
            .update_config(|config| config.remote_exec.enabled = true);
        case.check("none", "absent", "missing:preset", true);
    }

    #[test]
    fn digest_only_answer_logs_once() {
        let mut case = Case::enabled();
        case.request["digest_only"] = json!(true);
        let line = case.check("none", "enabled", "complete", true);
        assert!(line.contains("bash_in_catalog=true"), "{line}");
    }

    #[test]
    fn digest_only_confined_answer_has_no_bash() {
        let mut case = Case::worker(json!({"remote_exec":{"enabled":true}, "confine":"root"}));
        case.request["digest_only"] = json!(true);
        let line = case.check("bash_in_catalog", "enabled", "complete", false);
        assert!(line.contains("bash_in_catalog=false"), "{line}");
    }

    #[test]
    fn digest_only_disabled_bash_answer_has_no_bash() {
        let mut case = Case::enabled();
        case.request["digest_only"] = json!(true);
        Arc::get_mut(&mut case.route.0).unwrap().disabled_tools = Arc::new(vec!["bash".into()]);
        let line = case.check("bash_in_catalog", "enabled", "complete", false);
        assert!(line.contains("bash_in_catalog=false"), "{line}");
    }

    #[test]
    fn disabled_head_user_policy_gate() {
        let mut case = Case::enabled();
        case.request = json!({"preset":"head"});
        case.check("remote_exec", "absent", "missing:preset", false);
    }
}

#[cfg(not(unix))]
#[test]
fn catalog_runon_log_platform_gate() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "catalog-diagnostic", 7, true);
    let (request, preset) =
        tool_provider::validate_catalog_request(json!({"preset":"worker"})).unwrap();
    let before = tool_provider::catalog_for_request(
        request,
        preset,
        &route.disabled_tools,
        crate::bash_background::powershell_available(),
        false,
        ctx.config().revision_reads,
        ctx.config().bash.worker_wait_max_ms,
    )
    .unwrap();
    let (answer, lines) = crate::logging::capture_log_lines(|| {
        catalog(
            json!({"preset":"worker","params":{"remote_exec":{"enabled":true}}}),
            &route,
            &ctx,
        )
        .unwrap()
    });
    assert_eq!(answer, before, "agent-visible catalog changed");
    let lines: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("catalog runon:"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("INFO "), "{lines:?}");
    assert!(
        lines[0].contains("offered=false first_failed=platform"),
        "{lines:?}"
    );
}

#[test]
fn worker_reply_deadlines_track_resolved_config_only() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let route = identity(root.path(), "reply-deadline", 7, false);
    let mut previous = None;
    for (cap, expected) in [
        (60_000, 300_000),
        (300_000, 540_000),
        (1_800_000, 2_040_000),
    ] {
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
    for (cap, expected) in [
        (60_000, 300_000),
        (300_000, 540_000),
        (1_800_000, 2_040_000),
    ] {
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
fn exec_remote_catalog_validates_vocabulary_and_rejects_malformed_routing() {
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
        catalog(json!({"preset":"worker","params":malformed}), &bind, &ctx).unwrap_err();
        assert!(lookup(&ctx, &RemoteSource::Worker(key(&bind, Some("worker")))).is_none());
    }
}

#[test]
fn exec_remote_catalog_paramless_and_unscoped_parity() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let sibling = root.path().join("sibling");
    std::fs::create_dir_all(sibling.join(".git")).unwrap();
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
    let mut planned_fetch = fetch(&plan("broca-worker"));
    // The frozen plan uses Unix paths; validation needs a native absolute path.
    planned_fetch["params"]["siblings"] = json!([sibling]);
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
    let storage = tempfile::tempdir().unwrap();
    // Keep the execution sentinel outside the checkout so the write preflight
    // does not mask the executor refusal this helper is meant to exercise.
    let marker = storage.path().join("must-not-run-locally");
    let command = if refusal.is_some() {
        format!("printf local-proof > '{}'", marker.display())
    } else {
        "cargo test".into()
    };
    let bind = identity(root.path(), "one", 7, true);
    let root_id = bind.root.clone();
    let ctx = context(root.path(), storage.path(), Some(daemon.connection.clone()));
    ctx.app()
        .set_subc_connection_file(daemon.connection.clone());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root_id.clone(), Arc::new(ctx)));
    let (writer, mut replies) = mpsc::channel(8);
    let frame = Frame::build(FrameType::Request, control_flags(), 41, 1, 7, vec![]).unwrap();
    let mut request = fetch(&plan("broca-worker"));
    request["params"]["siblings"] = json!([root.path()]);
    submit_provider_read(
        &writer,
        &frame,
        bind.clone(),
        &executor,
        &Arc::default(),
        &Arc::new(DispatchPathMetrics::new()),
        "tool.catalog",
        request,
        PhaseTrace::new(Instant::now()),
        test_support::test_request_meta(),
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
    let confinement = CarriedConfinement::read(
        &executor.actor_context(&root_id).unwrap(),
        &routes[&route_key(42, 1)],
    );
    assert_eq!(confinement.read, Ok(false));
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
        None,
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
            "runon refused: remote refused: {reason}; command was not run; retry, or omit runon to run locally; remote job 0192a64a-1234-7000-8000-000000000001"
        ));
    } else {
        assert!(response.success, "{response:?}");
        assert!(
            response.data["output"]
                .as_str()
                .is_some_and(|s| s.starts_with(
                    "ran remotely on ck-motor (job 0192a64a-1234-7000-8000-000000000001)\n"
                )),
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
        json!([root.path().canonicalize().unwrap()])
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
        QuiescedLsp::ReapInBackground,
        None,
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
            if cancelled {
                crate::exec_remote::wire_tests::Script::Cancel
            } else {
                crate::exec_remote::wire_tests::Script::GatedAcceptance
            },
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
                    requested_memory_mib: None,
                    requested_network: false,
                    configured_env_names: Vec::new(),
                    return_paths: Vec::new(),
                },
                crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                "cargo test",
                crate::bash_background::registry::resolve_posix_shell(),
                "session".into(),
                dir.path().into(),
                HashMap::new(),
                crate::bash_background::HardKill::After(Duration::from_secs(30)),
                None,
                dir.path().into(),
                crate::bash_background::TaskSlot::Background { max: 10 },
                true,
                false,
                Some(dir.path().into()),
            )
            .unwrap();
        registry.begin_wait_mode_session("session", &task);
        daemon.wait_for_requests("exec.run", 1).await;
        bash::detach_held_bash_in_background(
            drain::BashDetachTarget {
                reply_cache: registry.drain_reply_cache(&task, "session"),
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
        if cancelled {
            assert_eq!(status, crate::protocol::BgTaskStatus::Killed);
        } else {
            assert_ne!(
                status,
                crate::protocol::BgTaskStatus::Killed,
                "draining the wait must not kill the detached task"
            );
            assert_eq!(
                status,
                crate::protocol::BgTaskStatus::Starting,
                "the fake runner has not sent Accepted yet"
            );
            daemon.acceptance_gate.notify_one();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let status = registry
                        .observed_status(&task, "session", 0)
                        .unwrap()
                        .info
                        .status;
                    assert_ne!(
                        status,
                        crate::protocol::BgTaskStatus::Killed,
                        "draining the wait must not kill the detached task"
                    );
                    if status == crate::protocol::BgTaskStatus::Running {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the runner's Accepted record should move the task to running");
        }
        let status = registry
            .observed_status(&task, "session", 0)
            .unwrap()
            .info
            .status;
        assert_eq!(
            status,
            if cancelled {
                crate::protocol::BgTaskStatus::Killed
            } else {
                crate::protocol::BgTaskStatus::Running
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

/// Answers one held server-completed call on `task` the way the module loop
/// does once a drain's backstop is due, and returns the error body it sent.
#[cfg(unix)]
async fn drained_detached_body(
    registry: &crate::bash_background::BgTaskRegistry,
    root: &Path,
    task: &str,
    corr: u64,
) -> Value {
    let metrics = DispatchPathMetrics::new();
    let route = route_key(41, 1);
    metrics.held_bash_calls.insert(route, corr);
    metrics.held_bash_calls.set_detach_target(
        route,
        corr,
        drain::BashDetachTarget {
            reply_cache: registry.drain_reply_cache(task, "session"),
            run_scoped: false,
            registry: registry.clone(),
            task_id: task.into(),
            session_id: "session".into(),
            wait_mode: false,
            worker_session: true,
            server_completion: true,
            late_results: false,
            request_id: format!("remote-detach-{corr}"),
            ver: PROTOCOL_VERSION,
            flags: control_flags(),
            format_context: Default::default(),
        },
    );
    metrics
        .held_bash_calls
        .set_phase(route, corr, drain::BashHoldPhase::Foreground);
    let routes = HashMap::from([(route, identity(root, "session", 1, false))]);
    let (writer, mut frames) = tokio::sync::mpsc::channel(4);
    assert_eq!(
        bash::answer_held_bash_calls_from_module_loop(&writer, &routes, &metrics, true)
            .await
            .unwrap(),
        1
    );
    let frame = frames.recv().await.unwrap();
    assert_eq!(frame.header.ty, FrameType::Error);
    serde_json::from_slice(&frame.body).unwrap()
}

/// A build-server call held when AFT restarts is answered `detached`. The
/// runner shows the agent only that error's code and message, so the message
/// names the task and its remote job: before the runner accepts the job, that
/// its inputs are still being captured; after, the job ID and phase.
#[cfg(unix)]
#[tokio::test]
async fn drained_remote_call_names_its_task_and_remote_job() {
    let daemon = crate::exec_remote::wire_tests::daemon(
        crate::exec_remote::wire_tests::Script::GatedAcceptance,
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
                explicit_runon: true,
                params: Default::default(),
                connection_file: Some(daemon.connection.clone()),
                harness: "runner".into(),
                session: "session".into(),
                requested_platform: None,
                requested_vcpus: None,
                requested_memory_mib: None,
                requested_network: false,
                configured_env_names: Vec::new(),
                return_paths: Vec::new(),
            },
            crate::sandbox_spawn::SpawnPlan::Unsandboxed,
            "cargo test",
            crate::bash_background::registry::resolve_posix_shell(),
            "session".into(),
            dir.path().into(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            dir.path().into(),
            crate::bash_background::TaskSlot::Background { max: 10 },
            true,
            false,
            Some(dir.path().into()),
        )
        .unwrap();
    daemon.wait_for_requests("exec.run", 1).await;
    let capture_warning = crate::commands::bash_orchestrate::REMOTE_INPUT_CAPTURE_WARNING;

    let submitting = drained_detached_body(&registry, dir.path(), &task, 1).await;
    assert_eq!(submitting["code"], "detached", "{submitting}");
    let message = submitting["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "call detached; command still running in the background as {task}. "
        )),
        "{message}"
    );
    assert!(
        message.contains(
            "\nDetached because AFT is restarting. Remote job: not assigned yet. Phase: submitting to ck-motor (capturing inputs; not accepted yet).\n"
        ),
        "{message}"
    );
    assert!(message.contains(capture_warning), "{message}");

    daemon.acceptance_gate.notify_one();
    let progress = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(progress) = registry
                .remote_progress(&task, "session")
                .filter(|progress| progress.job_id.is_some())
            {
                return progress;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the runner's Accepted record assigns a job ID");
    let accepted = drained_detached_body(&registry, dir.path(), &task, 2).await;
    let message = accepted["message"].as_str().unwrap();
    assert!(
        message.contains(&format!(
            "\nDetached because AFT is restarting. Remote job: {}. Phase: {}.",
            progress.job_id.unwrap(),
            progress.phase.describe()
        )),
        "{message}"
    );
    assert!(!message.contains(capture_warning), "{message}");
    tokio::task::spawn_blocking(move || registry.kill(&task, "session").unwrap())
        .await
        .unwrap();
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
fn head_worker_preflight_catalog_matches_worker_catalog() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let ctx = context(root.path(), storage.path(), None);
    for params in [
        json!({"remote_exec":{"enabled":true}, "siblings":[]}),
        json!({"remote_exec":{"enabled":false}}),
        json!({"confine":"root", "remote_exec":{"enabled":true}}),
    ] {
        let mut head = identity(root.path(), &format!("head-preflight-{params}"), 7, true);
        Arc::get_mut(&mut head.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap()
            .kind = subc_protocol::scope::ScopeKind::Head;
        let worker = identity(root.path(), &format!("worker-{params}"), 7, true);
        let request = json!({"op":"tool.catalog", "preset":"worker", "params":params});
        let preview = catalog(request.clone(), &head, &ctx).unwrap();
        let actual = catalog(request, &worker, &ctx).unwrap();
        assert_eq!(
            serde_json::to_vec(&preview).unwrap(),
            serde_json::to_vec(&actual).unwrap()
        );
        if params.get("confine").is_some() {
            assert!(preview["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["name"] != "bash"));
        } else {
            assert_eq!(
                bash_properties(&preview, "bash").contains(&"runon".to_string()),
                params["remote_exec"]["enabled"] == true
            );
        }
    }
}

#[test]
fn head_worker_preflight_validates_routing_without_a_policy_store() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.path().into()),
            ..Default::default()
        },
    );
    let mut head = identity(root.path(), "head-preflight", 7, true);
    Arc::get_mut(&mut head.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .kind = subc_protocol::scope::ScopeKind::Head;
    for params in [
        json!({"remote_exec":{"enabled":true}}),
        json!({"siblings":[root.path()]}),
        json!({"confine":"root"}),
    ] {
        catalog(json!({"preset":"worker", "params":params}), &head, &ctx).unwrap();
    }
    for params in [
        json!({"remote_exec":{"enabled":"yes"}}),
        json!({"siblings":["relative/path"]}),
        json!({"confine":"none"}),
    ] {
        assert!(catalog(json!({"preset":"worker", "params":params}), &head, &ctx).is_err());
    }
    assert!(!confined(&ctx, &head).unwrap());
}

#[test]
fn siblings_catalog_refuses_outside_checkout_and_dotdot_escape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workdir");
    let outside = dir.path().join("unrelated-checkout");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(outside.join(".git")).unwrap();
    let ctx = context(&root, &dir.path().join("store"), None);
    ctx.update_config(|config| config.restrict_to_project_root = false);
    let worker = identity(&root, "siblings-outside", 7, true);
    for sibling in [outside.clone(), root.join("..").join("unrelated-checkout")] {
        assert_siblings_refused(
            catalog(
                json!({"preset":"worker", "params":{"siblings":[sibling]}}),
                &worker,
                &ctx,
            ),
            &not_readable(&crate::access_policy::policy_path(&outside)),
        );
        assert!(lookup(&ctx, &RemoteSource::Worker(key(&worker, Some("worker")))).is_none());
    }
}

#[cfg(unix)]
#[test]
fn siblings_catalog_refuses_symlink_to_outside_checkout() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join(".git")).unwrap();
    let link = root.path().join("sibling");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let worker = identity(root.path(), "siblings-symlink", 7, true);
    ctx.update_config(|config| config.restrict_to_project_root = false);
    // The link's target is named, as the path that was refused.
    assert_siblings_refused(
        catalog(
            json!({"preset":"worker", "params":{"siblings":[link]}}),
            &worker,
            &ctx,
        ),
        &not_readable(&crate::access_policy::policy_path(outside.path())),
    );
}

#[test]
fn siblings_catalog_requires_checkout_root_and_saves_canonical_workdir() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let folder = root.path().join("folder");
    std::fs::create_dir(&folder).unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let worker = identity(root.path(), "siblings-workdir", 7, true);
    assert_siblings_refused(
        catalog(
            json!({"preset":"worker", "params":{"siblings":[folder]}}),
            &worker,
            &ctx,
        ),
        &not_checkout_root(&folder),
    );
    catalog(
        json!({"preset":"worker", "params":{"siblings":[folder.join("..")]}}),
        &worker,
        &ctx,
    )
    .unwrap();
    let saved = lookup(&ctx, &RemoteSource::Worker(key(&worker, Some("worker")))).unwrap();
    assert_eq!(
        saved.params.siblings,
        [root.path().canonicalize().unwrap().to_str().unwrap()]
    );
}

#[test]
fn siblings_catalog_uses_head_grants_and_worker_allow_paths() {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    std::fs::create_dir(outside.path().join(".git")).unwrap();
    std::fs::create_dir(outside.path().join("child")).unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let mut head = identity(root.path(), "siblings-head", 7, true);
    let head_stamp = Arc::get_mut(&mut head.0).unwrap().scope.as_mut().unwrap();
    head_stamp.kind = subc_protocol::scope::ScopeKind::Head;
    head_stamp.scope_ref = "siblings-head".into();
    access_grants::register(&head, &ctx).unwrap();
    catalog(
        json!({"preset":"head", "params":{"mode":"ask"}}),
        &head,
        &ctx,
    )
    .unwrap();
    let mut worker = identity(root.path(), "siblings-worker", 7, true);
    Arc::get_mut(&mut worker.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .parent = Some(
        serde_json::from_value(json!({
            "owner":head.scope.as_ref().unwrap().owner,
            "ref":"siblings-head", "scope_epoch":7
        }))
        .unwrap(),
    );
    let request =
        json!({"preset":"worker", "params":{"siblings":[outside.path().join("child/..")]}});
    for route in [&head, &worker] {
        assert_siblings_refused(
            catalog(request.clone(), route, &ctx),
            &not_readable(&crate::access_policy::policy_path(outside.path())),
        );
    }
    let granted_path = crate::access_policy::policy_path(outside.path());
    crate::db::access_grants::insert(
        &ctx.db().unwrap().lock().unwrap(),
        &access_grants::policy_scope_key(&head),
        &AccessGrant {
            path: granted_path.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    catalog(request.clone(), &head, &ctx).unwrap();
    // A head's grant is not inherited merely because the worker belongs to its project.
    assert!(catalog(request.clone(), &worker, &ctx).is_err());
    let mut worker = identity(root.path(), "siblings-granted-worker", 7, true);
    let stamp = Arc::get_mut(&mut worker.0).unwrap().scope.as_mut().unwrap();
    stamp.scope_ref = "siblings-granted-worker".into();
    stamp.parent = Some(
        serde_json::from_value(json!({
            "owner":head.scope.as_ref().unwrap().owner,
            "ref":"siblings-head", "scope_epoch":7
        }))
        .unwrap(),
    );
    let mut request = request;
    request["params"]["allow_paths"] = json!([{"path":granted_path, "mode":"read"}]);
    catalog(request, &worker, &ctx).unwrap();
    let saved = lookup(&ctx, &RemoteSource::Worker(key(&worker, Some("worker")))).unwrap();
    assert_eq!(
        saved.params.siblings,
        [outside.path().canonicalize().unwrap().to_str().unwrap()]
    );
}

/// The head's checks of a worker plan arrive over the orchestrator's own
/// connection (module `prefrontal-core`) rather than the head's session host,
/// so the route's principal is not a session host. The head's saved grants
/// then decide what it may delegate through allow_paths, but not what the
/// route's own file tools may read.
fn core_head(root: &Path, scope_ref: &str) -> RouteIdentity {
    let mut head = identity(root, scope_ref, 7, true);
    let data = Arc::get_mut(&mut head.0).unwrap();
    let stamp = data.scope.as_mut().unwrap();
    stamp.kind = subc_protocol::scope::ScopeKind::Head;
    stamp.scope_ref = scope_ref.into();
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
        *principal_id = Some("reserved:prefrontal-core".into());
    }
    head
}

/// A head (`head`) that holds a saved read grant on `granted`, a folder holding
/// `granted/commons`, a checkout root, and `granted/plain`, which is not one.
struct SiblingGrantFixture {
    dir: tempfile::TempDir,
    ctx: AppContext,
    head: RouteIdentity,
    head_grant: crate::access_policy::AccessGrant,
}

impl SiblingGrantFixture {
    fn new(scope_ref: &str) -> Self {
        use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("head");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(dir.path().join("granted/commons/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("granted/plain")).unwrap();
        std::fs::create_dir_all(dir.path().join("other/.git")).unwrap();
        let ctx = context(&root, &dir.path().join("store"), None);
        let head = core_head(&root, scope_ref);
        access_grants::register(&head, &ctx).unwrap();
        // Heads default to full access, which reads outside the project
        // without asking; asking leaves the saved grant as the head's only
        // access outside its project.
        catalog(
            json!({"preset":"head", "params":{"mode":"ask"}}),
            &head,
            &ctx,
        )
        .unwrap();
        let head_grant = AccessGrant {
            path: crate::access_policy::policy_path(&dir.path().join("granted")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        crate::db::access_grants::insert(
            &ctx.db().unwrap().lock().unwrap(),
            &access_grants::policy_scope_key(&head),
            &head_grant,
        )
        .unwrap();
        Self {
            dir,
            ctx,
            head,
            head_grant,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// The exact agent-visible refusal for a sibling that is not absolute, does not resolve,
/// or has no `.git` at its top, naming the path as the plan gave it. Spelled out
/// here so a wording change cannot pass unnoticed.
fn not_checkout_root(given: &Path) -> String {
    format!(
        "sibling {} is not the root of a git checkout; the command was not run",
        given.display()
    )
}

/// The exact agent-visible refusal for a checkout root the session may not read, naming
/// the resolved path.
fn not_readable(resolved: &Path) -> String {
    format!(
        "this session's file tools may not read sibling {}; grant read access to it in allow_paths; the command was not run",
        resolved.display()
    )
}

#[track_caller]
fn assert_siblings_refused(result: Result<Value, subc_protocol::ErrorBody>, message: &str) {
    let error = result.unwrap_err();
    assert_eq!(error.code, "invalid_request");
    assert_eq!(error.message, message);
    assert_eq!(error.detail.unwrap()["field"], "params.siblings");
}

/// A head checks a worker plan before the worker exists, so the plan's own
/// allow_paths grants are not saved yet. A sibling those grants cover passes
/// the check; the same plan without them is refused, and so is a sibling the
/// grants do not cover or one that is not a checkout root.
#[test]
fn siblings_head_preflight_counts_the_plans_own_allow_paths() {
    use crate::access_policy::policy_path;
    let fixture = SiblingGrantFixture::new("siblings-preflight-head");
    let (ctx, head) = (&fixture.ctx, &fixture.head);
    let commons = fixture.path("granted/commons");
    let granted = fixture.path("granted");
    let plan = |siblings: Value, allow_paths: Option<Value>| {
        let mut request = json!({"preset":"worker", "params":{
            "remote_exec":{"enabled":true}, "siblings":siblings,
        }});
        if let Some(allow_paths) = allow_paths {
            request["params"]["allow_paths"] = allow_paths;
        }
        request
    };
    // The head's saved grant does not let this route's own file tools read
    // the sibling, so without the plan's allow_paths it is refused.
    assert_siblings_refused(
        catalog(plan(json!([commons]), None), head, ctx),
        &not_readable(&policy_path(&commons)),
    );
    // A `sibling: true` entry reaches the catalog as the sibling plus a read
    // allow_paths entry for the same path.
    catalog(
        plan(
            json!([commons]),
            Some(json!([{"path":commons, "mode":"read"}])),
        ),
        head,
        ctx,
    )
    .unwrap();
    // A grant on a folder containing the sibling covers it too.
    catalog(
        plan(
            json!([commons]),
            Some(json!([{"path":granted, "mode":"read"}])),
        ),
        head,
        ctx,
    )
    .unwrap();
    let folder_grant = Some(json!([{"path":granted, "mode":"read"}]));
    // Covered, but not a checkout root.
    let plain = fixture.path("granted/plain");
    assert_siblings_refused(
        catalog(plan(json!([plain]), folder_grant.clone()), head, ctx),
        &not_checkout_root(&plain),
    );
    // A checkout root that no grant covers.
    let other = fixture.path("other");
    assert_siblings_refused(
        catalog(plan(json!([other]), folder_grant.clone()), head, ctx),
        &not_readable(&policy_path(&other)),
    );
    // A link inside the granted folder that resolves to a checkout outside it.
    #[cfg(unix)]
    {
        let link = granted.join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert_siblings_refused(
            catalog(plan(json!([link]), folder_grant), head, ctx),
            &not_readable(&policy_path(&other)),
        );
    }
    // The check saved nothing: the head holds only the grant it started with.
    assert_eq!(
        crate::db::access_grants::list(
            &ctx.db().unwrap().lock().unwrap(),
            &access_grants::policy_scope_key(head),
        )
        .unwrap(),
        [fixture.head_grant.clone()]
    );
}

/// The worker's own fetch saves the plan's allow_paths grants and then checks
/// its siblings against the session's saved grants only. Once the saved grant
/// is revoked, a refetch of the same plan is refused even though its
/// allow_paths entry still validates against the head.
#[test]
fn siblings_worker_fetch_counts_only_saved_grants() {
    let fixture = SiblingGrantFixture::new("siblings-saved-head");
    let ctx = &fixture.ctx;
    let commons = fixture.path("granted/commons");
    let mut worker = identity(&fixture.path("head"), "siblings-saved-worker", 7, true);
    let stamp = Arc::get_mut(&mut worker.0).unwrap().scope.as_mut().unwrap();
    stamp.scope_ref = "siblings-saved-worker".into();
    stamp.parent = Some(
        serde_json::from_value(json!({
            "owner":fixture.head.scope.as_ref().unwrap().owner,
            "ref":"siblings-saved-head", "scope_epoch":7
        }))
        .unwrap(),
    );
    let request = json!({"preset":"worker", "params":{
        "remote_exec":{"enabled":true}, "siblings":[commons],
        "allow_paths":[{"path":commons, "mode":"read"}],
    }});
    catalog(request.clone(), &worker, ctx).unwrap();
    let saved = lookup(ctx, &RemoteSource::Worker(key(&worker, Some("worker")))).unwrap();
    assert_eq!(
        saved.params.siblings,
        [commons.canonicalize().unwrap().to_str().unwrap()]
    );
    assert_eq!(
        crate::db::access_grants::revoke(
            &ctx.db().unwrap().lock().unwrap(),
            &access_grants::policy_scope_key(&worker),
            &crate::access_policy::policy_path(&commons),
        )
        .unwrap(),
        1
    );
    assert_siblings_refused(
        catalog(request, &worker, ctx),
        &not_readable(&crate::access_policy::policy_path(&commons)),
    );
}

/// Who carries a worker's catalog fetch. Broca, the worker's session host,
/// binds as `reserved:broca`. Core reads the worker's shell schema before the
/// worker starts over its own connection, so that route's principal is
/// `reserved:prefrontal-core`, the scope's owner, and not a session host.
const BROCA: &str = "reserved:broca";
const CORE: &str = "reserved:prefrontal-core";

/// A worker scope `scope_ref` of the fixture's head, carried by `principal`.
fn fixture_worker(
    fixture: &SiblingGrantFixture,
    scope_ref: &str,
    principal: &str,
) -> RouteIdentity {
    let head = fixture.head.scope.as_ref().unwrap();
    let parent = serde_json::from_value(json!({
        "owner":head.owner, "ref":head.scope_ref, "scope_epoch":head.scope_epoch,
    }))
    .unwrap();
    let mut worker = identity(&fixture.path("head"), "session", 7, true);
    let data = Arc::get_mut(&mut worker.0).unwrap();
    let stamp = data.scope.as_mut().unwrap();
    stamp.scope_ref = scope_ref.into();
    stamp.parent = Some(parent);
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
        *principal_id = Some(principal.into());
    }
    worker
}

/// The worker plan core sends with each fetch: runon enabled, `siblings`, and
/// a read `allow_paths` entry for each of `readable`. A `sibling: true` entry
/// reaches AFT as the sibling plus a read entry for the same path.
fn sibling_plan(siblings: &[&Path], readable: &[&Path]) -> Value {
    let allow_paths: Vec<Value> = readable
        .iter()
        .map(|path| json!({"path":path, "mode":"read"}))
        .collect();
    json!({"preset":"worker", "params":{
        "remote_exec":{"enabled":true}, "siblings":siblings, "allow_paths":allow_paths,
    }})
}

/// Core's fetches for a worker launched with a sibling checkout, in core's
/// order and with the same plan each time: the launch approval and the plan
/// composed at the first send, both under the head's scope, then the shell
/// schema read under the worker's own scope over core's connection. Broca
/// then fetches under the worker's scope, and the worker submits a runon job
/// naming the sibling.
#[cfg(unix)]
#[tokio::test]
async fn siblings_pass_cores_launch_fetches_then_brocas_fetch_and_submit() {
    let daemon = crate::exec_remote::wire_tests::daemon(
        crate::exec_remote::wire_tests::Script::Plain,
        "exec-remote/v1",
    )
    .await;
    let fixture = SiblingGrantFixture::new("siblings-launch-head");
    let ctx = &fixture.ctx;
    init_remote_git_fixture(&fixture.path("head"));
    ctx.update_config(|config| {
        config.semantic.subc_connection_file = Some(daemon.connection.clone());
    });
    let commons = fixture.path("granted/commons");
    let plan = sibling_plan(&[&commons], &[&commons]);
    for fetch in ["launch approval", "plan composition"] {
        if let Err(error) = catalog(plan.clone(), &fixture.head, ctx) {
            panic!("{fetch} under the head's scope: {error:?}");
        }
    }
    let core = fixture_worker(&fixture, "siblings-launch-worker", CORE);
    if let Err(error) = catalog(plan.clone(), &core, ctx) {
        panic!("shell schema read under the worker's scope: {error:?}");
    }
    let broca = fixture_worker(&fixture, "siblings-launch-worker", BROCA);
    catalog(plan, &broca, ctx).unwrap();
    let sibling = commons.canonicalize().unwrap();
    let launch = lookup(ctx, &RemoteSource::Worker(key(&broca, Some("worker")))).unwrap();
    assert_eq!(launch.params.siblings, [sibling.to_str().unwrap()]);
    // The worker's runon call, under Broca's route policy.
    let _scope = crate::access_policy::scope_guard(Some(access_grants::policy_scope_key(&broca)));
    let _grants =
        crate::access_policy::scope_grants_guard(access_grants::scope_grants_denial(&broca));
    let _ask =
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Consent {
            once: vec![],
            launch: access_grants::launch_key(&broca, "worker"),
        }));
    let run =
        crate::bash_background::registry::remote::submit_runon_for_test(ctx, launch, &daemon).await;
    assert_eq!(run["siblings"], json!([sibling]));
}

/// The worker-scope fetches still refuse a sibling the plan's allow_paths
/// leave out, a sibling that is not a checkout root, and an allow_paths entry
/// the head may not grant, with the same answer whether core or Broca
/// carries the fetch.
#[test]
fn siblings_worker_scope_fetches_still_refuse_ungranted_plans() {
    use crate::access_policy::policy_path;
    let fixture = SiblingGrantFixture::new("siblings-refused-head");
    let ctx = &fixture.ctx;
    let commons = fixture.path("granted/commons");
    let plain = fixture.path("granted/plain");
    let granted = fixture.path("granted");
    let other = fixture.path("other");
    let refused = |code: &str, message: String| (code.to_owned(), message);
    let cases = [
        // The plan grants a folder beside the sibling, not the sibling.
        (
            sibling_plan(&[&commons], &[&plain]),
            refused("invalid_request", not_readable(&policy_path(&commons))),
        ),
        (
            sibling_plan(&[&plain], &[&granted]),
            refused("invalid_request", not_checkout_root(&plain)),
        ),
        // `other` is a checkout root outside every grant the head holds.
        (
            sibling_plan(&[&other], &[&other]),
            refused("plan_param_refused", "allow_paths[0]: head_not_held".into()),
        ),
    ];
    for (index, (plan, expected)) in cases.into_iter().enumerate() {
        // Core's fetch comes first and Broca's follows, for the same worker.
        let scope_ref = format!("siblings-refused-worker-{index}");
        for principal in [CORE, BROCA] {
            let worker = fixture_worker(&fixture, &scope_ref, principal);
            let error = catalog(plan.clone(), &worker, ctx).unwrap_err();
            assert_eq!(
                (error.code, error.message),
                expected,
                "case {index} carried by {principal}"
            );
        }
        assert!(lookup(
            ctx,
            &RemoteSource::Worker(key(
                &fixture_worker(&fixture, &scope_ref, BROCA),
                Some("worker")
            ))
        )
        .is_none());
    }
}

/// A worker stamp whose owner the daemon did not authorize may not use the
/// grants saved for a worker scope of the same name, nor save its own, so its
/// siblings are refused whichever connection carries the fetch.
#[test]
fn siblings_unauthorized_worker_scope_is_refused() {
    use crate::access_policy::policy_path;
    let fixture = SiblingGrantFixture::new("siblings-unauthorized-head");
    let ctx = &fixture.ctx;
    let commons = fixture.path("granted/commons");
    let plan = sibling_plan(&[&commons], &[&commons]);
    let scope_ref = "siblings-unauthorized-worker";
    // The authorized worker scope of that name holds a grant on the sibling.
    catalog(
        plan.clone(),
        &fixture_worker(&fixture, scope_ref, BROCA),
        ctx,
    )
    .unwrap();
    let siblings_only = json!({"preset":"worker", "params":{
        "remote_exec":{"enabled":true}, "siblings":[commons],
    }});
    for principal in [CORE, BROCA] {
        let mut worker = fixture_worker(&fixture, scope_ref, principal);
        Arc::get_mut(&mut worker.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap()
            .owner_authorized = false;
        let error = catalog(plan.clone(), &worker, ctx).unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.message.as_str()),
            ("plan_param_refused", "allow_paths[0]: head_policy_unknown"),
            "carried by {principal}"
        );
        assert_siblings_refused(
            catalog(siblings_only.clone(), &worker, ctx),
            &not_readable(&policy_path(&commons)),
        );
    }
}

/// The waiver that lets core's worker-scope fetch count the worker's saved
/// grants covers only that fetch, carried by the scope's own owner. After
/// core's fetch has saved the worker's grant on the sibling, the same check is
/// still refused when a module that is neither a session host nor the owner
/// carries the fetch, when the stamp names a flow, and when the scope is not
/// an Alfonso worker scope. A read on core's connection outside the catalog
/// still may not use the saved grant. And a worker scope saves its launch
/// grants once, so a later plan naming the sibling in allow_paths adds none.
#[test]
fn siblings_owner_waiver_covers_only_the_owner_carried_worker_fetch() {
    use crate::access_policy::{policy_path, ScopeGrantsDenial};
    let fixture = SiblingGrantFixture::new("siblings-waiver-head");
    let ctx = &fixture.ctx;
    let commons = fixture.path("granted/commons");
    let plan = sibling_plan(&[&commons], &[&commons]);
    let refused = not_readable(&policy_path(&commons));
    let scope_ref = "siblings-waiver-worker";
    let core = fixture_worker(&fixture, scope_ref, CORE);
    catalog(plan.clone(), &core, ctx).unwrap();

    let module = "reserved:example-module";
    let other_carrier = fixture_worker(&fixture, scope_ref, module);
    assert_eq!(
        access_grants::scope_grants_denial(&other_carrier),
        Some(ScopeGrantsDenial::NotSessionHost)
    );
    assert_siblings_refused(catalog(plan.clone(), &other_carrier, ctx), &refused);

    let mut flow = fixture_worker(&fixture, scope_ref, CORE);
    Arc::get_mut(&mut flow.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .attributes
        .flow_id = Some("flow-1".into());
    assert_eq!(
        access_grants::scope_grants_denial(&flow),
        Some(ScopeGrantsDenial::Flow)
    );
    assert_siblings_refused(catalog(plan.clone(), &flow, ctx), &refused);

    // A worker scope owned by another module and carried by that owner, which
    // holds a saved grant on the sibling.
    let mut foreign = fixture_worker(&fixture, "siblings-waiver-foreign", module);
    Arc::get_mut(&mut foreign.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .owner =
        serde_json::from_value(json!({"kind":"reserved","module_id":"example-module"})).unwrap();
    access_grants::register(&foreign, ctx).unwrap();
    crate::db::access_grants::insert(
        &ctx.db().unwrap().lock().unwrap(),
        &access_grants::policy_scope_key(&foreign),
        &crate::access_policy::AccessGrant {
            path: policy_path(&commons),
            kind: crate::access_policy::GrantKind::Subtree,
            operation: crate::access_policy::AccessOperation::Read,
        },
    )
    .unwrap();
    assert_eq!(
        access_grants::scope_grants_denial(&foreign),
        Some(ScopeGrantsDenial::UnknownScopeKind)
    );
    assert_siblings_refused(
        catalog(
            json!({"preset":"worker", "params":{
                "remote_exec":{"enabled":true}, "siblings":[commons],
            }}),
            &foreign,
            ctx,
        ),
        &refused,
    );

    // A file read on core's connection, outside the catalog's sibling check.
    assert_eq!(
        access_grants::scope_grants_denial(&core),
        Some(ScopeGrantsDenial::NotSessionHost)
    );
    {
        let _scope =
            crate::access_policy::scope_guard(Some(access_grants::policy_scope_key(&core)));
        let _grants =
            crate::access_policy::scope_grants_guard(access_grants::scope_grants_denial(&core));
        let _ask =
            crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Consent {
                once: vec![],
                launch: access_grants::launch_key(&core, "worker"),
            }));
        assert!(ctx
            .validate_read_path_without_approval(
                "core-read",
                &core.session,
                &commons.canonicalize().unwrap()
            )
            .is_err());
    }

    let earlier = "siblings-waiver-earlier";
    catalog(
        sibling_plan(&[], &[&fixture.path("granted/plain")]),
        &fixture_worker(&fixture, earlier, CORE),
        ctx,
    )
    .unwrap();
    for principal in [CORE, BROCA] {
        assert_siblings_refused(
            catalog(
                plan.clone(),
                &fixture_worker(&fixture, earlier, principal),
                ctx,
            ),
            &refused,
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn siblings_submit_refuses_withdrawn_catalog_grant_before_daemon_contact() {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    let daemon = crate::exec_remote::wire_tests::daemon(
        crate::exec_remote::wire_tests::Script::Plain,
        "exec-remote/v1",
    )
    .await;
    let root = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    init_remote_git_fixture(root.path());
    std::fs::create_dir(sibling.path().join(".git")).unwrap();
    let ctx = context(root.path(), storage.path(), Some(daemon.connection.clone()));
    ctx.update_config(|config| config.restrict_to_project_root = false);
    let mut head = identity(root.path(), "siblings-submit-head", 7, true);
    let stamp = Arc::get_mut(&mut head.0).unwrap().scope.as_mut().unwrap();
    stamp.kind = subc_protocol::scope::ScopeKind::Head;
    stamp.scope_ref = "siblings-submit-head".into();
    access_grants::register(&head, &ctx).unwrap();
    let path = crate::access_policy::policy_path(sibling.path());
    crate::db::access_grants::insert(
        &ctx.db().unwrap().lock().unwrap(),
        &access_grants::policy_scope_key(&head),
        &AccessGrant {
            path: path.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    let mut worker = identity(root.path(), "session", 7, true);
    Arc::get_mut(&mut worker.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .parent = Some(
        serde_json::from_value(json!({
            "owner":head.scope.as_ref().unwrap().owner,
            "ref":"siblings-submit-head", "scope_epoch":7
        }))
        .unwrap(),
    );
    catalog(
        json!({"preset":"worker", "params":{
            "remote_exec":{"enabled":true}, "siblings":[path],
            "mode":"ask",
            "allow_paths":[{"path":path, "mode":"read"}]
        }}),
        &worker,
        &ctx,
    )
    .unwrap();
    let launch = lookup(&ctx, &RemoteSource::Worker(key(&worker, Some("worker")))).unwrap();
    let scope = access_grants::policy_scope_key(&worker);
    assert_eq!(
        crate::db::access_grants::revoke(&ctx.db().unwrap().lock().unwrap(), &scope, &path)
            .unwrap(),
        1
    );
    let _scope = crate::access_policy::scope_guard(Some(scope.clone()));
    let _ask =
        crate::access_policy::ask_route_guard(Some(crate::access_policy::AskRoute::Consent {
            once: vec![],
            launch: access_grants::launch_key(&worker, "worker"),
        }));
    let read = ctx
        .validate_read_path("siblings-read-control", &worker.session, &path)
        .unwrap_err();
    assert_eq!(read.data["code"], crate::access_policy::ACCESS_ASK_REQUIRED);
    let asks_before_submit = crate::context::confinement::ask_calls();
    crate::bash_background::registry::remote::assert_siblings_submit_refused_before_connect(
        &ctx,
        launch,
        &daemon,
        &format!("runon refused: {}", not_readable(&path)),
    );
    assert_eq!(
        crate::context::confinement::ask_calls(),
        asks_before_submit,
        "sibling submission must not enter the consent ask handler"
    );
    let db = ctx.db().unwrap();
    let conn = db.lock().unwrap();
    assert!(crate::db::access_asks::for_scope(&conn, &scope)
        .unwrap()
        .is_empty());
    assert!(crate::db::access_grants::list(&conn, &scope)
        .unwrap()
        .is_empty());
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

#[test]
fn malformed_routing_policy_is_refused_before_freeze_and_valid_retry_can_freeze() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let sibling = root.path().join("sibling");
    std::fs::create_dir_all(sibling.join(".git")).unwrap();
    let ctx = context(root.path(), storage.path(), None);
    let mut valid_fetch = fetch(&plan("broca-worker"));
    // Keep the plan's policy but replace its Unix sibling path for native validation.
    valid_fetch["params"]["siblings"] = json!([sibling]);
    for (index, (field, malformed)) in [
        ("remote_exec", json!({"remote_exec":{"enabled":"yes"}})),
        (
            "remote_exec",
            json!({"remote_exec":{"enabled":true,"unexpected":1}}),
        ),
        ("siblings", json!({"siblings":[4]})),
        ("siblings", json!({"siblings":["relative/path"]})),
    ]
    .into_iter()
    .enumerate()
    {
        let bind = identity(root.path(), &format!("invalid-routing-{index}"), 7, true);
        let worker_key = key(&bind, Some("worker"));
        let error =
            catalog(json!({"preset":"worker", "params":malformed}), &bind, &ctx).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        let serialized = serde_json::to_value(&error).unwrap();
        assert_eq!(serialized["detail"]["field"], format!("params.{field}"));
        assert!(lookup(&ctx, &RemoteSource::Worker(worker_key.clone())).is_none());
        catalog(valid_fetch.clone(), &bind, &ctx).unwrap();
        let frozen = lookup(&ctx, &RemoteSource::Worker(worker_key.clone())).unwrap();
        assert!(frozen.params.remote_exec.as_ref().unwrap().enabled);
        let before = serde_json::to_value(&frozen.params).unwrap();
        assert!(catalog(json!({"preset":"worker", "params":malformed}), &bind, &ctx).is_err());
        let after = lookup(&ctx, &RemoteSource::Worker(worker_key)).unwrap();
        assert_eq!(serde_json::to_value(&after.params).unwrap(), before);
    }
    let unscoped = identity(root.path(), "invalid-preflight", 7, false);
    assert!(catalog(
        json!({"preset":"worker", "params":{"siblings":[false]}}),
        &unscoped,
        &ctx
    )
    .is_err());
}
