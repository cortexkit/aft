use super::*;
use crate::bash_background::persistence::TaskArtifact;

fn output_log(frame: &Frame) -> OperationLog {
    OperationLog {
        op: "shells.output",
        caller: "desktop",
        channel: frame.header.channel,
        started: Instant::now(),
    }
}

#[test]
fn shells_v1_operation_logs_are_once_per_event_and_content_free() {
    use subc_protocol::Principal;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (task_id, lines) = crate::logging::capture_log_lines(|| {
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let (snapshot, reader, _task) = captured(dir.path(), b"private output bytes", false, true);
            let task_id = snapshot.metadata.task_id.clone();
            let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
            let sample = super::super::samples::row(&task_id, "background", false);
            for index in 0..51 {
                let mut row = sample.clone();
                if index != 0 {
                    row.task_id = format!("bash-{index:016x}");
                }
                service.state.write().await.rows.insert(row.task_id.clone(), row);
            }
            service.sources.write().await.insert(task_id.clone(), reader);
            let identity = ManagementIdentity {
                principal: Some(Principal::Reserved { module_id: "callosum".into() }),
                harness: Some("fed-phone:private-person".into()),
                ..Default::default()
            };
            let metrics = DispatchPathMetrics::new();
            let (tx, mut frames) = tokio::sync::mpsc::channel(16);
            let request = |corr| Frame::build(
                FrameType::Request, subc_protocol::Flags(0), 41, 1, corr, b"{}".to_vec(),
            ).unwrap();
            fn receive(frames: &mut tokio::sync::mpsc::Receiver<Frame>) -> tokio::time::Timeout<impl std::future::Future<Output = Option<Frame>> + '_> {
                tokio::time::timeout(Duration::from_secs(5), frames.recv())
            }

            // Holding refresh proves the timer includes work queued before the reply.
            let refresh = service.refresh_gate.lock().await;
            service.dispatch(&tx, &metrics, &request(1), &identity, &json!({"method":"shells.list"})).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(refresh);
            let list = receive(&mut frames).await.unwrap().unwrap();
            assert_eq!(list.header.ty, FrameType::Response);
            assert_eq!(serde_json::from_slice::<Value>(&list.body).unwrap()["result"]["rows"].as_array().unwrap().len(), 50);

            service.dispatch(&tx, &metrics, &request(2), &identity, &json!({"method":"shells.watch"})).await.unwrap();
            // Two initial resync parts must still produce only one open log.
            for _ in 0..2 {
                assert_eq!(receive(&mut frames).await.unwrap().unwrap().header.ty, FrameType::StreamData);
            }
            assert!(service.cancel(route_key(41, 1), 2).await);
            assert_eq!(receive(&mut frames).await.unwrap().unwrap().header.ty, FrameType::StreamEnd);

            let mut ended = project_row(&snapshot, None);
            ended.command = sample.command;
            service.state.write().await.rows.insert(task_id.clone(), ended);
            service.dispatch(&tx, &metrics, &request(3), &identity, &json!({"method":"shells.output","params":{"task_id":task_id}})).await.unwrap();
            loop {
                if receive(&mut frames).await.unwrap().unwrap().header.ty == FrameType::StreamEnd {
                    break;
                }
            }
            service.dispatch(&tx, &metrics, &request(4), &identity, &json!({"method":"shells.output","params":{"task_id":"secret-command\nsecond-line","ansi":"invalid"}})).await.unwrap();
            assert_eq!(receive(&mut frames).await.unwrap().unwrap().header.ty, FrameType::Error);
            service.drain().await;
            service.stop();
            task_id
        })
    });
    let lines: Vec<_> = lines
        .into_iter()
        .filter(|line| line.starts_with("INFO shells/v1 "))
        .collect();
    assert_eq!(lines.len(), 6, "one line per event: {lines:?}");
    let normalized: Vec<_> = lines
        .iter()
        .map(|line| {
            line.split_whitespace()
                .map(|field| {
                    if let Some(value) = field.strip_prefix("elapsed_ms=") {
                        value.parse::<u128>().unwrap();
                        "elapsed_ms=<ms>"
                    } else {
                        field
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(
        normalized[0],
        "INFO shells/v1 list: caller=phone channel=41 rows=50 elapsed_ms=<ms>"
    );
    let elapsed: u128 = lines[0]
        .split("elapsed_ms=")
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        elapsed >= 30,
        "list latency omitted refresh wait: {}",
        lines[0]
    );
    assert_eq!(
        normalized[1],
        "INFO shells/v1 open: op=shells.watch caller=phone channel=41 elapsed_ms=<ms>"
    );
    assert_eq!(normalized[2], "INFO shells/v1 end: op=shells.watch caller=phone channel=41 reason=client_cancel elapsed_ms=<ms>");
    assert_eq!(normalized[3], format!("INFO shells/v1 open: op=shells.output caller=phone channel=41 elapsed_ms=<ms> task_id={task_id:?}"));
    assert_eq!(normalized[4], "INFO shells/v1 end: op=shells.output caller=phone channel=41 reason=task_finished elapsed_ms=<ms>");
    assert_eq!(
        normalized[5],
        "INFO shells/v1 refusal: op=shells.output code=invalid_params"
    );
    for line in &lines {
        assert!(
            !line.contains('\n') && !line.contains('\r'),
            "multiline event: {line:?}"
        );
        for content in [
            "TOKEN",
            "fixture-secret-value",
            "cargo test",
            "REDACTED",
            "secret-command",
            "second-line",
            "private output bytes",
            "printf progress",
            "/work/project",
            "session-alf",
            "plain-session",
            "private-person",
        ] {
            assert!(!line.contains(content), "content leaked in log: {line}");
        }
    }
}

fn status_context() -> (
    tempfile::TempDir,
    Arc<crate::context::AppContext>,
    Arc<Executor>,
    Arc<App>,
) {
    let dir = tempfile::tempdir().unwrap();
    let app = App::default_shared();
    let ctx = Arc::new(crate::context::AppContext::with_app_and_provider(
        app.clone(),
        Box::new(crate::language::StubProvider),
        crate::config::Config {
            project_root: Some(dir.path().to_owned()),
            disabled_lsp: ["rust".into()].into(),
            ..Default::default()
        },
    ));
    ctx.set_db(Arc::new(std::sync::Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    )));
    ctx.set_harness(crate::harness::Harness::Opencode);
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(
        crate::path_identity::ProjectRootId::from_path(dir.path()).unwrap(),
        ctx.clone()
    ));
    (dir, ctx, executor, app)
}

thread_local! {
    static FACTS_SQL_TRACE: std::cell::RefCell<Vec<String>> = const {std::cell::RefCell::new(Vec::new())};
}

fn trace_facts_sql(sql: &str) {
    FACTS_SQL_TRACE.with(|trace| trace.borrow_mut().push(sql.to_owned()));
}

fn insert_launch(conn: &rusqlite::Connection, root: &str, key: &ScopeKey) {
    conn.execute("INSERT INTO access_launch_params(project_root,harness,session,principal,owner,scope_ref,epoch,preset,params)
        VALUES (?1,'test',?2,?3,?3,?2,?4,'default','{}')",rusqlite::params![root,key.scope_ref,key.owner,key.epoch.to_string()]).unwrap();
}

#[test]
fn shells_v1_database_facts_scale_with_shells_not_thirty_thousand_scope_rows() {
    use crate::bash_background::persistence::{read_task_at, write_task_at};
    let (dir, ctx, executor, app) = status_context();
    let db = app.db().unwrap();
    let root = ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .display()
        .to_string();
    {
        let conn = db.lock().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<29999)
            INSERT INTO access_scopes(owner,scope_ref,epoch,config,ended)
            SELECT 'history',printf('old-%05d',i),'1','{}',1 FROM n",
            [],
        )
        .unwrap();
        tx.execute("INSERT INTO access_scope_agents(owner,scope_ref,epoch,agent_id,kind)
            SELECT owner,scope_ref,epoch,'old-agent-'||scope_ref,'head' FROM access_scopes WHERE owner='history'",[]).unwrap();
        tx.execute("INSERT INTO access_launch_params(project_root,harness,session,principal,owner,scope_ref,epoch,preset,params)
            SELECT CASE WHEN CAST(substr(scope_ref,5) AS INTEGER)<10000 THEN ?1 ELSE '/unbound-history' END,
            'test',scope_ref,'history',owner,scope_ref,epoch,'default','{}' FROM access_scopes WHERE owner='history'",[&root]).unwrap();
        tx.commit().unwrap();
    }
    for index in 0..3 {
        let scope = stamp(&format!("live-{index}"), false);
        let key = ScopeKey::from_stamp(&scope);
        let (_, _, task) = captured(dir.path(), b"", false, false);
        let mut metadata = read_task_at(&task).unwrap();
        metadata.scope = Some(scope);
        metadata.remote=Some(serde_json::from_value(json!({"harness":"test","session":"plain-session","job_id":format!("0b816da2-78cb-4fef-8abc-{index:012x}"),"stdout_len":0,"stderr_len":0,"cancel_requested":false})).unwrap());
        let conn = db.lock().unwrap();
        crate::db::access_grants::register_scope(&conn, &key, None, &Default::default()).unwrap();
        crate::db::access_grants::register_agent(&conn, &key, &format!("agent-live-{index}"), true)
            .unwrap();
        insert_launch(&conn, &root, &key);
        if index == 0 {
            let snapshot = crate::bash_background::registry::ShellSnapshot {
                metadata: metadata.clone(),
                stdout: 0,
                stderr: 0,
                last_output_at_ms: None,
            };
            let row = project_row(&snapshot, Some(&conn));
            metadata.shell_fingerprint = Some(
                blake3::hash(&serde_json::to_vec(&row).unwrap())
                    .to_hex()
                    .to_string(),
            );
            metadata.shell_revision = 1;
        }
        drop(conn);
        write_task_at(&task, &metadata).unwrap();
        let conn = db.lock().unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &conn,
            &metadata.to_bash_task_row("test", &task.paths).unwrap(),
        )
        .unwrap();
        drop(conn);
        if index == 0 {
            ctx.bash_background()
                .insert_shell_fixture(metadata, task.paths)
                .unwrap();
        }
    }
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM access_scope_agents", [], |row| row
                .get::<_, usize>(
                0
            ))
            .unwrap(),
        30_003
    );
    assert_eq!(
        db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM access_launch_params", [], |row| row
                .get::<_, usize>(
                0
            ))
            .unwrap(),
        30_003
    );
    FACTS_SQL_TRACE.with(|trace| trace.borrow_mut().clear());
    DB_LOCK_DURATIONS.with(|durations| durations.borrow_mut().clear());
    db.lock().unwrap().trace(Some(trace_facts_sql));
    // Supply an explicit revision to isolate the cache test from other tests'
    // concurrent registry writes. Production supplies the metadata revision.
    let first = collect_at_revision(&executor, Some(&db), true, DbFacts::default(), 10);
    let sql = FACTS_SQL_TRACE.with(|trace| trace.borrow().clone());
    let durations = DB_LOCK_DURATIONS.with(|samples| samples.borrow().clone());
    assert!(first.db_available);
    assert_eq!(first.rows.len(), 3);
    assert!(first
        .rows
        .iter()
        .all(|row| row.state == "queued" && row.holder.kind == "agent"));
    assert_eq!(first.db.holders.len(), 3);
    assert_eq!(
        first
            .sources
            .values()
            .filter(|source| !source.is_persisted())
            .count(),
        1
    );
    assert_eq!(
        first.status.roots[0].agent_ids,
        ["agent-live-0", "agent-live-1", "agent-live-2"]
    );
    assert!(
        sql.len() <= 6,
        "statement count grew with scope history: {sql:?}"
    );
    assert_eq!(durations.len(), 2);
    let held: Duration = durations.iter().sum();
    // Wall time includes scheduler pauses on shared runners; the statement and
    // lock-section counts above enforce the history-independent work bound.
    assert!(
        held < Duration::from_secs(2),
        "DB lock held for {held:?}: {durations:?}"
    );
    println!(
        "30k-scope facts: {} statements, {} lock sections, total lock held {held:?}",
        sql.len(),
        durations.len()
    );
    FACTS_SQL_TRACE.with(|trace| trace.borrow_mut().clear());
    DB_LOCK_DURATIONS.with(|durations| durations.borrow_mut().clear());
    let idle = collect_at_revision(&executor, Some(&db), false, first.db, 10);
    assert!(
        FACTS_SQL_TRACE.with(|trace| trace.borrow().is_empty()),
        "idle status issued SQL"
    );
    assert!(
        DB_LOCK_DURATIONS.with(|samples| samples.borrow().is_empty()),
        "idle status acquired aft.db"
    );
    // A watchdog wakeup without a metadata mutation must not dirty the cache,
    // even when persisted output observation asks for refreshed rows.
    crate::bash_background::shell_metadata::changed();
    let polled = collect_at_revision(&executor, Some(&db), true, idle.db, 10);
    assert_eq!(polled.rows.len(), 3);
    assert!(FACTS_SQL_TRACE.with(|trace| trace.borrow().is_empty()));
    assert!(DB_LOCK_DURATIONS.with(|samples| samples.borrow().is_empty()));
    db.lock()
        .unwrap()
        .execute(
            "UPDATE access_scope_agents SET agent_id='agent-renamed' WHERE scope_ref='live-0'",
            [],
        )
        .unwrap();
    FACTS_SQL_TRACE.with(|trace| trace.borrow_mut().clear());
    let changed = collect_at_revision(&executor, Some(&db), true, polled.db, 11);
    assert!(changed.db_available);
    assert!(
        !FACTS_SQL_TRACE.with(|trace| trace.borrow().is_empty()),
        "metadata change did not refresh facts"
    );
    assert!(changed
        .rows
        .iter()
        .any(|row| row.holder.agent_id.as_deref() == Some("agent-renamed")));
    db.lock().unwrap().trace(None);
}

#[test]
fn shells_v1_root_agent_hints_are_live_bound_and_explicitly_truncated() {
    let (dir, ctx, executor, app) = status_context();
    let second_root = dir.path().join("z");
    std::fs::create_dir(&second_root).unwrap();
    let second = Arc::new(crate::context::AppContext::with_app_and_provider(
        app.clone(),
        Box::new(crate::language::StubProvider),
        crate::config::Config {
            project_root: Some(second_root.clone()),
            ..Default::default()
        },
    ));
    second.set_harness(crate::harness::Harness::Opencode);
    assert!(executor.register_actor(
        crate::path_identity::ProjectRootId::from_path(&second_root).unwrap(),
        second
    ));
    let first_root = ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .display()
        .to_string();
    let second_root = second_root.display().to_string();
    let db = app.db().unwrap();
    {
        let conn = db.lock().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        for (name, root, ended) in [
            ("first", first_root.as_str(), false),
            ("ended", first_root.as_str(), true),
            ("outside", "/unbound-live-root", false),
        ] {
            let key = ScopeKey::from_stamp(&stamp(name, false));
            crate::db::access_grants::register_scope(&tx, &key, None, &Default::default()).unwrap();
            crate::db::access_grants::register_agent(&tx, &key, &format!("agent-{name}"), true)
                .unwrap();
            insert_launch(&tx, root, &key);
            if ended {
                tx.execute(
                    "UPDATE access_scopes SET ended=1 WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
                    rusqlite::params![key.owner, key.scope_ref, key.epoch.to_string()],
                )
                .unwrap();
            }
        }
        for index in 0..ROOT_ASSOCIATION_LIMIT + 1 {
            let key = ScopeKey::from_stamp(&stamp(&format!("cap-{index:04}"), false));
            crate::db::access_grants::register_scope(&tx, &key, None, &Default::default()).unwrap();
            crate::db::access_grants::register_agent(
                &tx,
                &key,
                &format!("agent-cap-{index:04}"),
                true,
            )
            .unwrap();
            insert_launch(&tx, &second_root, &key);
        }
        let head = ScopeKey::from_stamp(&stamp("first", false));
        let worker = ScopeKey::from_stamp(&stamp("root-worker", true));
        crate::db::access_grants::register_scope(&tx, &worker, Some(&head), &Default::default())
            .unwrap();
        crate::db::access_grants::register_worker(&tx, &worker).unwrap();
        insert_launch(&tx, &first_root, &worker);
        let revived = ScopeKey {
            epoch: 2,
            ..head.clone()
        };
        crate::db::access_grants::register_scope(&tx, &revived, None, &Default::default()).unwrap();
        crate::db::access_grants::register_agent(&tx, &revived, "agent-first", true).unwrap();
        tx.execute(
            "DELETE FROM access_scope_agents WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
            rusqlite::params![head.owner, head.scope_ref, head.epoch.to_string()],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let collected = collect_at_revision(&executor, Some(&db), true, DbFacts::default(), 1);
    assert!(collected.db_available);
    assert_eq!(collected.status.roots.len(), 2);
    let first = collected
        .status
        .roots
        .iter()
        .find(|root| root.root == first_root)
        .unwrap();
    assert_eq!(first.agent_ids, ["agent-first"]);
    assert!(first.agent_ids_truncated.is_none());
    let second = collected
        .status
        .roots
        .iter()
        .find(|root| root.root == second_root)
        .unwrap();
    assert_eq!(second.agent_ids.len(), ROOT_ASSOCIATION_LIMIT - 1);
    assert_eq!(second.agent_ids_truncated, Some(true));
    assert!(second
        .agent_ids
        .iter()
        .all(|agent| agent.starts_with("agent-cap-")));
    assert_eq!(
        serde_json::to_value(second).unwrap()["agent_ids_truncated"],
        true
    );
    assert!(serde_json::to_value(first)
        .unwrap()
        .get("agent_ids_truncated")
        .is_none());
    let requested = (0..ROOT_ASSOCIATION_LIMIT + 1)
        .map(|index| format!("agent-cap-{index:04}"))
        .find(|agent| !second.agent_ids.contains(agent))
        .unwrap();
    let filtered = state::filtered_status(&collected.status, Some(&[requested]));
    assert_eq!(filtered.roots.len(), 1);
    assert_eq!(filtered.roots[0].root, second_root);
    assert_eq!(filtered.roots[0].agent_ids_truncated, Some(true));
}

#[test]
fn shells_v1_status_refresh_skips_busy_lsp_and_releases_database_before_probes() {
    use std::sync::mpsc;
    let (_dir, ctx, executor, app) = status_context();
    let db = app.db().unwrap();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_lsp, lsp_release) = mpsc::channel();
    let locked = std::thread::spawn(move || {
        let _lsp = ctx.lsp();
        held_tx.send(()).unwrap();
        let _ = lsp_release.recv();
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (phase_tx, phase_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let db_for_refresh = db.clone();
    let refresh = std::thread::spawn(move || {
        // Pause a real refresh after its SQL copy, not before it starts. A
        // writer must complete while root probing is still pending.
        AFTER_DB_FACTS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                phase_tx.send(()).unwrap();
                let _ = continue_rx.recv();
            }))
        });
        assert!(result_tx
            .send(collect(
                &executor,
                Some(&db_for_refresh),
                true,
                DbFacts::default()
            ))
            .is_ok());
    });
    phase_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (written_tx, written_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let result=db.try_lock().map_err(|_| "refresh retained aft.db".to_owned()).and_then(|conn| conn.execute_batch("CREATE TABLE IF NOT EXISTS shells_lock_probe(value INTEGER); INSERT INTO shells_lock_probe VALUES (1)").map_err(|error| error.to_string()));
        written_tx.send(result).unwrap();
    });
    let written = written_rx.recv_timeout(Duration::from_millis(250));
    continue_tx.send(()).unwrap();
    let result = result_rx.recv_timeout(Duration::from_millis(250));
    release_lsp.send(()).unwrap();
    locked.join().unwrap();
    writer.join().unwrap();
    refresh.join().unwrap();
    written
        .expect("a refresh held aft.db outside its SQL phase")
        .unwrap();
    let result = result.expect("refresh waited behind the language server mutex");
    assert_eq!(result.status.roots.len(), 1);
    assert!(
        result.db_available,
        "database facts must be copied before probing roots"
    );
    assert!(result.status.roots[0].lsp.is_none());
    assert!(result.status.roots[0].errors.is_none());
    assert!(result.status.roots[0].warnings.is_none());
    assert!(serde_json::to_value(&result.status.roots[0])
        .unwrap()
        .get("lsp")
        .is_none());
}

#[test]
fn shells_v1_status_refresh_does_not_wait_for_a_busy_database() {
    use std::sync::mpsc;
    let (_dir, _ctx, executor, app) = status_context();
    let db = app.db().unwrap();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let db_to_hold = db.clone();
    let holder = std::thread::spawn(move || {
        let _guard = db_to_hold.lock().unwrap();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (result_tx, result_rx) = mpsc::channel();
    let refresh = std::thread::spawn(move || {
        assert!(result_tx
            .send(collect(&executor, Some(&db), true, DbFacts::default()))
            .is_ok())
    });
    let result = result_rx.recv_timeout(Duration::from_millis(250));
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    refresh.join().unwrap();
    let result = result.expect("status refresh waited for the shared database mutex");
    assert!(!result.db_available);
    assert_eq!(result.status.roots.len(), 1);
}

#[tokio::test]
async fn shells_v1_status_refresh_keeps_cached_lsp_when_the_manager_is_busy() {
    use std::sync::mpsc;
    let (_dir, ctx, executor, app) = status_context();
    let service = Service::new(app, executor);
    service.refresh(false).await;
    let known = service.state.read().await.status.roots[0]
        .lsp
        .clone()
        .unwrap();
    assert!(known
        .iter()
        .any(|entry| entry.server == "rust" && entry.state == "disabled"));
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = ctx.lsp();
        held_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let refreshed = tokio::time::timeout(Duration::from_millis(250), service.refresh(false)).await;
    let cached = service.state.read().await.status.roots[0].lsp.clone();
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    service.stop();
    refreshed.expect("cached status waited for LSP");
    assert_eq!(cached, Some(known));
}

fn stamp(reference: &str, worker: bool) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":reference,"scope_epoch":1,"kind":if worker {"worker"} else {"head"},
        "owner_authorized":true
    }))
    .unwrap()
}

#[test]
fn shells_v1_holder_resolution_uses_registered_head_and_worker_parent() {
    let dir = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
    let head = stamp("head", false);
    let worker = stamp("worker", true);
    let head_key = ScopeKey::from_stamp(&head);
    let worker_key = ScopeKey::from_stamp(&worker);
    let config = crate::config::Config::default();
    crate::db::access_grants::register_scope(&conn, &head_key, None, &config).unwrap();
    crate::db::access_grants::register_agent(&conn, &head_key, "agent-alf", true).unwrap();
    crate::db::access_grants::register_scope(&conn, &worker_key, Some(&head_key), &config).unwrap();
    crate::db::access_grants::register_worker(&conn, &worker_key).unwrap();
    let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
        "bash-0123456789abcdef".into(),
        "bg_worker_cedar".into(),
        "true".into(),
        dir.path().to_owned(),
        None,
        None,
        false,
        false,
    );
    metadata.scope = Some(head);
    assert_eq!(
        holder(Some(&conn), &metadata).agent_id.as_deref(),
        Some("agent-alf")
    );
    metadata.scope = Some(worker);
    let resolved = holder(Some(&conn), &metadata);
    assert_eq!(resolved.kind, "worker");
    assert_eq!(resolved.agent_id.as_deref(), Some("agent-alf"));
    assert_eq!(resolved.scope_ref.as_deref(), Some("worker"));
    assert_eq!(resolved.execution_id.as_deref(), Some("bg_worker_cedar"));
    metadata.scope = None;
    metadata.harness = Some("opencode".into());
    let resolved = holder(Some(&conn), &metadata);
    assert_eq!(resolved.kind, "unattributed");
    assert_eq!(resolved.harness.as_deref(), Some("opencode"));
    assert!(resolved.agent_id.is_none());
    let db = std::sync::Mutex::new(conn);
    let (facts, available) = read_db_facts(
        Some(&db),
        &[head_key.clone(), worker_key.clone()].into(),
        &Default::default(),
        DbFacts::default(),
    );
    assert!(available);
    assert_eq!(
        facts.holders.get(&head_key),
        Some(&GrantHolder::Agent {
            agent_id: Some("agent-alf".into())
        })
    );
    assert_eq!(
        facts.holders.get(&worker_key),
        Some(&GrantHolder::Worker {
            agent_id: Some("agent-alf".into()),
            scope_ref: "worker".into()
        })
    );
}

#[test]
fn shells_v1_spawn_facts_persist_and_legacy_records_omit_unknowns() {
    use crate::bash_background::shell_metadata::{enter, ShellCall};
    let guard = enter(ShellCall {
        description: Some("Check API types".into()),
        session: "bg_worker_cedar".into(),
        call_id: None,
        kind: "foreground".into(),
    });
    let metadata = crate::bash_background::with_call_key(Some("call-1".into()), || {
        crate::bash_background::persistence::PersistedTask::starting(
            "bash-0123456789abcdef".into(),
            "bg_worker_cedar".into(),
            "true".into(),
            "/work".into(),
            None,
            None,
            false,
            false,
        )
    });
    let mut encoded = serde_json::to_value(&metadata).unwrap();
    drop(guard);
    let decoded: crate::bash_background::persistence::PersistedTask =
        serde_json::from_value(encoded.clone()).unwrap();
    let call = decoded.shell_call.unwrap();
    assert_eq!(call.description.as_deref(), Some("Check API types"));
    assert_eq!(call.session, "bg_worker_cedar");
    assert_eq!(call.call_id.as_deref(), Some("call-1"));
    assert!(crate::bash_background::shell_metadata::current().is_none());
    encoded.as_object_mut().unwrap().remove("shell_call");
    encoded.as_object_mut().unwrap().remove("shell_revision");
    let legacy: crate::bash_background::persistence::PersistedTask =
        serde_json::from_value(encoded).unwrap();
    assert!(legacy.shell_call.is_none());
    assert_eq!(legacy.shell_revision, 0);
    assert!(serde_json::to_value(legacy)
        .unwrap()
        .get("shell_call")
        .is_none());
}

#[test]
fn shells_v1_status_unknown_counts_are_omitted_individually() {
    let ready = Readiness {
        state: "ready".into(),
        reason: None,
    };
    let root = RootStatus {
        agent_ids_truncated: None,
        root: "/work".into(),
        agent_ids: vec![],
        text_index: ready.clone(),
        call_graph: ready.clone(),
        meaning: ready,
        lsp: Some(vec![]),
        errors: None,
        warnings: None,
        todos: Some(7),
    };
    let value = serde_json::to_value(root).unwrap();
    assert!(value.get("errors").is_none());
    assert!(value.get("warnings").is_none());
    assert_eq!(value["todos"], 7);
    assert_eq!(readiness(&json!({"status":"building"})).state, "building");
    assert_eq!(
        readiness(&json!({"status":"unavailable","reason":"index_missing"}))
            .reason
            .as_deref(),
        Some("index_missing")
    );
}

fn captured(
    dir: &std::path::Path,
    bytes: &[u8],
    pty: bool,
    ended: bool,
) -> (
    crate::bash_background::registry::ShellSnapshot,
    ShellReader,
    crate::bash_background::persistence::ResolvedTask,
) {
    use crate::bash_background::persistence::{allocate_task_layout, write_task_at, PersistedTask};
    let task = allocate_task_layout(dir, "plain-session").unwrap();
    let mut metadata = PersistedTask::starting(
        task.paths.task_id.clone(),
        "plain-session".into(),
        "printf progress".into(),
        dir.to_owned(),
        Some(dir.to_owned()),
        None,
        false,
        false,
    );
    metadata.harness = Some("test".into());
    metadata.completion_has_secret_placeholders = Some(false);
    metadata.completion_has_env_secrets = Some(false);
    metadata.completion_has_non_ticket_env_secrets = Some(false);
    if pty {
        metadata.mode = BgMode::Pty;
        metadata.pty_rows = Some(3);
        metadata.pty_cols = Some(20);
    }
    if ended {
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    } else {
        metadata.status = BgTaskStatus::Running;
    }
    write_task_at(&task, &metadata).unwrap();
    std::fs::write(&task.paths.stdout, if pty { &[] } else { bytes }).unwrap();
    std::fs::write(&task.paths.stderr, []).unwrap();
    std::fs::write(&task.paths.pty, if pty { bytes } else { &[] }).unwrap();
    let stored = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let (snapshot, reader) = ShellReader::persisted(stored).unwrap();
    (snapshot, reader, task)
}

#[test]
fn shells_v1_persisted_observation_settles_lost_without_adopting_and_keeps_revision() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, reader, task) = captured(dir.path(), b"last output\n", false, false);
    assert_eq!(snapshot.metadata.status, BgTaskStatus::FateUnknown);
    let row = project_row(&snapshot, None);
    assert_eq!(row.end_reason.as_deref(), Some("lost"));
    assert_eq!(reader.shell_revision(&row.task_id, "first").unwrap(), 1);
    let metadata = crate::bash_background::persistence::read_task_at(&task).unwrap();
    assert_eq!(metadata.status, BgTaskStatus::FateUnknown);
    assert!(metadata.finished_at.is_some());
    let stored = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let (_, reader) = ShellReader::persisted(stored).unwrap();
    assert_eq!(reader.shell_revision(&row.task_id, "first").unwrap(), 1);
    assert_eq!(reader.shell_revision(&row.task_id, "changed").unwrap(), 2);
}

#[test]
fn shells_v1_remote_without_live_registry_keeps_durable_pending_state() {
    use crate::bash_background::persistence::{read_task_at, write_task_at};
    let dir = tempfile::tempdir().unwrap();
    let (_, _, task) = captured(dir.path(), b"", false, false);
    let mut metadata = read_task_at(&task).unwrap();
    let job = "0b816da2-78cb-4fef-8abc-0123456789ab";
    metadata.remote=Some(serde_json::from_value(json!({"harness":"test","session":"plain-session","job_id":job,"stdout_len":0,"stderr_len":0,"cancel_requested":false})).unwrap());
    write_task_at(&task, &metadata).unwrap();
    let stored = metadata.to_bash_task_row("test", &task.paths).unwrap();
    let (snapshot, reader) = ShellReader::persisted(stored).unwrap();
    assert!(!snapshot.metadata.is_terminal());
    let row = project_row(&snapshot, None);
    assert_eq!(row.state, "queued");
    assert_eq!(row.remote.unwrap().job_id, job);
    assert!(row.end_reason.is_none());
    reader.shell_revision(&row.task_id, "pending").unwrap();
    let unchanged = read_task_at(&task).unwrap();
    assert_eq!(unchanged.status, BgTaskStatus::Running);
    assert!(unchanged.finished_at.is_none());
}

#[test]
fn shells_v1_remote_row_emits_only_the_persisted_optional_placement_host() {
    let dir = tempfile::tempdir().unwrap();
    let (mut snapshot, _, _task) = captured(dir.path(), b"", false, false);
    snapshot.metadata.remote=Some(serde_json::from_value(json!({"harness":"test","session":"plain-session","job_id":"0b816da2-78cb-4fef-8abc-0123456789ab","stdout_len":0,"stderr_len":0,"cancel_requested":false})).unwrap());
    for host in [Some("ovh-1"), None] {
        snapshot.metadata.shell_remote_host = host.map(str::to_owned);
        let row = serde_json::to_value(project_row(&snapshot, None)).unwrap();
        assert_eq!(row["remote"].get("host").and_then(Value::as_str), host);
        assert_eq!(row["remote"].get("host").is_some(), host.is_some());
        assert!(row["remote"].get("provider").is_none());
    }
}

#[tokio::test]
async fn shells_v1_output_real_files_default_backfill_exact_resume_and_end_record() {
    let dir = tempfile::tempdir().unwrap();
    let raw = "o".repeat(70_000);
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let row = project_row(&snapshot, None);
    let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
    service
        .state
        .write()
        .await
        .rows
        .insert(row.task_id.clone(), row.clone());
    service
        .sources
        .write()
        .await
        .insert(row.task_id.clone(), reader);
    let metrics = DispatchPathMetrics::new();
    let (tx, mut output) = tokio::sync::mpsc::channel(16);
    let frame = Frame::build(
        FrameType::Request,
        subc_protocol::Flags(0),
        41,
        1,
        7,
        b"{}".to_vec(),
    )
    .unwrap();
    let began = tokio::time::Instant::now();
    assert!(service
        .output(
            &tx,
            &metrics,
            &frame,
            OutputParams {
                task_id: row.task_id.clone(),
                ..Default::default()
            },
            &CancellationToken::new(),
            &CancellationToken::new(),
            &output_log(&frame),
        )
        .await
        .unwrap()
        .is_some());
    assert!(
        began.elapsed() >= Duration::from_secs(1),
        "four backfill chunks must use separate 250 ms ticks"
    );
    let mut text = String::new();
    let mut end = raw.len() as u64 - BACKFILL_BYTES;
    let mut chunks = 0;
    let first = output.try_recv().unwrap();
    let first: OutputFrame = serde_json::from_slice(first.body()).unwrap();
    assert!(
        matches!(first,OutputFrame::Start{truncated_before:Some(Offsets{stdout,stderr:0}),..} if stdout==end)
    );
    let mut ended = false;
    while let Ok(frame) = output.try_recv() {
        assert_eq!((frame.header.channel, frame.header.corr), (41, 7));
        match serde_json::from_slice::<OutputFrame>(frame.body()).unwrap() {
            OutputFrame::Chunk {
                stream,
                offset,
                end_offset,
                text: part,
            } => {
                assert_eq!(stream, "stdout");
                assert_eq!(offset, end);
                end = end_offset;
                text.push_str(&part);
                chunks += 1;
            }
            OutputFrame::End { row: ended_row } => {
                assert_eq!(ended_row.end_reason.as_deref(), Some("exited"));
                ended = true;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(chunks, 4);
    assert!(ended);
    assert_eq!(end, raw.len() as u64);
    assert_eq!(text, &raw[raw.len() - BACKFILL_BYTES as usize..]);
    let (tx, mut output) = tokio::sync::mpsc::channel(16);
    service
        .output(
            &tx,
            &metrics,
            &frame,
            OutputParams {
                task_id: row.task_id,
                from_offset: Some(Offsets {
                    stdout: end - 2,
                    stderr: 0,
                }),
                ..Default::default()
            },
            &CancellationToken::new(),
            &CancellationToken::new(),
            &output_log(&frame),
        )
        .await
        .unwrap();
    let mut resumed = String::new();
    while let Ok(frame) = output.try_recv() {
        if let OutputFrame::Chunk {
            offset,
            end_offset,
            text,
            ..
        } = serde_json::from_slice(frame.body()).unwrap()
        {
            assert_eq!((offset, end_offset), (end - 2, end));
            resumed.push_str(&text);
        }
    }
    assert_eq!(resumed, "oo");
    service.stop();
}

#[test]
fn shells_v1_real_capture_harmless_long_line_is_not_replaced_by_a_preview_marker() {
    let dir = tempfile::tempdir().unwrap();
    let raw = "o".repeat(150_000);
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let text = reader.shell_masked_range(
        &snapshot.metadata.task_id,
        TaskArtifact::Stdout,
        70_000,
        85_000,
    );
    assert_eq!(text, "o".repeat(15_000));
}

#[test]
fn shells_v1_real_capture_private_key_opening_outside_window_is_withheld() {
    let dir = tempfile::tempdir().unwrap();
    for body in ["A".repeat(64), "private prose that is not base64".into()] {
        let raw = format!(
            "-----BEGIN PRIVATE KEY-----\n{}-----END PRIVATE KEY-----\n",
            format!("{body}\n").repeat(10_000)
        );
        let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
        let text = reader.shell_masked_range(
            &snapshot.metadata.task_id,
            TaskArtifact::Stdout,
            120_000,
            121_000,
        );
        assert!(text.contains("withheld"), "{text}");
        assert!(!text.contains(&body));
    }
}

#[test]
fn shells_v1_real_capture_unbounded_authorization_value_is_withheld_not_leaked() {
    let dir = tempfile::tempdir().unwrap();
    let raw = format!(
        "{}\nAuthorization: Bearer {}\n",
        "o".repeat(DEFAULT_CHUNK_BYTES - 7),
        "r".repeat(150_000)
    );
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let text = reader.shell_masked_range(
        &snapshot.metadata.task_id,
        TaskArtifact::Stdout,
        70_000,
        85_000,
    );
    assert!(text.contains("withheld"), "{text}");
    assert!(!text.contains(&"r".repeat(32)));
}

#[test]
fn shells_v1_real_capture_json_opening_outside_window_is_withheld() {
    let dir = tempfile::tempdir().unwrap();
    let raw = format!(
        "\"password\":\"{}\"\n",
        "private secret body\n".repeat(10_000)
    );
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let text = reader.shell_masked_range(
        &snapshot.metadata.task_id,
        TaskArtifact::Stdout,
        120_000,
        121_000,
    );
    assert!(text.contains("withheld"), "{text}");
    assert!(!text.contains("private secret body"));
}

#[test]
fn shells_v1_real_capture_complete_json_lines_remain_readable() {
    let dir = tempfile::tempdir().unwrap();
    let raw = "{\"status\":\"ready\"}\n".repeat(10_000);
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let text = reader.shell_masked_range(
        &snapshot.metadata.task_id,
        TaskArtifact::Stdout,
        120_000,
        121_000,
    );
    assert_eq!(text, &raw[120_000..121_000]);
}

#[test]
fn shells_v1_real_capture_long_preserved_header_prefix_respects_chunk_limit() {
    let dir = tempfile::tempdir().unwrap();
    let spaces = " ".repeat(200);
    let raw = format!("Authorization:{spaces}Bearer {}\nafter", "q".repeat(100));
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), false, true);
    let row = project_row(&snapshot, None);
    let (mut capture, _) = super::super::capture::Capture::open(
        reader,
        &row,
        OutputParams {
            task_id: row.task_id.clone(),
            max_chunk_bytes: Some(64),
            ..Default::default()
        },
    )
    .unwrap();
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let (next, frames, finished) = capture.read(row.clone());
        capture = next;
        for frame in frames {
            if let OutputFrame::Chunk {
                offset: from,
                end_offset,
                text: part,
                ..
            } = frame
            {
                assert_eq!(from, offset);
                offset = end_offset;
                assert!(part.len() <= 64);
                text.push_str(&part);
            }
        }
        if finished {
            break;
        }
    }
    assert_eq!(offset, raw.len() as u64);
    assert_eq!(
        text,
        format!(
            "Authorization:{spaces}Bearer {}\nafter",
            crate::log_redact::SECRET_PLACEHOLDER
        )
    );
}

#[cfg(unix)]
#[test]
fn shells_v1_real_capture_long_known_secret_crosses_chunks_without_leaking() {
    let dir = tempfile::tempdir().unwrap();
    let registry = crate::bash_background::BgTaskRegistry::default();
    let secret = "q".repeat(2048);
    let prefix = "x".repeat(61);
    let suffix = " suffix";
    let command = format!("printf '%s' '{prefix}' \"$SHELLS_V1_TOKEN\" '{suffix}'");
    let session = "shells-secret-test";
    let task_id = registry
        .spawn(
            crate::sandbox_spawn::SpawnPlan::Unsandboxed,
            &command,
            session.into(),
            dir.path().to_owned(),
            HashMap::from([("SHELLS_V1_TOKEN".into(), secret.clone())]),
            crate::bash_background::HardKill::After(Duration::from_secs(10)),
            None,
            dir.path().to_owned(),
            10,
            true,
            false,
            Some(dir.path().to_owned()),
        )
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !registry.wait_target_finished(&task_id, session) {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture command did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let snapshot = registry
        .shell_snapshots()
        .into_iter()
        .find(|snapshot| snapshot.metadata.task_id == task_id)
        .unwrap();
    let row = project_row(&snapshot, None);
    let reader = registry.shell_reader(&task_id).unwrap();
    assert!(reader.shell_longest_secret(&task_id) >= secret.len());
    let (mut capture, _) = super::super::capture::Capture::open(
        reader,
        &row,
        OutputParams {
            task_id,
            max_chunk_bytes: Some(64),
            ..Default::default()
        },
    )
    .unwrap();
    let mut text = String::new();
    let mut offset = 0;
    let mut count = 0;
    loop {
        let (next, frames, finished) = capture.read(row.clone());
        capture = next;
        for frame in frames {
            if let OutputFrame::Chunk {
                stream,
                offset: from,
                end_offset,
                text: part,
            } = frame
            {
                assert_eq!(stream, "stdout");
                assert_eq!(from, offset);
                offset = end_offset;
                assert!(part.len() <= 64);
                assert!(!part.contains(&"q".repeat(32)));
                text.push_str(&part);
                count += 1;
            }
        }
        if finished {
            break;
        }
        assert!(count < 100, "capture made no terminal progress");
    }
    assert!(count > 2);
    assert_eq!(offset, (prefix.len() + secret.len() + suffix.len()) as u64);
    assert_eq!(
        text,
        format!("{prefix}{}{suffix}", crate::log_redact::SECRET_PLACEHOLDER)
    );
}

#[tokio::test]
async fn shells_v1_real_capture_missing_secret_list_marks_and_advances_the_chunk() {
    use crate::bash_background::persistence::{read_task_at, write_task_at};
    let dir = tempfile::tempdir().unwrap();
    let (_, _, task) = captured(dir.path(), b"opaque original secret", false, true);
    let mut metadata = read_task_at(&task).unwrap();
    metadata.completion_has_env_secrets = Some(true);
    metadata.completion_has_non_ticket_env_secrets = Some(true);
    write_task_at(&task, &metadata).unwrap();
    let (snapshot, reader) =
        ShellReader::persisted(metadata.to_bash_task_row("test", &task.paths).unwrap()).unwrap();
    let row = project_row(&snapshot, None);
    let (capture, _) = super::super::capture::Capture::open(
        reader,
        &row,
        OutputParams {
            task_id: row.task_id.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    let (_, frames, finished) = capture.read(row.clone());
    assert!(finished);
    let OutputFrame::Chunk {
        offset,
        end_offset,
        text,
        ..
    } = &frames[0]
    else {
        panic!("{frames:?}")
    };
    assert_eq!((*offset, *end_offset), (0, row.output_bytes.stdout));
    assert!(text.contains("withheld"));
    assert!(!text.contains("opaque original secret"));
    assert!(matches!(frames.last(), Some(OutputFrame::End { .. })));
}

#[tokio::test]
async fn shells_v1_output_pty_screen_uses_masked_renderer_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let raw = format!(
        "{}\x1b[2J\x1b[Hhello\rworld\n",
        "o\n".repeat(DEFAULT_CHUNK_BYTES - 20)
    );
    let (snapshot, reader, _task) = captured(dir.path(), raw.as_bytes(), true, true);
    let row = project_row(&snapshot, None);
    let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
    service
        .state
        .write()
        .await
        .rows
        .insert(row.task_id.clone(), row.clone());
    service
        .sources
        .write()
        .await
        .insert(row.task_id.clone(), reader);
    let (tx, mut output) = tokio::sync::mpsc::channel(16);
    let metrics = DispatchPathMetrics::new();
    let frame = Frame::build(
        FrameType::Request,
        subc_protocol::Flags(0),
        41,
        1,
        7,
        b"{}".to_vec(),
    )
    .unwrap();
    service
        .output(
            &tx,
            &metrics,
            &frame,
            OutputParams {
                task_id: row.task_id,
                ..Default::default()
            },
            &CancellationToken::new(),
            &CancellationToken::new(),
            &output_log(&frame),
        )
        .await
        .unwrap();
    let mut screen = None;
    let mut screens = 0;
    while let Ok(frame) = output.try_recv() {
        if let OutputFrame::Screen {
            rows,
            cols,
            lines,
            cursor,
        } = serde_json::from_slice(frame.body()).unwrap()
        {
            screens += 1;
            screen = Some((rows, cols, lines, cursor));
        }
    }
    let (rows, cols, lines, cursor) = screen.unwrap();
    assert!(
        screens >= 2,
        "the final screen must not be lost to coalescing"
    );
    assert_eq!((rows, cols), (3, 20));
    assert_eq!(lines, vec!["world".to_owned()]);
    assert_eq!((cursor.row, cursor.col), (1, 5));
    service.stop();
}

#[tokio::test(flavor = "current_thread")]
async fn shells_list_agent_db_contention_does_not_block_runtime_polling() {
    let (_dir, ctx, executor, app) = status_context();
    let db = ctx.db().unwrap();
    {
        let conn = db.lock().unwrap();
        let key = ScopeKey::from_stamp(&stamp("head", false));
        crate::db::access_grants::register_scope(&conn, &key, None, &Default::default()).unwrap();
        crate::db::access_grants::register_agent(&conn, &key, "known-agent", true).unwrap();
    }
    let service = Service::new(app, executor);
    let (release, released) = std::sync::mpsc::channel();
    let (locked, acquired) = tokio::sync::oneshot::channel();
    let holder_db = Arc::clone(&db);
    let holder = std::thread::spawn(move || {
        let _guard = holder_db.lock().unwrap();
        locked.send(()).unwrap();
        // The external deadline still runs if the current-thread runtime blocks.
        released.recv_timeout(Duration::from_secs(2)).is_ok()
    });
    acquired.await.unwrap();
    let list_service = Arc::clone(&service);
    let mut list = tokio::spawn(async move {
        list_service
            .list(ListParams {
                agent_ids: Some(vec!["unknown-agent".into(), "known-agent".into()]),
                ..Default::default()
            })
            .await
    });
    let start = std::time::Instant::now();
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        result = &mut list => panic!("list finished while db was held: {result:?}"),
    }
    let _ = release.send(());
    assert!(
        holder.join().unwrap(),
        "runtime did not release the lock holder in time"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "runtime stopped polling while waiting for the database"
    );
    let reply = list.await.unwrap().unwrap();
    assert!(reply.rows.is_empty());
    assert!(reply.next_cursor.is_none());
    assert_eq!(reply.unknown_agent_ids, Some(vec!["unknown-agent".into()]));
    service.stop();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shells_v1_list_pages_fifty_running_first_and_cursor_is_filter_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, _reader, _task) = captured(dir.path(), b"", false, true);
    let base = project_row(&snapshot, None);
    let service = Service::new(App::default_shared(), Arc::new(Executor::new()));
    for index in 0..51 {
        let mut row = base.clone();
        row.task_id = format!("bash-{index:016x}");
        row.started_at_ms = index;
        if index == 0 {
            row.state = "running".into();
            row.ended_at_ms = None;
        }
        service
            .state
            .write()
            .await
            .rows
            .insert(row.task_id.clone(), row);
    }
    let first = service.list(ListParams::default()).await.unwrap();
    assert_eq!(first.rows.len(), 50);
    assert_eq!(first.rows[0].task_id, "bash-0000000000000000");
    assert_eq!(first.rows[1].started_at_ms, 50);
    let cursor = first.next_cursor.unwrap();
    let second = service
        .list(ListParams {
            cursor: Some(cursor.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(second.rows.len(), 1);
    assert!(second.next_cursor.is_none());
    assert!(first
        .rows
        .iter()
        .all(|row| row.task_id != second.rows[0].task_id));
    assert_eq!(
        service
            .list(ListParams {
                cursor: Some(cursor),
                states: Some(vec!["running".into()]),
                ..Default::default()
            })
            .await
            .err()
            .unwrap()
            .code,
        "invalid_params"
    );
    service.stop();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shells_v1_real_route_refusals_unknown_agent_watch_cancel_and_drain() {
    use super::super::super::{
        control_flags, read_frame, run_module_loop, write_frame, ModuleControlCommand,
        ModuleControlRequest, QuiescedLsp, RouteTarget, HELLO_CORR,
    };

    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("shells route response timed out")
    }
    async fn write(
        daemon: &mut tokio::io::DuplexStream,
        ty: FrameType,
        channel: u16,
        corr: u64,
        body: Value,
    ) {
        let frame = Frame::build(
            ty,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            if ty == FrameType::Cancel {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        )
        .unwrap();
        write_frame(daemon, &frame).await.unwrap();
    }
    async fn request(
        daemon: &mut tokio::io::DuplexStream,
        channel: u16,
        corr: u64,
        body: Value,
    ) -> Frame {
        write(daemon, FrameType::Request, channel, corr, body).await;
        receive(daemon, channel, corr).await
    }
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("module-storage");
    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let module_app = app.clone();
    let module_executor = executor.clone();
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let module_storage = storage.clone();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (read, write) = tokio::io::split(module);
        runtime.block_on(run_module_loop(
            read,
            write,
            &module_storage.join("absent-connection.json"),
            module_app,
            module_executor,
            |_, _| panic!("shells routes must never execute an agent command"),
            None,
            false,
            usize::MAX,
            None,
            &module_storage,
            QuiescedLsp::ReapInBackground,
            None,
            None,
            None,
        ))
    });
    assert_eq!(
        receive(&mut daemon, 0, HELLO_CORR).await.header.ty,
        FrameType::Hello
    );
    write(&mut daemon, FrameType::HelloAck, 0, HELLO_CORR, json!({})).await;
    std::fs::create_dir_all(&storage).unwrap();
    app.open_db(&storage.join("aft.db")).unwrap();
    let bind = |channel: u16, harness: &str, principal: subc_protocol::Principal| {
        serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new(harness, format!("shells:{channel}")),
            principal: Some(principal),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        })
        .unwrap()
    };
    let callosum = || subc_protocol::Principal::Reserved {
        module_id: "callosum".into(),
    };
    for (channel, harness, admitted) in [
        (41, "fed-phone:person", true),
        (42, "fed-sim:person", true),
        (43, "fed-computer:person", false),
        (44, "fed-unknown:person", false),
        (45, "callosum-self", false),
    ] {
        let ack = request(
            &mut daemon,
            0,
            channel as u64,
            bind(channel, harness, callosum()),
        )
        .await;
        assert_eq!(
            ack.header.ty,
            if admitted {
                FrameType::Response
            } else {
                FrameType::Error
            },
            "{harness}: {ack:?}"
        );
        if !admitted {
            assert_eq!(
                serde_json::from_slice::<Value>(&ack.body).unwrap()["code"],
                "shells_operator_required"
            );
        }
    }
    let bound = request(
        &mut daemon,
        0,
        46,
        bind(
            46,
            "prefrontal-core",
            subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
        ),
    )
    .await;
    assert_eq!(bound.header.ty, FrameType::Response);
    let refused = request(
        &mut daemon,
        46,
        1,
        json!({"method":"shells.list","params":{}}),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.body).unwrap()["code"],
        "shells_operator_required"
    );
    let bound = request(
        &mut daemon,
        0,
        47,
        bind(47, "operator", subc_protocol::Principal::Direct),
    )
    .await;
    assert_eq!(bound.header.ty, FrameType::Response);
    let status = request(
        &mut daemon,
        47,
        1,
        json!({"method":"aft.status","params":{}}),
    )
    .await;
    assert_eq!(status.header.ty, FrameType::Response);
    assert_eq!(
        serde_json::from_slice::<Value>(&status.body).unwrap()["result"]["aft"]["version"],
        env!("CARGO_PKG_VERSION")
    );
    let invalid=request(&mut daemon,47,2,json!({"method":"shells.output","params":{"task_id":"bash-0123456789abcdef","max_chunk_bytes":63}})).await;
    assert_eq!(invalid.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&invalid.body).unwrap()["code"],
        "invalid_params"
    );
    let list = request(
        &mut daemon,
        41,
        1,
        json!({"method":"shells.list","params":{"agent_ids":["agent-never-seen"]}}),
    )
    .await;
    assert_eq!(list.header.ty, FrameType::Response);
    let value: Value = serde_json::from_slice(&list.body).unwrap();
    assert_eq!(value["result"]["rows"], json!([]));
    assert_eq!(
        value["result"]["unknown_agent_ids"],
        json!(["agent-never-seen"])
    );
    let refused = request(
        &mut daemon,
        41,
        2,
        json!({"method":"permissions.list","params":{}}),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.body).unwrap()["code"],
        "route_not_admitted"
    );
    let initial = request(
        &mut daemon,
        41,
        3,
        json!({"method":"shells.watch","params":{"status":true}}),
    )
    .await;
    assert_eq!(initial.header.ty, FrameType::StreamData);
    assert_eq!(
        serde_json::from_slice::<Value>(&initial.body).unwrap()["type"],
        "resync"
    );
    let duplicate = request(
        &mut daemon,
        41,
        3,
        json!({"method":"shells.watch","params":{}}),
    )
    .await;
    assert_eq!(duplicate.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&duplicate.body).unwrap()["code"],
        "invalid_params"
    );
    write(&mut daemon, FrameType::Cancel, 41, 3, json!({})).await;
    assert_eq!(
        receive(&mut daemon, 41, 3).await.header.ty,
        FrameType::StreamEnd
    );
    let initial = request(
        &mut daemon,
        42,
        4,
        json!({"method":"shells.watch","params":{}}),
    )
    .await;
    assert_eq!(initial.header.ty, FrameType::StreamData);
    write(
        &mut daemon,
        FrameType::Push,
        0,
        99,
        serde_json::to_value(ModuleControlCommand::Draining {
            reason: subc_protocol::RouteCloseReason::Reload,
            deadline_ms: 1000,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(
        receive(&mut daemon, 42, 4).await.header.ty,
        FrameType::StreamEnd
    );
    drop(daemon);
    thread.join().unwrap().unwrap();
}
