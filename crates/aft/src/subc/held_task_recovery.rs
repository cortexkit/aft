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

// Each page is an index seek, not an OFFSET scan. The existing replay index
// includes legacy metadata/column disagreements, which are filtered after reading.
const RECOVERY_CHUNK: usize = 64;
const RUNNER_PAGE: &str = "SELECT harness,session_id,cwd,task_id,metadata,
                                project_key,started_at,rowid,status,completion_delivered
    FROM bash_tasks INDEXED BY idx_bash_tasks_project_replay
    WHERE (status NOT IN ('completed','failed','killed','timed_out','fate_unknown')
           OR completion_delivered=0
           OR CASE WHEN json_valid(metadata)
                   THEN json_type(metadata,'$.completion_delivered')='false' ELSE 0 END)
      AND (harness,project_key,started_at,task_id,rowid)>(?1,?2,?3,?4,?5)
    ORDER BY harness,project_key,started_at,task_id,rowid LIMIT ?6";

#[cfg(test)]
thread_local! {
    // The offline benchmark starts cleanup only after recovery owns the mutex.
    static FIRST_RECOVERY_LOCK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

struct RecoveryReader<'a> {
    db: &'a Arc<StdMutex<crate::db::TrackedConnection>>,
    #[cfg(test)]
    on_chunk: &'a mut dyn FnMut(usize, Duration),
}

impl RecoveryReader<'_> {
    fn read<T>(
        &mut self,
        query: impl FnOnce(&crate::db::TrackedConnection) -> rusqlite::Result<Vec<T>>,
    ) -> rusqlite::Result<Vec<T>> {
        let conn = self.db.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
        #[cfg(test)]
        let started = Instant::now();
        #[cfg(test)]
        FIRST_RECOVERY_LOCK.with(|probe| {
            if let Some(observer) = probe.borrow_mut().take() {
                observer();
            }
        });
        let rows = query(&conn)?;
        #[cfg(test)]
        let held = started.elapsed();
        drop(conn);
        #[cfg(test)]
        (self.on_chunk)(rows.len(), held);
        Ok(rows)
    }
}

fn sessions(
    db: &Arc<StdMutex<crate::db::TrackedConnection>>,
    #[cfg(test)] on_chunk: &mut dyn FnMut(usize, Duration),
) -> rusqlite::Result<Vec<Session>> {
    use rusqlite::params;
    let mut reader = RecoveryReader {
        db,
        #[cfg(test)]
        on_chunk,
    };
    let mut pending = std::collections::BTreeMap::<_, std::collections::BTreeSet<String>>::new();
    let mut include = |harness: String, session, cwd: String, task, metadata: String, keyed| {
        let metadata: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        let root = metadata["project_root"].as_str().unwrap_or(&cwd).to_owned();
        let runner = [
            metadata["launching_identity"]["principal"].as_str(),
            metadata["call_key"]["requester"].as_str(),
        ]
        .into_iter()
        .any(|carrier| {
            matches!(
                carrier,
                Some("reserved:thalamus-harness" | "reserved:broca")
            )
        }) || [
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
                .insert(task);
        }
    };
    let mut cursor = (
        String::new(),
        String::new(),
        i64::MIN,
        String::new(),
        i64::MIN,
    );
    loop {
        let rows = reader.read(|conn| {
            conn.prepare(RUNNER_PAGE)?
                .query_map(
                    params![
                        cursor.0,
                        cursor.1,
                        cursor.2,
                        cursor.3,
                        cursor.4,
                        RECOVERY_CHUNK
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, String>(8)?,
                            row.get::<_, bool>(9)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        if rows.is_empty() {
            break;
        }
        for (harness, session, cwd, task, metadata, project, started, rowid, status, delivered) in
            rows
        {
            cursor = (harness.clone(), project, started, task.clone(), rowid);
            if !delivered
                || !matches!(
                    status.as_str(),
                    "completed" | "failed" | "killed" | "timed_out" | "fate_unknown"
                )
            {
                include(harness, session, cwd, task, metadata, false);
            }
        }
    }
    // Start with the small DispatchStarted range of the retention index. Walk
    // NULL and non-NULL settled_at separately so every page has an index bound.
    for null_settled in [true, false] {
        let mut cursor = (i64::MIN, i64::MIN);
        loop {
            let keys = reader.read(|conn| {
                let sql = if null_settled {
                    "SELECT settled_at,rowid,carrier,call_key,task_id,recovered FROM call_ledger
                     WHERE state='DispatchStarted' AND settled_at IS NULL AND rowid>?2
                     ORDER BY settled_at,rowid LIMIT ?3"
                } else {
                    "SELECT settled_at,rowid,carrier,call_key,task_id,recovered FROM call_ledger
                     WHERE state='DispatchStarted' AND (settled_at,rowid)>(?1,?2)
                     ORDER BY settled_at,rowid LIMIT ?3"
                };
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt
                    .query_map(params![cursor.0, cursor.1, RECOVERY_CHUNK], |row| {
                        Ok((
                            row.get::<_, Option<i64>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, bool>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>();
                rows
            })?;
            if keys.is_empty() {
                break;
            }
            for (settled, rowid, carrier, key, task, recovered) in keys {
                cursor = (settled.unwrap_or(i64::MIN), rowid);
                let Some(task) = task.filter(|_| recovered) else {
                    continue;
                };
                let mut owner = (String::new(), String::new());
                loop {
                    let rows = reader.read(|conn| {
                        conn.prepare(
                            "SELECT harness,session_id,cwd,task_id,metadata FROM bash_tasks
                            WHERE task_id=?1 AND (harness,session_id)>(?2,?3)
                            ORDER BY harness,session_id LIMIT ?4",
                        )?
                        .query_map(params![task, owner.0, owner.1, RECOVERY_CHUNK], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()
                    })?;
                    if rows.is_empty() {
                        break;
                    }
                    for (harness, session, cwd, task, metadata) in rows {
                        owner = (harness.clone(), session.clone());
                        let parsed: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
                        if parsed["call_key"]["requester"].as_str() == Some(carrier.as_str())
                            && parsed["call_key"]["key"].as_str() == Some(key.as_str())
                        {
                            include(harness, session, cwd, task, metadata, true);
                        }
                    }
                }
            }
        }
    }
    Ok(pending
        .into_iter()
        .map(|((harness, session_id, root), task_ids)| Session {
            harness,
            session_id,
            project_root: root.into(),
            task_ids: task_ids.into_iter().collect(),
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
    let pending = sessions(
        &db,
        #[cfg(test)]
        &mut |_, _| {},
    )
    .map_err(|error| error.to_string())?;
    let mut replayed = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut missing_roots = std::collections::BTreeMap::<PathBuf, usize>::new();
    for session in pending {
        if std::fs::metadata(&session.project_root)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            let count = missing_roots
                .entry(session.project_root.clone())
                .or_default();
            match settle_missing_root(&db, &session.task_ids) {
                Ok(settled) => *count += settled,
                Err(error) => log::warn!(
                    "held task startup settlement failed for {}/{}: {error}",
                    session.harness,
                    session.session_id
                ),
            }
            continue;
        }
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
    let mut empty_count = 0;
    let mut examples = Vec::new();
    for (root, settled) in missing_roots {
        if settled > 0 {
            crate::slog_warn!(
                "held task startup recovery reason=root_missing root={} settled_entries={settled}",
                root.display()
            );
        } else {
            empty_count += 1;
            if examples.len() < 5 {
                examples.push(root.display().to_string());
            }
        }
    }
    if empty_count > 0 {
        crate::slog_info!(
            "held task startup recovery: {empty_count} task roots no longer exist and had nothing to settle (e.g. {})",
            examples.join(", ")
        );
    }
    Ok(replayed.into_iter().collect())
}

fn settle_missing_root(
    db: &Arc<StdMutex<crate::db::TrackedConnection>>,
    task_ids: &[String],
) -> rusqlite::Result<usize> {
    use crate::db::call_ledger as ledger;
    let conn = db.lock().map_err(|_| rusqlite::Error::InvalidQuery)?;
    let mut settled = 0;
    for task in task_ids {
        let mut stmt = conn.prepare(
            "SELECT carrier,call_key FROM call_ledger
             WHERE recovered=1 AND state='DispatchStarted' AND task_id=?1",
        )?;
        let keys = stmt
            .query_map([task], |row| {
                Ok(ledger::Key {
                    carrier: row.get(0)?,
                    call_key: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for key in keys {
            // A missing project is not evidence that its command exited or was
            // cancelled. Record the outcome as unknown, without signalling a
            // process or contacting its build server.
            settled += usize::from(ledger::settle(
                &conn,
                &key,
                &ledger::unknown_bash_result_missing_root(task),
                "outcome_unknown",
                None,
                ledger::now_ms(),
            )?);
        }
    }
    Ok(settled)
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
#[path = "held_task_recovery_benchmark.rs"]
mod benchmark;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bash_background::persistence::write_task_at;
    #[cfg(unix)]
    use crate::bash_background::persistence::{read_task_at, ResolvedTask};
    #[cfg(unix)]
    use crate::db::call_ledger as ledger;
    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
            serde_json::from_slice::<serde_json::Value>(&settled(&restarted, &key).await.body)
                .unwrap()["content"][0]["text"],
            "Task bash-0000000000000001: outcome unknown (AFT lost the command's result).\nCheck this task with bash_status before running the command again."
        );
        for actor in executor.actor_contexts() {
            actor.bash_background().detach();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn held_deleted_project_root_settles_as_unknown_without_a_route_bind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("deleted-project");
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(&root).unwrap();
        let old = context(&root, &storage);
        let layout = crate::bash_background::persistence::create_task_layout(
            &old.harness_dir(),
            "deleted-root-worker",
            "bash-0000000000000003",
        )
        .unwrap();
        let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
            layout.paths.task_id.clone(),
            "deleted-root-worker".into(),
            "lost command".into(),
            root.clone(),
            Some(root.clone()),
            None,
            true,
            false,
        );
        metadata.status = crate::bash_background::BgTaskStatus::Running;
        metadata.child_pid = Some(i32::MAX as u32);
        write_task_at(&layout, &metadata).unwrap();
        let key = hold_task(&old, &layout, "deleted-root-held");
        drop(old);
        std::fs::remove_dir(&root).unwrap();
        let restarted = context(dir.path(), &storage);
        assert!(!root.exists());
        let executor = startup(&restarted).await;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&settled(&restarted, &key).await.body)
                .unwrap()["content"][0]["text"],
            "Task bash-0000000000000003: outcome unknown (AFT restarted and this command's project folder no longer exists, so it can't tell whether the command ran)."
        );
        assert_eq!(
            executor.actor_count(),
            0,
            "missing roots must not start a registry worker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_roots_log_settlements_and_one_bounded_empty_summary() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let ctx = context(dir.path(), &storage);
        let mut held = Vec::new();
        let mut empty_roots = Vec::new();
        for i in 0..10 {
            // Two sessions share one empty root, which must be counted only once.
            let root_index = if i == 9 { 2 } else { i };
            let root = dir.path().join(format!("missing-{root_index:02}"));
            let session = format!("worker-{i}");
            let task = format!("bash-{i:016x}");
            let layout = crate::bash_background::persistence::create_task_layout(
                &ctx.harness_dir(),
                &session,
                &task,
            )
            .unwrap();
            let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
                task,
                session,
                "lost command".into(),
                root.clone(),
                Some(root.clone()),
                None,
                true,
                false,
            );
            metadata.status = crate::bash_background::BgTaskStatus::Running;
            metadata.harness = Some("runner".into());
            write_task_at(&layout, &metadata).unwrap();
            if i < 2 {
                held.push((root, hold_task(&ctx, &layout, &format!("held-{i}"))));
            } else {
                crate::db::bash_tasks::upsert_bash_task(
                    &ctx.db().unwrap().lock().unwrap(),
                    &metadata.to_bash_task_row("runner", &layout.paths).unwrap(),
                )
                .unwrap();
                if i != 9 {
                    empty_roots.push(root);
                }
            }
        }
        let executor = Arc::new(Executor::new());
        let (result, logs) = crate::logging::capture_log_lines(|| {
            recover(&ctx.app(), &executor, &senders(), &storage)
        });
        assert!(result.unwrap().is_empty());
        let recovery_logs: Vec<_> = logs
            .iter()
            .filter(|line| line.contains("held task startup recovery"))
            .collect();
        assert_eq!(recovery_logs.len(), 3, "{recovery_logs:?}");
        for (root, key) in &held {
            let expected = format!(
                "WARN held task startup recovery reason=root_missing root={} settled_entries=1",
                root.display()
            );
            assert_eq!(
                recovery_logs
                    .iter()
                    .filter(|line| line.as_str() == expected)
                    .count(),
                1
            );
            let row = ledger::get(&ctx.db().unwrap().lock().unwrap(), key)
                .unwrap()
                .unwrap();
            assert_eq!(row.state, ledger::State::Settled);
            assert_eq!(row.frame.unwrap().body, ledger::unknown_held_result().body);
        }
        let summary = recovery_logs
            .iter()
            .find(|line| line.starts_with("INFO "))
            .unwrap();
        assert!(summary.contains("7 task roots no longer exist and had nothing to settle"));
        for (index, root) in empty_roots.iter().enumerate() {
            assert_eq!(summary.contains(&root.display().to_string()), index < 5);
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

    #[test]
    fn recovery_selection_uses_bounded_index_pages_and_releases_shared_mutex() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), &dir.path().join("storage"));
        let db = ctx.db().unwrap();
        {
            let conn = db.lock().unwrap();
            conn.execute_batch(r#"BEGIN;
                WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<4095)
                INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,
                                       started_at,completion_delivered,metadata)
                SELECT 'runner','history',printf('old-%05d',i),'root','done','/root','completed',0,1,'{}' FROM n;
                WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<129)
                INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,
                                       started_at,completion_delivered,metadata)
                SELECT 'runner',printf('active-%03d',i),'same-task','root','running','/root','running',0,1,'{}' FROM n;
                INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,
                                       started_at,completion_delivered,metadata) VALUES
                ('runner','undelivered','undelivered','root','','/root','completed',0,0,'{}'),
                ('opencode','keyed','keyed','root','','/root','completed',0,1,
                    '{"project_root":"/project","call_key":{"requester":"direct","key":"held"}}'),
                ('opencode','wrong-owner','keyed','root','','/root','completed',0,1,'{}'),
                ('runner','legacy-disagreement','legacy','root','','/root','completed',0,1,'{"completion_delivered":false}'),
                ('opencode','not-runner','not-runner','root','','/root','running',0,0,'{}'),
                ('opencode','not-recovered','not-recovered','root','','/root','completed',0,1,
                    '{"call_key":{"requester":"direct","key":"not-recovered"}}'),
                ('opencode','settled','settled','root','','/root','completed',0,1,
                    '{"call_key":{"requester":"direct","key":"settled"}}');
                INSERT INTO call_ledger(carrier,call_key,digest,state,event_id,task_id,recovered) VALUES
                ('direct','held','','DispatchStarted','','keyed',1),
                ('direct','not-recovered','','DispatchStarted','','not-recovered',0),
                ('direct','settled','','Settled','','settled',1);
                WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<129)
                INSERT INTO call_ledger(carrier,call_key,digest,state,event_id,settled_at,recovered)
                SELECT 'direct',printf('unlinked-%03d',i),'','DispatchStarted','',
                       CASE WHEN i<65 THEN NULL ELSE 1 END,1 FROM n;
                COMMIT;"#).unwrap();
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {RUNNER_PAGE}"))
                .unwrap()
                .query_map(
                    rusqlite::params!["", "", i64::MIN, "", i64::MIN, RECOVERY_CHUNK],
                    |row| row.get::<_, String>(3),
                )
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join("\n");
            assert!(
                plan.contains("SEARCH bash_tasks USING INDEX idx_bash_tasks_project_replay"),
                "{plan}"
            );
            assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        }
        let mut chunks = Vec::new();
        let selected = sessions(&db, &mut |rows, held| {
            assert!(
                db.try_lock().is_ok(),
                "chunk observer must run after unlocking"
            );
            assert!(rows <= RECOVERY_CHUNK, "read {rows} rows in one lock hold");
            chunks.push((rows, held));
        })
        .unwrap();
        let mut expected = (0..130)
            .map(|i| Session {
                harness: "runner".into(),
                session_id: format!("active-{i:03}"),
                project_root: "/root".into(),
                task_ids: vec!["same-task".into()],
            })
            .collect::<Vec<_>>();
        expected.insert(
            0,
            Session {
                harness: "opencode".into(),
                session_id: "keyed".into(),
                project_root: "/project".into(),
                task_ids: vec!["keyed".into()],
            },
        );
        expected.push(Session {
            harness: "runner".into(),
            session_id: "undelivered".into(),
            project_root: "/root".into(),
            task_ids: vec!["undelivered".into()],
        });
        assert_eq!(selected, expected);
        assert!(
            chunks
                .iter()
                .filter(|(rows, _)| *rows == RECOVERY_CHUNK)
                .count()
                >= 4,
            "both candidate sources must cross page boundaries: {chunks:?}"
        );
    }

    #[test]
    fn runner_saved_selection_includes_carriers_and_parsed_harnesses_only() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), &dir.path().join("storage"));
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        for (index, carrier, raw_harness, delivered) in [
            (1, Some("reserved:broca"), None, false),
            (2, Some("reserved:thalamus-harness"), None, false),
            (3, None, Some("claude-code"), false),
            (4, Some("direct"), Some("opencode"), false),
            (5, Some("reserved:broca"), None, true),
        ] {
            let session = format!("saved-{index}");
            let layout = crate::bash_background::persistence::create_task_layout(
                &ctx.storage_dir().join("opencode"),
                &session,
                &format!("bash-{index:016x}"),
            )
            .unwrap();
            let mut metadata = crate::bash_background::persistence::PersistedTask::starting(
                layout.paths.task_id.clone(),
                session,
                "finished".into(),
                dir.path().into(),
                Some(dir.path().into()),
                None,
                true,
                false,
            );
            metadata.harness = Some("opencode".into());
            metadata.launching_raw_harness = raw_harness.map(str::to_owned);
            metadata.call_key = carrier.map(|carrier| crate::bash_background::TaskCallKey {
                requester: carrier.into(),
                key: "unlinked".into(),
                minted: false,
            });
            metadata.mark_terminal(
                crate::bash_background::BgTaskStatus::Completed,
                Some(0),
                None,
            );
            metadata.completion_delivered = delivered;
            crate::db::bash_tasks::upsert_bash_task(
                &conn,
                &metadata
                    .to_bash_task_row("opencode", &layout.paths)
                    .unwrap(),
            )
            .unwrap();
        }
        drop(conn);
        let selected = sessions(&db, &mut |_, _| {}).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["saved-1", "saved-2", "saved-3"]
        );
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
