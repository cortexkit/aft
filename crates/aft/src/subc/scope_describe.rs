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

/// A live rebind does not end ordinary commands, but old-epoch grants must
/// never authorize another request from a surviving secret command.
#[cfg(unix)]
fn secret_authorization_rebound(stamp: &ScopeStamp, reply: &ModuleControlResponseToModule) -> bool {
    matches!(reply, ModuleControlResponseToModule::ScopeDescribe { status: subc_protocol::scope::ScopeStatus::Live, scope_epoch: Some(epoch), .. } if *epoch != stamp.scope_epoch)
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
    observed: mpsc::UnboundedSender<ScopeStamp>,
}

pub(super) fn start_for_speech(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    run_scopes: Arc<super::run_scope::RunScopes>,
    speech: Arc<crate::gh_shim_speech::service::Service>,
) -> (
    ScopeDescribes,
    mpsc::UnboundedReceiver<EndedScope>,
    JoinHandle<()>,
) {
    let watched_runs = run_scopes.clone();
    start_with_scope_source_impl(
        control,
        executor.clone(),
        poll_interval(),
        move || work_scopes(executor.clone(), watched_runs.clone()),
        Some(speech),
        Some(run_scopes),
    )
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

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the binary's consent fixtures use this helper; library fixtures do not"
)]
pub(super) fn start(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    run_scopes: Arc<super::run_scope::RunScopes>,
) -> (
    ScopeDescribes,
    mpsc::UnboundedReceiver<EndedScope>,
    JoinHandle<()>,
) {
    let watched_runs = run_scopes.clone();
    start_with_scope_source_impl(
        control,
        executor.clone(),
        poll_interval(),
        move || work_scopes(executor.clone(), watched_runs.clone()),
        None,
        Some(run_scopes),
    )
}

#[cfg(test)]
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
    start_with_scope_source_impl(control, executor, interval, scope_source, None, None)
}

fn start_with_scope_source_impl<F, Fut>(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    interval: Duration,
    scope_source: F,
    speech: Option<Arc<crate::gh_shim_speech::service::Service>>,
    run_scopes: Option<Arc<super::run_scope::RunScopes>>,
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
    let (observed, mut observations) = mpsc::unbounded_channel();
    let (ended, receiver) = mpsc::unbounded_channel();
    let routed = Arc::new(StdMutex::new(HashSet::new()));
    let handle = ScopeDescribes {
        goodbye,
        routed: routed.clone(),
        observed,
    };
    if let Some(service) = &speech {
        let sender = handle.observed.clone();
        service.set_end_notice(Arc::new(move |stamp| {
            let _ = sender.send(stamp);
        }));
    }
    let worker = tokio::spawn(async move {
        if !control.advertises(subc_protocol::scope::SCOPE_DESCRIBE_OP) {
            return;
        }
        let mut queued = HashMap::new();
        let mut retry = HashMap::new();
        let mut pending = HashSet::new();
        let mut confirmed = HashSet::new();
        let mut next_poll: HashMap<_, tokio::time::Instant> = HashMap::new();
        // Only unix refreshes the set from secret proxy authorizations.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut secret_authorizations: HashSet<(String, String, u64)> = HashSet::new();
        let mut inflight = tokio::task::JoinSet::new();
        let mut fanout = tokio::task::JoinSet::new();
        let mut next_scan = tokio::time::Instant::now() + interval;
        loop {
            while inflight.len() < 4 && !queued.is_empty() {
                let key = queued.keys().next().cloned().unwrap();
                let stamp: ScopeStamp = queued.remove(&key).unwrap();
                if confirmed.contains(&key) {
                    pending.remove(&key);
                    continue;
                }
                let control = control.clone();
                let speech = speech.clone();
                inflight.spawn(async move {
                    let sent = std::time::Instant::now();
                    let generation = speech
                        .as_ref()
                        .map(|service| service.generation())
                        .unwrap_or_default();
                    let reply = control.describe(&stamp).await;
                    control
                        .metrics
                        .scope_descriptions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .record(&stamp, reply.as_ref().ok(), Instant::now());
                    if let Some(service) = &speech {
                        service.remember_probe(
                            &stamp,
                            &generation,
                            sent,
                            reply
                                .as_ref()
                                .map(Clone::clone)
                                .map_err(ToString::to_string),
                        );
                    }
                    (key, stamp, reply, generation)
                });
            }
            let routed_now = routed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let wake_at = next_poll
                .iter()
                .filter(|(key, _)| {
                    !confirmed.contains(*key)
                        && !pending.contains(*key)
                        && (!routed_now.contains(*key)
                            || secret_authorizations.contains(*key)
                            || retry.contains_key(*key))
                })
                .map(|(_, due)| *due)
                .min()
                .map_or(next_scan, |due| due.min(next_scan));
            tokio::select! {
                Some(stamp)=observations.recv()=>{
                    if let Some(service)=&speech { if confirmed.insert(stamp_key(&stamp)) {
                        control.metrics.scope_descriptions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).cleanup_started(&stamp);
                        if let Some(scopes)=&run_scopes { scopes.revoke(&stamp); }
                        fence_confirmed_end(&executor, &stamp);
                        let service=service.clone();
                        let executor=executor.clone();
                        let metrics=control.metrics.clone();
                        let notices=ended.clone();
                        fanout.spawn(async move { finish_fenced_notice(executor, stamp.clone(), || service.store.scope_ended(&stamp), metrics, notices).await });
                    } }
                }
                Some(Ok(Some(notice)))=fanout.join_next(), if !fanout.is_empty()=>{ let _=ended.send(notice); }
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
                    if !confirmed.contains(&key) && scopes.iter().any(|live| stamp_key(live) == key) && pending.insert(key.clone()) {
                        #[cfg(feature = "test-timing-hooks")]
                        crate::slog_info!("scope diagnostic phase=goodbye_describe_queued scope_epoch={}", stamp.scope_epoch);
                        queued.insert(key, stamp);
                    }
                }
                _ = tokio::time::sleep_until(wake_at) => {
                    let scopes = scope_source().await;
                    #[cfg(unix)]
                    {
                        let executor = executor.clone();
                        secret_authorizations = tokio::task::spawn_blocking(move || executor.actor_contexts().into_iter().flat_map(|ctx| ctx.bash_background().secret_authorization_scope_stamps()).map(|stamp| stamp_key(&stamp)).collect()).await.unwrap_or_default();
                    }
                    next_scan = tokio::time::Instant::now() + interval;
                    retry.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    next_poll.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    let routed = routed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                    for stamp in scopes {
                        let key = stamp_key(&stamp);
                        if !confirmed.contains(&key) && (!routed.contains(&key) || secret_authorizations.contains(&key) || retry.contains_key(&key))
                            && next_poll.get(&key).is_none_or(|due| tokio::time::Instant::now() >= *due)
                            && pending.insert(key.clone()) {
                            queued.insert(key, stamp);
                        }
                    }
                }
                Some(result) = inflight.join_next(), if !inflight.is_empty() => {
                    if let Ok((key, stamp, reply,generation)) = result {
                        // A Goodbye query can finish just before the connection's
                        // periodic tick. Wait a full interval after its answer
                        // before rechecking shell tasks with that owner/ref/epoch.
                        next_poll.insert(key.clone(), tokio::time::Instant::now() + interval);
                        #[cfg(unix)]
                        if reply.as_ref().is_ok_and(|answer| secret_authorization_rebound(&stamp, answer)) {
                            let executor = executor.clone();
                            let described = stamp.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                for ctx in executor.actor_contexts() { ctx.bash_background().revoke_secret_authorization(&described); }
                            }).await;
                        }
                        if let Some(service)=&speech{
                            if let Ok(reply)=&reply{
                                let role=if stamp.attributes.run_id.is_some(){crate::gh_shim_speech::ScopeRole::Run}else{crate::gh_shim_speech::ScopeRole::Authority};
                                match crate::gh_shim_speech::scope::validate_scope_identity(&stamp,role,reply,&service.generation(),&generation,crate::db::call_ledger::now_ms() as u64){
                                    Ok(crate::gh_shim_speech::scope::Answer::Ended(proof))=>{
                                        if service.record_end(&stamp,proof).is_err(){kill_only(executor.clone(),&stamp).await;}
                                        retry.remove(&key);
                                    },
                                    Ok(crate::gh_shim_speech::scope::Answer::Live(_))=>{retry.remove(&key);},
                                    Err(_)=>{retry.insert(key.clone(),stamp);},
                                }
                            }else{retry.insert(key.clone(),stamp);}
                        }else if reply.as_ref().is_ok_and(describe_answer_ends_scope) {
                            #[cfg(feature = "test-timing-hooks")]
                            crate::slog_info!("scope diagnostic phase=describe_ended scope_epoch={}", stamp.scope_epoch);
                            if confirmed.insert(key.clone()) {
                                control.metrics.scope_descriptions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).cleanup_started(&stamp);
                                if let Some(scopes)=&run_scopes { scopes.revoke(&stamp); }
                                fence_confirmed_end(&executor, &stamp);
                                let executor=executor.clone();
                                let metrics=control.metrics.clone();
                                let notices=ended.clone();
                                fanout.spawn(async move { finish_confirmed_end(executor, stamp, metrics, notices).await });
                            }
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

async fn finish_fenced_notice(
    executor: Arc<Executor>,
    stamp: ScopeStamp,
    mut check: impl FnMut() -> Result<bool, crate::gh_shim_speech::Refusal> + Send,
    metrics: Arc<DispatchPathMetrics>,
    notices: mpsc::UnboundedSender<EndedScope>,
) -> Option<EndedScope> {
    let mut delay = Duration::from_millis(100);
    loop {
        match check() {
            Ok(true) => {
                return finish_confirmed_end(executor, stamp, metrics, notices).await;
            }
            Ok(false) => {
                log::warn!("committed scope-end notice is not yet visible; retaining fanout")
            }
            Err(error) => log::warn!(
                "scope-end fence read failed ({}); retaining fanout",
                error.reason
            ),
        }
        // A committed notice remains pending until it can complete lifecycle cleanup.
        // Each notice has its own task so storage trouble does not stop other probes.
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

#[cfg(all(test, unix))]
#[path = "scope_describe_speech_tests.rs"]
mod speech_fanout_tests;

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

#[cfg(all(test, unix))]
pub(crate) async fn confirm_secret_rebind_for_test(
    ctx: Arc<AppContext>,
    stamp: ScopeStamp,
    task_id: &str,
) {
    let executor = Arc::new(Executor::new());
    let root = ProjectRootId::from_path(ctx.config().project_root.as_ref().unwrap()).unwrap();
    executor.register_actor(root, ctx.clone());
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
    let source = ctx.clone();
    let (scopes, mut ended, worker) =
        start_with_scope_source(control, executor, Duration::from_millis(20), move || {
            let source = source.clone();
            async move {
                tokio::task::spawn_blocking(move || source.bash_background().live_scope_stamps())
                    .await
                    .unwrap()
            }
        });
    // A still-open head route must not suppress authorization-epoch checks.
    scopes.routed.lock().unwrap().insert(stamp_key(&stamp));
    let until = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut answered_head = false;
    loop {
        if answered_head
            && ctx
                .bash_background()
                .secret_proxy_address_for_test(task_id)
                .is_none()
        {
            break;
        }
        let frame = tokio::time::timeout_at(until, frames.recv())
            .await
            .expect("head epoch change was not applied")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
        let is_head = body["ref"].as_str() == Some(stamp.scope_ref.as_str());
        let answer = ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::Live,
            scope_epoch: Some(if is_head { stamp.scope_epoch + 1 } else { 1 }),
            daemon_incarnation: "audit".into(),
            owner_synced: true,
            owner_configured: true,
            scope: None,
        };
        answered_head |= is_head;
        assert!(pending.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&answer).unwrap()
            )
            .unwrap()
        ));
        tokio::task::yield_now().await;
    }
    assert!(
        ended.try_recv().is_err(),
        "a rebind is not an ordinary scope end"
    );
    worker.abort();
}

fn fence_confirmed_end(executor: &Executor, stamp: &ScopeStamp) {
    for ctx in executor.actor_contexts() {
        ctx.bash_background().fence_scope_end(stamp);
    }
}

async fn finish_confirmed_end(
    executor: Arc<Executor>,
    stamp: ScopeStamp,
    metrics: Arc<DispatchPathMetrics>,
    notices: mpsc::UnboundedSender<EndedScope>,
) -> Option<EndedScope> {
    metrics
        .scope_descriptions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .cleanup_started(&stamp);
    let first = revoke(executor.clone(), &stamp).await;
    metrics
        .scope_descriptions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .cleanup_result(&stamp, first.complete);
    // Publish revocation even when settlement failed: the shared live fence is
    // already installed, and a committed tombstone independently blocks dispatch.
    let _ = notices.send(EndedScope {
        stamp: stamp.clone(),
        keys: first.keys,
    });
    if first.complete {
        return None;
    }
    for delay in [1, 2, 4] {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        let result = revoke(executor.clone(), &stamp).await;
        metrics
            .scope_descriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cleanup_result(&stamp, result.complete);
        if result.complete {
            return Some(EndedScope {
                stamp,
                keys: result.keys,
            });
        }
    }
    log::warn!(
        "scope-ended cleanup remains fenced after bounded retries scope_ref={:?} scope_epoch={}",
        stamp.scope_ref,
        stamp.scope_epoch
    );
    None
}

struct ScopeRevokeResult {
    keys: Vec<crate::db::call_ledger::Key>,
    complete: bool,
}

async fn revoke(executor: Arc<Executor>, stamp: &ScopeStamp) -> ScopeRevokeResult {
    let entries = executor.actor_entries();
    fence_confirmed_end(&executor, stamp);
    let mut keys = Vec::new();
    let mut complete = true;
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
                let mut failures = Vec::new();
                let keys = match ctx.db() {
                    Some(db) => { let settled = match db.lock() {
                        Ok(conn) => {
                            // Persist confirmed ends even if this epoch never registered
                            // a policy, so older head policies cannot be reused after end.
                            if let Err(error) = crate::db::access_grants::end_scope_grants(
                                &conn,
                                &crate::db::access_grants::ScopeKey::from_stamp(&stamp),
                            ) {
                                log::warn!("scope-ended grant revocation failed scope_ref={:?} scope_epoch={}: {error}", stamp.scope_ref, stamp.scope_epoch);
                                failures.push(format!("grant revocation: {error}"));
                            }
                            match crate::db::call_ledger::end_scope(
                                &conn,
                                &scope,
                                crate::db::call_ledger::now_ms(),
                            ) {
                                Ok(keys) => keys,
                                Err(error) => {
                                    log::warn!("scope-ended ledger settlement failed scope_ref={:?} scope_epoch={}: {error}", stamp.scope_ref, stamp.scope_epoch);
                                    failures.push(format!("ledger settlement: {error}"));
                                    Vec::new()
                                }
                            }
                        }
                        Err(error) => {
                            log::warn!("scope-ended database lock failed: {error}");
                            failures.push(error.to_string());
                            Vec::new()
                        }
                    }; settled },
                    None => {
                        log::warn!("scope-ended cleanup has no database");
                        failures.push("aft.db is unavailable".into());
                        Vec::new()
                    }
                };
                // Drop the ledger lock before killing: terminal task writes use
                // the actor's same connection and must not deadlock on it.
                ctx.bash_background().kill_scope(&stamp);
                #[cfg(feature = "test-timing-hooks")]
                crate::slog_info!(
                    "scope diagnostic phase=revoke_finished scope_epoch={} elapsed_ms={}",
                    stamp.scope_epoch,
                    submitted_at.elapsed().as_millis()
                );
                let complete = failures.is_empty();
                let _ = result.send(ScopeRevokeResult { keys, complete });
                if complete {
                    Response::success("scope-ended", json!({}))
                } else {
                    Response::error("scope-ended", "database_unavailable", failures.join("; "))
                }
            }),
        );
        let response = await_executor_response(job, "scope-ended".into()).await;
        complete &= response.success;
        match receiver.await {
            Ok(result) => {
                keys.extend(result.keys);
                complete &= result.complete;
            }
            Err(error) => {
                complete = false;
                log::warn!("scope-ended cleanup result was lost: {error}");
            }
        }
    }
    ScopeRevokeResult { keys, complete }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_scope_settlement_stays_fenced_in_health_and_retries() {
        use crate::db::{access_grants as grants, call_ledger as ledger};
        let (dir, root) = super::super::test_support::test_root("scope-cleanup-retry");
        let ctx = super::super::test_support::test_ctx();
        ctx.update_config(|config| {
            config.project_root = Some(root.as_path().into());
            config.storage_dir = Some(dir.path().join("store"));
        });
        let db = Arc::new(StdMutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(db.clone());
        let stamp: ScopeStamp = serde_json::from_value(json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"run:cleanup","scope_epoch":7,"kind":"ephemeral","owner_authorized":true})).unwrap();
        let key = ledger::Key {
            carrier: "reserved:basal".into(),
            call_key: "waiting".into(),
        };
        let scope = ledger::ScopeIdentity {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: stamp.scope_ref.clone(),
            scope_epoch: 7,
        };
        {
            let conn = db.lock().unwrap();
            ledger::admit(&conn, &key, "digest", Some(&scope), ledger::State::Prepared).unwrap();
            conn.execute_batch("CREATE TRIGGER reject_scope_end BEFORE UPDATE OF state ON call_ledger WHEN NEW.outcome='not_started' BEGIN SELECT RAISE(ABORT, 'temporary scope cleanup failure'); END;").unwrap();
        }
        let executor = Arc::new(Executor::new());
        executor.register_actor(root, ctx.clone());
        let metrics = Arc::new(DispatchPathMetrics::new());
        let (notices, mut received) = mpsc::unbounded_channel();
        let cleanup = tokio::spawn(finish_confirmed_end(
            executor,
            stamp.clone(),
            metrics.clone(),
            notices,
        ));
        let first = tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(first.keys.is_empty());
        assert_eq!(
            metrics.scope_descriptions.lock().unwrap().cleanup_health(),
            json!({"pending_scopes":1,"failed_attempts":1})
        );
        {
            let conn = db.lock().unwrap();
            assert!(grants::scope_ended(&conn, &grants::ScopeKey::from_stamp(&stamp)).unwrap());
            assert_eq!(
                ledger::get(&conn, &key).unwrap().unwrap().state,
                ledger::State::Prepared
            );
            assert!(ctx
                .bash_background()
                .with_live_scope(&stamp, || ())
                .is_none());
            conn.execute_batch("DROP TRIGGER reject_scope_end").unwrap();
        }
        let retried = tokio::time::timeout(Duration::from_secs(10), cleanup)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(retried.keys, vec![key.clone()]);
        assert_eq!(
            metrics.scope_descriptions.lock().unwrap().cleanup_health(),
            json!({"pending_scopes":0,"failed_attempts":1})
        );
        let conn = db.lock().unwrap();
        assert_eq!(
            ledger::get(&conn, &key)
                .unwrap()
                .unwrap()
                .outcome
                .as_deref(),
            Some("not_started")
        );
        assert!(!ledger::begin_approved(&conn, &key).unwrap());
    }

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
        let census_metrics = control.metrics.clone();
        let (scopes, mut ended, worker) =
            start_with_scope_source(control, executor, Duration::from_secs(60), move || {
                std::future::ready(vec![live.clone()])
            });
        scopes.goodbye(stamp.clone());
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
        let described = census_metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .get(&stamp, Instant::now());
        assert_eq!(described["status"], "ended");
        assert_eq!(described["scope_epoch"], 10);
        assert!(described["age_ms"].is_u64());
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

async fn kill_only(executor: Arc<Executor>, stamp: &ScopeStamp) {
    for (root, _) in executor.actor_entries() {
        let stamp = stamp.clone();
        let job = executor.submit_async(
            root,
            Lane::Mutating,
            "speech-end-safety-kill".into(),
            Box::new(move |ctx| {
                ctx.bash_background().kill_scope(&stamp);
                Response::success("speech-end-safety-kill", json!({}))
            }),
        );
        let _ = await_executor_response(job, "speech-end-safety-kill".into()).await;
    }
}
