//! Ask the daemon whether a scope ended before killing its shell tasks.
//! Route closure and module drain alone do not establish scope end.

use super::*;
use subc_protocol::{scope::ScopeStamp, session::ModuleControlResponseToModule};

pub(super) fn describe_timeout() -> Duration {
    timing(
        "AFT_TEST_SCOPE_DESCRIBE_TIMEOUT_MS",
        Duration::from_secs(10),
    )
}
fn poll_interval() -> Duration {
    timing("AFT_TEST_SCOPE_POLL_MS", Duration::from_secs(60))
}
fn timing(_name: &str, production: Duration) -> Duration {
    #[cfg(feature = "test-timing-hooks")]
    if let Some(ms) = std::env::var(_name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    {
        return Duration::from_millis(ms);
    }
    production
}

/// A changed epoch for the same scope reference identifies a rebind of the
/// same agent, not its termination. Ended means the daemon remembers an end;
/// synced NotLive means the owner published its scope set without this scope.
pub(super) fn describe_answer_ends_scope(reply: &ModuleControlResponseToModule) -> bool {
    matches!(
        reply,
        ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::Ended,
            ..
        } | ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::NotLive,
            owner_synced: true,
            ..
        }
    )
}

pub(super) fn stamp_key(stamp: &ScopeStamp) -> (String, String, u64) {
    (
        principal_id(&Some(stamp.owner.clone())).expect("scope owner is verified"),
        stamp.scope_ref.clone(),
        stamp.scope_epoch,
    )
}

#[derive(Clone)]
pub(super) struct ScopeDescribes {
    goodbye: mpsc::UnboundedSender<ScopeStamp>,
    routed: Arc<StdMutex<HashSet<(String, String, u64)>>>,
}
impl ScopeDescribes {
    pub(super) fn routes(&self, routes: &HashMap<RouteChannel, RouteIdentity>) {
        *self
            .routed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = routes
            .values()
            .filter_map(|identity| identity.scope.as_ref())
            .map(stamp_key)
            .collect();
    }
    pub(super) fn goodbye(&self, stamp: ScopeStamp) {
        let _ = self.goodbye.send(stamp);
    }
}

pub(super) struct EndedScope {
    pub(super) stamp: ScopeStamp,
    pub(super) keys: Vec<crate::db::call_ledger::Key>,
}

pub(super) fn start(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    run_scopes: Arc<super::run_scope::RunScopes>,
) -> (
    ScopeDescribes,
    mpsc::UnboundedReceiver<EndedScope>,
    JoinHandle<()>,
) {
    start_with_scope_source(control, executor.clone(), poll_interval(), move || {
        work_scopes(executor.clone(), run_scopes.clone())
    })
}

fn start_with_scope_source<F, Fut>(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    interval: Duration,
    scope_source: F,
) -> (
    ScopeDescribes,
    mpsc::UnboundedReceiver<EndedScope>,
    JoinHandle<()>,
)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Vec<ScopeStamp>> + Send,
{
    let (goodbye, mut closes) = mpsc::unbounded_channel();
    let (ended, receiver) = mpsc::unbounded_channel();
    let routed = Arc::new(StdMutex::new(HashSet::new()));
    let handle = ScopeDescribes {
        goodbye,
        routed: routed.clone(),
    };
    let worker = tokio::spawn(async move {
        if !control.advertises(subc_protocol::scope::SCOPE_DESCRIBE_OP) {
            return;
        }
        let mut queued = HashMap::new();
        let mut retry = HashMap::new();
        let mut pending = HashSet::new();
        let mut next_poll: HashMap<_, tokio::time::Instant> = HashMap::new();
        let mut inflight = tokio::task::JoinSet::new();
        let mut next_scan = tokio::time::Instant::now() + interval;
        loop {
            while inflight.len() < 4 && !queued.is_empty() {
                let key = queued.keys().next().cloned().unwrap();
                let stamp: ScopeStamp = queued.remove(&key).unwrap();
                let control = control.clone();
                inflight.spawn(async move {
                    let reply = control.describe(&stamp).await;
                    (key, stamp, reply)
                });
            }
            let routed_now = routed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let wake_at = next_poll
                .iter()
                .filter(|(key, _)| {
                    !pending.contains(*key)
                        && (!routed_now.contains(*key) || retry.contains_key(*key))
                })
                .map(|(_, due)| *due)
                .min()
                .map_or(next_scan, |due| due.min(next_scan));
            tokio::select! {
                Some(stamp) = closes.recv() => {
                    // An idle route needs no RPC; parked consent asks still
                    // need a scope-end determination even without a process.
                    let scopes = scope_source().await;
                    let key = stamp_key(&stamp);
                    // Let the test daemon observe the exact Goodbye snapshot,
                    // including a close that found no registered shell task.
                    #[cfg(feature = "test-timing-hooks")]
                    if let Some(path) = std::env::var_os("AFT_TEST_SCOPE_GOODBYE_SAMPLED_FILE") {
                        let matches = scopes.iter().filter(|live| stamp_key(live) == key).count();
                        let _ = std::fs::write(path, matches.to_string());
                    }
                    if scopes.iter().any(|live| stamp_key(live) == key) && pending.insert(key.clone()) {
                        #[cfg(feature = "test-timing-hooks")]
                        crate::slog_info!("scope diagnostic phase=goodbye_describe_queued scope_epoch={}", stamp.scope_epoch);
                        queued.insert(key, stamp);
                    }
                }
                _ = tokio::time::sleep_until(wake_at) => {
                    let scopes = scope_source().await;
                    next_scan = tokio::time::Instant::now() + interval;
                    retry.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    next_poll.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    let routed = routed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                    for stamp in scopes {
                        let key = stamp_key(&stamp);
                        if (!routed.contains(&key) || retry.contains_key(&key))
                            && next_poll.get(&key).is_none_or(|due| tokio::time::Instant::now() >= *due)
                            && pending.insert(key.clone()) {
                            queued.insert(key, stamp);
                        }
                    }
                }
                Some(result) = inflight.join_next(), if !inflight.is_empty() => {
                    if let Ok((key, stamp, reply)) = result {
                        // A Goodbye query can finish just before the connection's
                        // periodic tick. Wait a full interval after its answer
                        // before rechecking shell tasks with that owner/ref/epoch.
                        next_poll.insert(key.clone(), tokio::time::Instant::now() + interval);
                        if reply.as_ref().is_ok_and(describe_answer_ends_scope) {
                            #[cfg(feature = "test-timing-hooks")]
                            crate::slog_info!("scope diagnostic phase=describe_ended scope_epoch={}", stamp.scope_epoch);
                            let keys = revoke(executor.clone(), &stamp).await;
                            let _ = ended.send(EndedScope { stamp, keys });
                            retry.remove(&key);
                        } else if !matches!(reply, Ok(ModuleControlResponseToModule::ScopeDescribe { status: subc_protocol::scope::ScopeStatus::Live, .. })) {
                            retry.insert(key.clone(), stamp);
                        } else {
                            retry.remove(&key);
                        }
                        // Non-definitive answers and errors retry only on the
                        // regular poll, never in a reconnect-induced busy loop.
                        pending.remove(&key);
                    }
                }
                else => break,
            }
        }
    });
    (handle, receiver, worker)
}

async fn work_scopes(
    executor: Arc<Executor>,
    run_scopes: Arc<super::run_scope::RunScopes>,
) -> Vec<ScopeStamp> {
    tokio::task::spawn_blocking(move || {
        let mut scopes: Vec<_> = executor
            .actor_contexts()
            .into_iter()
            .flat_map(|ctx| ctx.bash_background().live_scope_stamps())
            .collect();
        scopes.extend(run_scopes.parked_scopes());
        scopes
    })
    .await
    .unwrap_or_default()
}

async fn revoke(executor: Arc<Executor>, stamp: &ScopeStamp) -> Vec<crate::db::call_ledger::Key> {
    let entries = executor.actor_entries();
    let mut keys = Vec::new();
    for (root, _) in entries {
        let stamp = stamp.clone();
        #[cfg(feature = "test-timing-hooks")]
        let submitted_at = std::time::Instant::now();
        let (result, receiver) = oneshot::channel();
        let job = executor.submit_async(
            root,
            Lane::Mutating,
            "scope-ended".into(),
            Box::new(move |ctx| {
                #[cfg(feature = "test-timing-hooks")]
                crate::slog_info!(
                    "scope diagnostic phase=revoke_admitted scope_epoch={} queue_ms={}",
                    stamp.scope_epoch,
                    submitted_at.elapsed().as_millis()
                );
                let scope = crate::db::call_ledger::ScopeIdentity {
                    owner: principal_id(&Some(stamp.owner.clone()))
                        .expect("scope owner is verified"),
                    scope_ref: stamp.scope_ref.clone(),
                    scope_epoch: stamp.scope_epoch,
                };
                let keys = ctx
                    .db()
                    .and_then(|db| {
                        let conn = db.lock().ok()?;
                        // Persist confirmed ends even if this epoch never registered
                        // a policy, so older head policies cannot be reused after end.
                        if let Err(error) = crate::db::access_grants::end_scope_grants(
                            &conn,
                            &crate::db::access_grants::ScopeKey::from_stamp(&stamp),
                        ) {
                            log::warn!("scope-ended grant revocation failed: {error}");
                        }
                        match crate::db::call_ledger::end_scope(
                            &conn,
                            &scope,
                            crate::db::call_ledger::now_ms(),
                        ) {
                            Ok(keys) => Some(keys),
                            Err(error) => {
                                log::warn!("scope-ended ledger settlement failed: {error}");
                                None
                            }
                        }
                    })
                    .unwrap_or_default();
                // Drop the ledger lock before killing: terminal task writes use
                // the actor's same connection and must not deadlock on it.
                ctx.bash_background().kill_scope(&stamp);
                #[cfg(feature = "test-timing-hooks")]
                crate::slog_info!(
                    "scope diagnostic phase=revoke_finished scope_epoch={} elapsed_ms={}",
                    stamp.scope_epoch,
                    submitted_at.elapsed().as_millis()
                );
                let _ = result.send(keys);
                Response::success("scope-ended", json!({}))
            }),
        );
        let _ = await_executor_response(job, "scope-ended".into()).await;
        keys.extend(receiver.await.unwrap_or_default());
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn confirmed_head_end_tombstones_policy_for_later_epochs() {
        use crate::db::access_grants::{self as store, ScopeKey};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("head");
        std::fs::create_dir(&root).unwrap();
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(dir.path().join("store"));
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config.clone(),
        ));
        ctx.set_db(Arc::new(StdMutex::new(
            crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
        )));
        let stamp: ScopeStamp = serde_json::from_value(json!({
            "owner":{"kind":"reserved","module_id":"prefrontal-core"},
            "ref":"head", "scope_epoch":10, "kind":"head", "owner_authorized":true
        }))
        .unwrap();
        let key = ScopeKey::from_stamp(&stamp);
        let older = ScopeKey {
            epoch: 7,
            ..key.clone()
        };
        {
            let db = ctx.db().unwrap();
            let conn = db.lock().unwrap();
            store::register_scope(&conn, &older, None, &config).unwrap();
            assert_eq!(
                store::head_policy_scope(&conn, &key).unwrap(),
                Some(older.clone())
            );
        }
        let executor = Arc::new(Executor::new());
        executor.register_actor(ProjectRootId::from_path(&root).unwrap(), ctx.clone());
        let (writer, mut frames) = mpsc::channel(8);
        let pending = readiness::PendingControlReplies::default();
        let control = Arc::new(readiness::DaemonControl::new(
            writer,
            Arc::new(DispatchPathMetrics::new()),
            PROTOCOL_VERSION,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
            vec![],
            pending.clone(),
            HELLO_CORR + 1,
        ));
        let live = stamp.clone();
        let (scopes, mut ended, worker) =
            start_with_scope_source(control, executor, Duration::from_secs(60), move || {
                std::future::ready(vec![live.clone()])
            });
        scopes.goodbye(stamp);
        let request = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .unwrap()
            .unwrap();
        let answer = ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::Ended,
            scope_epoch: Some(10),
            daemon_incarnation: "test-daemon".into(),
            owner_synced: true,
            owner_configured: true,
            scope: None,
        };
        assert!(pending.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&answer).unwrap(),
            )
            .unwrap()
        ));
        tokio::time::timeout(Duration::from_secs(5), ended.recv())
            .await
            .unwrap()
            .unwrap();
        worker.abort();
        let db = ctx.db().unwrap();
        let conn = db.lock().unwrap();
        assert!(store::scope_config(&conn, &older).unwrap().is_some());
        for epoch in [10, 12] {
            let target = ScopeKey {
                epoch,
                ..key.clone()
            };
            assert!(store::head_policy_scope(&conn, &target).unwrap().is_none());
        }
    }

    #[cfg(not(feature = "test-timing-hooks"))]
    #[test]
    fn production_scope_timing_is_exact() {
        assert_eq!(describe_timeout(), Duration::from_secs(10));
        assert_eq!(poll_interval(), Duration::from_secs(60));
    }

    #[tokio::test(start_paused = true)]
    async fn live_epoch_move_repolls_only_after_scope_poll_interval() {
        let interval = Duration::from_millis(500);
        let stamp = ScopeStamp::new(
            subc_protocol::Principal::Direct,
            "scope",
            1,
            subc_protocol::scope::ScopeKind::Head,
        );
        let (writer, mut frames) = mpsc::channel(8);
        let pending = readiness::PendingControlReplies::default();
        let control = Arc::new(readiness::DaemonControl::new(
            writer,
            Arc::new(DispatchPathMetrics::new()),
            PROTOCOL_VERSION,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
            vec![],
            pending.clone(),
            HELLO_CORR + 1,
        ));
        let live = stamp.clone();
        let (scopes, mut ended, worker) =
            start_with_scope_source(control, Arc::new(Executor::new()), interval, move || {
                std::future::ready(vec![live.clone()])
            });
        tokio::task::yield_now().await;
        // Finish a Goodbye query shortly before the connection's periodic tick.
        tokio::time::advance(Duration::from_millis(400)).await;
        scopes.goodbye(stamp.clone());
        let first = frames.recv().await.unwrap();
        let answer = ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::Live,
            scope_epoch: Some(2),
            daemon_incarnation: "test-daemon".into(),
            owner_synced: true,
            owner_configured: true,
            scope: None,
        };
        assert!(pending.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                first.header.corr,
                serde_json::to_vec(&answer).unwrap(),
            )
            .unwrap()
        ));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let answered_at = tokio::time::Instant::now();
        tokio::time::advance(interval - Duration::from_millis(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            frames.try_recv().is_err(),
            "scope.describe repeated before the interval"
        );
        assert!(
            ended.try_recv().is_err(),
            "live epoch movement revoked the scope"
        );
        // The task's route closed, but Live did not end its scope.
        // A later timer must keep checking for an actual scope end.
        tokio::time::advance(Duration::from_millis(1)).await;
        let repeated = frames.recv().await.unwrap();
        assert_eq!(tokio::time::Instant::now(), answered_at + interval);
        assert_eq!(repeated.header.ty, FrameType::Request);
        let body: serde_json::Value = serde_json::from_slice(&repeated.body).unwrap();
        assert_eq!(body["op"], "scope.describe");
        assert_eq!(body["ref"], stamp.scope_ref);
        assert_ne!(repeated.header.corr, first.header.corr);
        assert!(ended.try_recv().is_err());
        worker.abort();
    }
}
