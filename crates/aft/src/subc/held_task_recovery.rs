//! Restore commands that still owe a call-ledger result or a runner completion,
//! even if their session never binds a route after the provider restarts.

use super::*;

#[derive(Debug, PartialEq, Eq)]
struct Session {
    harness: String,
    session_id: String,
    project_root: PathBuf,
    task_ids: Vec<String>,
}

fn sessions(db: &crate::db::TrackedConnection) -> rusqlite::Result<Vec<Session>> {
    let mut stmt = db.prepare(
        "SELECT t.harness,t.session_id,
                COALESCE(json_extract(t.metadata,'$.project_root'),t.cwd),
                t.task_id,t.metadata,
                EXISTS(SELECT 1 FROM call_ledger c
                       WHERE c.task_id=t.task_id AND c.recovered=1 AND c.state='DispatchStarted'
                         AND json_extract(t.metadata,'$.call_key.requester')=c.carrier
                         AND json_extract(t.metadata,'$.call_key.key')=c.call_key) AS keyed
         FROM bash_tasks t
         WHERE keyed OR t.status NOT IN ('completed','failed','killed','timed_out','fate_unknown')
                     OR t.completion_delivered=0
         ORDER BY 1,2,3,t.task_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, bool>(5)?,
        ))
    })?;
    let mut pending = std::collections::BTreeMap::<_, Vec<String>>::new();
    for row in rows {
        let (harness, session, root, task, metadata, keyed) = row?;
        let metadata: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        let principal = metadata["launching_identity"]["principal"].as_str();
        let runner = matches!(
            principal,
            Some("reserved:thalamus-harness" | "reserved:broca")
        ) || [
            Some(harness.as_str()),
            metadata["launching_raw_harness"].as_str(),
            metadata["launching_identity"]["harness"].as_str(),
        ]
        .into_iter()
        .flatten()
        .any(|raw| raw.parse::<crate::harness::Harness>() == Ok(crate::harness::Harness::Runner));
        if keyed || runner {
            pending
                .entry((harness, session, root))
                .or_default()
                .push(task);
        }
    }
    Ok(pending
        .into_iter()
        .map(|((harness, session_id, root), task_ids)| Session {
            harness,
            session_id,
            project_root: root.into(),
            task_ids,
        })
        .collect())
}

/// The callback runs once, after adoption, even when there were no saved tasks.
pub(super) fn start(
    app: Arc<App>,
    executor: Arc<Executor>,
    push_senders: PushSenders,
    storage: PathBuf,
    on_replayed: impl FnOnce(&[(String, Vec<String>)]) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    // One blocking worker, not one worker per call. Database opening, ledger
    // recovery and bounded registry replay scans all stay off the route loop.
    tokio::task::spawn_blocking(move || {
        let replayed = recover(&app, &executor, &push_senders, &storage).unwrap_or_else(|error| {
            log::warn!("held task startup recovery failed: {error}");
            Vec::new()
        });
        on_replayed(&replayed);
    })
}

fn recover(
    app: &Arc<App>,
    executor: &Arc<Executor>,
    push_senders: &PushSenders,
    storage: &Path,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let db = app
        .open_db(&storage.join("aft.db"))
        .map_err(|error| error.to_string())?;
    crate::db::call_ledger::recover_pool_once(&db).map_err(|error| error.to_string())?;
    let pending = {
        let conn = db.lock().map_err(|error| error.to_string())?;
        sessions(&conn).map_err(|error| error.to_string())?
    };
    let mut replayed = std::collections::BTreeMap::<String, Vec<String>>::new();
    for session in pending {
        match replay(app, executor, push_senders, storage, &db, &session) {
            Ok(tasks) => replayed
                .entry(session.session_id)
                .or_default()
                .extend(tasks),
            Err(error) => {
                log::warn!(
                    "held task startup replay failed for {}/{} in {}: {error}",
                    session.harness,
                    session.session_id,
                    session.project_root.display()
                );
            }
        }
    }
    Ok(replayed.into_iter().collect())
}

fn replay(
    app: &Arc<App>,
    executor: &Arc<Executor>,
    push_senders: &PushSenders,
    storage: &Path,
    db: &Arc<StdMutex<crate::db::TrackedConnection>>,
    session: &Session,
) -> Result<Vec<String>, String> {
    let root =
        ProjectRootId::from_path(&session.project_root).map_err(|error| error.to_string())?;
    let candidate = Arc::new(AppContext::from_app(
        app.clone(),
        Config {
            storage_dir: Some(storage.to_path_buf()),
            project_root: Some(root.as_path().to_path_buf()),
            ..Config::default()
        },
    ));
    install_bash_compressor(&candidate);
    candidate.install_fleet_status_client(Some(push_senders.fleet_status_client.clone()));
    candidate.set_progress_sender(Some(push::progress_sender_for_root(
        push_senders.clone(),
        root.clone(),
    )));
    // Registration keeps an actor installed by a racing bind. Always replay
    // through the winning registry so later configure sees the same tasks.
    executor.register_actor(root.clone(), candidate);
    let ctx = executor
        .actor_context(&root)
        .ok_or_else(|| "project actor retired during startup recovery".to_string())?;
    let registry = ctx.bash_background();
    registry.set_db_pool(db.clone());
    registry.set_worker_wait_window(Duration::from_millis(
        crate::commands::bash_orchestrate::worker_wait_max_ms(&ctx),
    ));
    // Configure also reserves this gate. Wait outside lifecycle admission and
    // outside the route loop; a concurrent bind must not skip either replay.
    loop {
        if let Some(batch) = registry.claim_project_replay_batch() {
            let harness_dir = storage.join(&session.harness);
            batch.replay_saved_session_for_project(
                &harness_dir,
                &session.harness,
                &session.session_id,
                root.as_path(),
            )?;
            let mut adopted = Vec::new();
            for task in &session.task_ids {
                // A finished command may have delivered its completion notice
                // while its call-ledger result still needs settlement. Ordinary
                // project replay omits commands whose notices were delivered.
                registry.recover_watch_task(
                    &harness_dir,
                    &session.session_id,
                    task,
                    Some(root.as_path()),
                    Some(&session.harness),
                )?;
                if registry.task_registration_exists(task) {
                    adopted.push(task.clone());
                }
            }
            return Ok(adopted);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bash_background::persistence::{read_task_at, write_task_at, ResolvedTask};
    use crate::db::call_ledger as ledger;
    use serde_json::json;

    pub(crate) fn context(root: &Path, storage: &Path) -> Arc<AppContext> {
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.into()),
                storage_dir: Some(storage.into()),
                harness: Some(crate::harness::Harness::Runner),
                experimental_bash_background: true,
                ..Default::default()
            },
        ));
        ctx.update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
        let db = Arc::new(StdMutex::new(
            crate::db::open(&storage.join("aft.db")).unwrap(),
        ));
        ctx.set_db(db.clone());
        ctx.set_harness(crate::harness::Harness::Runner);
        ctx.bash_background().set_db_pool(db);
        ctx
    }

    pub(crate) fn hold_task(ctx: &AppContext, layout: &ResolvedTask, key: &str) -> ledger::Key {
        let key = ledger::Key {
            carrier: "reserved:broca".into(),
            call_key: key.into(),
        };
        let mut metadata = read_task_at(layout).unwrap();
        metadata.call_key = Some(crate::bash_background::TaskCallKey {
            requester: key.carrier.clone(),
            key: key.call_key.clone(),
            minted: false,
        });
        metadata.harness = Some("runner".into());
        write_task_at(layout, &metadata).unwrap();
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &conn,
            &metadata.to_bash_task_row("runner", &layout.paths).unwrap(),
        )
        .unwrap();
        ledger::admit(&conn, &key, "digest", None, ledger::State::DispatchStarted).unwrap();
        ledger::link_task(&conn, &key, &metadata.task_id).unwrap();
        ledger::hold_running(&conn, &key, &metadata.task_id,
            br#"{"content":[{"type":"text","text":""}],"held":{"reason":"running","yielded":true}}"#, true).unwrap();
        key
    }

    fn senders() -> PushSenders {
        PushSenders {
            lossy_tx: mpsc::channel(16).0,
            reliable_tx: mpsc::unbounded_channel().0,
            lossy_overflow: Arc::new(push::LossyOverflow::default()),
            lossy_seq: Arc::new(AtomicU64::new(0)),
            fleet_status_client: FleetStatusClient::channel(1).0,
        }
    }

    pub(crate) async fn startup(ctx: &AppContext) -> Arc<Executor> {
        let executor = Arc::new(Executor::new());
        start(
            ctx.app(),
            executor.clone(),
            senders(),
            ctx.storage_dir(),
            |_| {},
        )
        .await
        .unwrap();
        executor
    }

    pub(crate) async fn settled(ctx: &AppContext, key: &ledger::Key) -> ledger::RecordedFrame {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let row = ledger::get(&ctx.db().unwrap().lock().unwrap(), key)
                .unwrap()
                .unwrap();
            if row.state == ledger::State::Settled {
                return row.frame.unwrap();
            }
            assert!(
                Instant::now() < until,
                "startup replay did not settle {}",
                key.call_key
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn saved_local(ctx: &AppContext, root: &Path) -> (String, ResolvedTask, ledger::Key) {
        let release = root.join("release");
        let command = format!(
            "while [ ! -f '{}' ]; do sleep 0.02; done; echo held-restart-output; exit 7",
            release.display()
        );
        let request = serde_json::from_value(json!({"id":"launch", "command":"bash", "session_id":"held-worker", "params":{"command":command,"background":true,"timeout":20000}})).unwrap();
        let launched = crate::commands::bash::handle(&request, ctx);
        assert!(launched.success, "{launched:?}");
        let task_id = launched.data["task_id"].as_str().unwrap().to_owned();
        ctx.bash_background().detach();
        let layout = crate::bash_background::persistence::resolve_task_layout(
            &crate::bash_background::persistence::session_tasks_dir(
                &ctx.harness_dir(),
                "held-worker",
            ),
            &task_id,
        )
        .unwrap();
        let key = hold_task(ctx, &layout, "local-held");
        (task_id, layout, key)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn held_local_call_settles_after_restart_without_a_route_bind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&root).unwrap();
        let old = context(&root, &storage);
        let (task_id, _layout, key) = saved_local(&old, &root);
        drop(old);
        let restarted = context(&root, &storage);
        let executor = startup(&restarted).await;
        let actor = executor
            .actor_context(&ProjectRootId::from_path(&root).unwrap())
            .unwrap();
        assert!(actor.bash_background().task_registration_exists(&task_id));
        std::fs::write(root.join("release"), "").unwrap();
        let result: Value = serde_json::from_slice(&settled(&restarted, &key).await.body).unwrap();
        assert!(
            result["content"]
                .to_string()
                .contains("held-restart-output"),
            "{result}"
        );
        assert_eq!(result["structuredContent"]["exit_code"], 7, "{result}");
        actor.bash_background().detach();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn held_missing_local_process_settles_as_existing_unknown_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let old = context(dir.path(), &dir.path().join("storage"));
        let layout = crate::bash_background::persistence::create_task_layout(
            &old.harness_dir(),
            "held-worker",
            "bash-0000000000000001",
        )
        .unwrap();
        let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
            layout.paths.task_id.clone(),
            "held-worker".into(),
            "lost command".into(),
            dir.path().into(),
            Some(dir.path().into()),
            None,
            true,
            false,
        );
        metadata.status = crate::bash_background::BgTaskStatus::Running;
        metadata.child_pid = Some(i32::MAX as u32);
        write_task_at(&layout, &metadata).unwrap();
        let key = hold_task(&old, &layout, "missing-held");
        drop(old);
        let restarted = context(dir.path(), &dir.path().join("storage"));
        let executor = startup(&restarted).await;
        assert_eq!(
            settled(&restarted, &key).await.body,
            ledger::unknown_held_result().body
        );
        for actor in executor.actor_contexts() {
            actor.bash_background().detach();
        }
    }

    #[tokio::test]
    async fn runner_undelivered_completion_is_replayed_before_startup_callback_without_bind() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let old = context(dir.path(), &storage);
        let task_id = "bash-0000000000000002";
        let layout = crate::bash_background::persistence::create_task_layout(
            &old.harness_dir(),
            "runner-worker",
            task_id,
        )
        .unwrap();
        let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
            task_id.into(),
            "runner-worker".into(),
            "echo finished".into(),
            dir.path().into(),
            Some(dir.path().into()),
            None,
            true,
            false,
        );
        metadata.harness = Some("runner".into());
        metadata.launching_raw_harness = Some("claude-code".into());
        metadata.mark_terminal(
            crate::bash_background::BgTaskStatus::Completed,
            Some(0),
            None,
        );
        write_task_at(&layout, &metadata).unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &old.db().unwrap().lock().unwrap(),
            &metadata.to_bash_task_row("runner", &layout.paths).unwrap(),
        )
        .unwrap();
        drop(old);
        let restarted = context(dir.path(), &storage);
        let executor = Arc::new(Executor::new());
        let observing = executor.clone();
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        start(
            restarted.app(),
            executor.clone(),
            senders(),
            storage,
            move |replayed| {
                assert_eq!(replayed, &[("runner-worker".into(), vec![task_id.into()])]);
                let actor = observing.actor_context(&root).unwrap();
                for (_, tasks) in replayed {
                    for task in tasks {
                        assert!(actor.bash_background().task_registration_exists(task));
                    }
                }
                sent.send(()).unwrap();
            },
        )
        .await
        .unwrap();
        received.await.unwrap();
        for actor in executor.actor_contexts() {
            actor.bash_background().detach();
        }
    }

    #[tokio::test]
    async fn startup_callback_also_runs_for_an_empty_replay_set() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), &dir.path().join("storage"));
        let (sent, received) = tokio::sync::oneshot::channel();
        start(
            ctx.app(),
            Arc::new(Executor::new()),
            senders(),
            ctx.storage_dir(),
            move |replayed| {
                assert!(replayed.is_empty());
                sent.send(()).unwrap();
            },
        )
        .await
        .unwrap();
        received.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn held_startup_replay_and_binding_share_one_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let old = context(dir.path(), &storage);
        let (task_id, _, key) = saved_local(&old, dir.path());
        drop(old);
        let restarted = context(dir.path(), &storage);
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let executor = Arc::new(Executor::new());
        executor.register_actor(root.clone(), restarted.clone());
        // A bind has reserved replay but has not restored its session yet.
        let bind = restarted
            .bash_background()
            .claim_project_replay_batch()
            .unwrap();
        let mut job = start(
            restarted.app(),
            executor.clone(),
            senders(),
            storage,
            |_| {},
        );
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut job)
            .await
            .is_err());
        bind.replay_session_for_project(
            &restarted.harness_dir(),
            &crate::harness::Harness::Runner,
            "held-worker",
            dir.path(),
        )
        .unwrap();
        let first = restarted
            .bash_background()
            .task_registration_identity_for_test(&task_id)
            .unwrap();
        job.await.unwrap();
        assert_eq!(
            restarted
                .bash_background()
                .task_registration_identity_for_test(&task_id),
            Some(first)
        );
        std::fs::write(dir.path().join("release"), "").unwrap();
        assert!(settled(&restarted, &key)
            .await
            .body
            .windows(b"held-restart-output".len())
            .any(|part| part == b"held-restart-output"));
        restarted.bash_background().detach();
    }
}
