//! Ask the daemon whether a scope ended before killing its shell tasks.
//! Route closure and module drain alone do not establish scope end.

use super::*;
use crate::access_policy::prefrontal::{self, Continuation, CounterpartReply};
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
/// An unconfigured owner is not evidence of scope end.
pub(super) fn describe_answer_ends_scope(reply: &ModuleControlResponseToModule) -> bool {
    matches!(
        reply,
        ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::Ended,
            owner_configured: true,
            ..
        } | ModuleControlResponseToModule::ScopeDescribe {
            status: subc_protocol::scope::ScopeStatus::NotLive,
            owner_configured: true,
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

// The scope.watch contract bounds daemon tombstones by count, not time.
// Keep local notification deduplication for ten minutes and at most 4,096 ends.
const COMPLETED_END_TTL: Duration = Duration::from_secs(10 * 60);
const COMPLETED_END_CAP: usize = 4_096;

#[derive(Default)]
struct CompletedEnds(std::collections::VecDeque<(scope_watch::Key, tokio::time::Instant)>);

impl CompletedEnds {
    fn contains(&mut self, key: &scope_watch::Key) -> bool {
        let now = tokio::time::Instant::now();
        while self
            .0
            .front()
            .is_some_and(|(_, at)| now.duration_since(*at) >= COMPLETED_END_TTL)
        {
            self.0.pop_front();
        }
        self.0.iter().any(|(completed, _)| completed == key)
    }

    fn insert(&mut self, key: scope_watch::Key) {
        if self.contains(&key) {
            return;
        }
        if self.0.len() == COMPLETED_END_CAP {
            self.0.pop_front();
        }
        // After eviction a duplicate may send a redundant notice, but lifecycle
        // cleanup remains idempotent and cannot kill the ended task a second time.
        self.0.push_back((key, tokio::time::Instant::now()));
    }
}

#[derive(Clone)]
pub(super) struct ScopeDescribes {
    goodbye: mpsc::UnboundedSender<ScopeStamp>,
    routed: Arc<StdMutex<HashSet<(String, String, u64)>>>,
    observed: mpsc::UnboundedSender<ScopeStamp>,
    #[cfg(test)]
    iterations: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    completed: Arc<StdMutex<CompletedEnds>>,
    bound: mpsc::UnboundedSender<ScopeStamp>,
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
        crate::access_policy::prefrontal::catalog_updates(),
    )
}
impl ScopeDescribes {
    pub(super) fn routes(&self, routes: &HashMap<RouteChannel, RouteIdentity>) {
        let current: HashSet<_> = routes
            .values()
            .filter_map(|identity| identity.scope.as_ref())
            .map(stamp_key)
            .collect();
        let mut routed = self
            .routed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for stamp in routes
            .values()
            .filter_map(|identity| identity.scope.as_ref())
        {
            if stamp.owner_authorized
                && crate::access_policy::prefrontal::is_principal(&stamp.owner)
                && !routed.contains(&stamp_key(stamp))
            {
                let _ = self.bound.send(stamp.clone());
            }
        }
        *routed = current;
    }
    pub(super) fn goodbye(&self, stamp: ScopeStamp) {
        let _ = self.goodbye.send(stamp);
    }
}

pub(super) struct EndedScope {
    pub(super) stamp: ScopeStamp,
    pub(super) keys: Vec<crate::db::call_ledger::Key>,
}

fn trigger_describe(
    key: scope_watch::Key,
    stamp: ScopeStamp,
    pending: &mut HashSet<scope_watch::Key>,
    queued: &mut HashMap<scope_watch::Key, ScopeStamp>,
    retrigger: &mut HashSet<scope_watch::Key>,
) {
    if pending.insert(key.clone()) {
        queued.insert(key, stamp);
    } else {
        retrigger.insert(key);
    }
}
type ScopeReference = (String, String);
type ProbeGroup = Vec<(scope_watch::Key, ScopeStamp, bool)>;

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
        tokio::sync::watch::channel(Vec::new()).1,
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
    start_with_scope_source_impl(
        control,
        executor,
        interval,
        move || {
            let source = scope_source();
            async move {
                let mut inventory = scope_watch::Inventory::default();
                inventory.add(scope_watch::Source::Shell, source.await);
                inventory
            }
        },
        None,
        None,
        tokio::sync::watch::channel(Vec::new()).1,
    )
}

fn start_with_scope_source_impl<F, Fut>(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    interval: Duration,
    scope_source: F,
    speech: Option<Arc<crate::gh_shim_speech::service::Service>>,
    run_scopes: Option<Arc<super::run_scope::RunScopes>>,
    mut catalog: tokio::sync::watch::Receiver<Vec<String>>,
) -> (
    ScopeDescribes,
    mpsc::UnboundedReceiver<EndedScope>,
    JoinHandle<()>,
)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = scope_watch::Inventory> + Send,
{
    let (goodbye, mut closes) = mpsc::unbounded_channel();
    let (observed, mut observations) = mpsc::unbounded_channel();
    let (bound, mut bindings) = mpsc::unbounded_channel();
    let (ended, receiver) = mpsc::unbounded_channel();
    let routed = Arc::new(StdMutex::new(HashSet::new()));
    let completed = Arc::new(StdMutex::new(CompletedEnds::default()));
    #[cfg(test)]
    let iterations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = ScopeDescribes {
        goodbye,
        routed: routed.clone(),
        observed,
        #[cfg(test)]
        iterations: iterations.clone(),
        #[cfg(test)]
        completed: completed.clone(),
        bound,
    };
    if let Some(service) = &speech {
        let sender = handle.observed.clone();
        service.set_end_notice(Arc::new(move |stamp| {
            let _ = sender.send(stamp);
        }));
    }
    let worker = tokio::spawn(async move {
        let (coverage, covered) = tokio::sync::watch::channel(scope_watch::Coverage::default());
        let mut grant_sweep = Box::pin(access_grants::sweep::run(
            control.clone(),
            executor.clone(),
            covered,
            catalog.clone(),
        ));
        if !control.advertises(subc_protocol::scope::SCOPE_DESCRIBE_OP) {
            grant_sweep.await;
            return;
        }
        let mut queued: HashMap<scope_watch::Key, ScopeStamp> = HashMap::new();
        let mut retry = HashMap::new();
        let mut pending = HashSet::new();
        let mut retrigger = HashSet::new();
        let mut verification_keys = HashSet::new();
        let mut confirmed = HashSet::new();
        let mut speech_pending = HashSet::new();
        let mut cleanup_keys = HashSet::new();
        let mut continuity_holds = HashSet::new();
        let mut next_poll: HashMap<_, tokio::time::Instant> = HashMap::new();
        // Only unix refreshes the set from secret proxy authorizations.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut secret_authorizations: HashSet<(String, String, u64)> = HashSet::new();
        let mut inflight = tokio::task::JoinSet::new();
        let mut inflight_jobs: HashMap<tokio::task::Id, (ScopeReference, ProbeGroup)> =
            HashMap::new();
        let mut inflight_refs: HashSet<ScopeReference> = HashSet::new();
        let mut fanout = tokio::task::JoinSet::new();
        let mut next_scan = tokio::time::Instant::now() + interval;
        let mut next_backstop = tokio::time::Instant::now() + scope_watch::BACKSTOP;
        let mut inventory = scope_watch::Inventory::default();
        let mut inventory_revision = scope_watch::revisions();
        let mut refresh = scope_watch::Refresh::new();
        let mut verifications = scope_watch::Verifications::default();
        let mut watches = scope_watch::Watches::new(control.metrics.clone());
        let mut catalog_open = true;
        let mut catalog_seen = HashSet::new();
        loop {
            #[cfg(test)]
            iterations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let changed = scope_watch::notifier().notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if scope_watch::revisions() != inventory_revision {
                refresh.changed();
            }
            if refresh.due() {
                inventory_revision = scope_watch::revisions();
                let mut refreshed = scope_source().await;
                refreshed.retain_unread(&inventory);
                inventory = refreshed;
                refresh.at = None;
            }
            let mut held_scopes = inventory.scopes();
            for module_id in catalog.borrow_and_update().clone() {
                if !catalog_seen.insert(module_id.clone()) {
                    continue;
                }
                let owner = Principal::Reserved { module_id };
                for stamp in held_scopes.values() {
                    if let Some(other) = crate::access_policy::prefrontal::counterpart(stamp)
                        .filter(|other| other.owner == owner)
                    {
                        crate::access_policy::prefrontal::log_continuation(
                            stamp,
                            &other,
                            "catalog_reconcile",
                        );
                        trigger_describe(
                            stamp_key(stamp),
                            stamp.clone(),
                            &mut pending,
                            &mut queued,
                            &mut retrigger,
                        );
                    }
                }
            }
            speech_pending
                .retain(|key| held_scopes.contains_key(key) || cleanup_keys.contains(key));
            confirmed.retain(|key| {
                held_scopes.contains_key(key)
                    || cleanup_keys.contains(key)
                    || speech_pending.contains(key)
            });
            next_poll.retain(|key, _| held_scopes.contains_key(key) && !confirmed.contains(key));
            retry.retain(|key, _| held_scopes.contains_key(key) && !confirmed.contains(key));
            continuity_holds
                .retain(|key| held_scopes.contains_key(key) && !confirmed.contains(key));
            retrigger.retain(|key| held_scopes.contains_key(key) && !confirmed.contains(key));
            verification_keys
                .retain(|key| held_scopes.contains_key(key) && !confirmed.contains(key));
            queued.retain(|key, _| held_scopes.contains_key(key) && !confirmed.contains(key));
            let active_keys: HashSet<_> = inflight_jobs
                .values()
                .flat_map(|(_, group)| group.iter().map(|(key, _, _)| key.clone()))
                .collect();
            pending.retain(|key| queued.contains_key(key) || active_keys.contains(key));
            held_scopes.retain(|key, _| !confirmed.contains(key));
            watches.reconcile(&held_scopes, &control, interval);
            let protected = watches.coverage(held_scopes.len());
            let protected_keys = &protected.keys;
            verifications.reconcile(&watches.healthy_keys(), &held_scopes);
            if let Some(key) = verifications.take_due() {
                if let Some(stamp) = held_scopes.get(&key) {
                    verification_keys.insert(key.clone());
                    trigger_describe(
                        key,
                        stamp.clone(),
                        &mut pending,
                        &mut queued,
                        &mut retrigger,
                    );
                }
            }
            watches.health.skipped_grant_rows = inventory.skipped[0];
            watches.health.skipped_held_rows = inventory.skipped[1];
            watches.health.poll_keys = next_poll.len();
            watches.health.confirmed_cleanup = confirmed.len();
            watches.health.prefrontal_continuity_holds = continuity_holds.len();
            coverage.send_replace(protected.clone());
            control
                .metrics
                .scope_descriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .watch_health(&watches.health);
            #[cfg(feature = "test-timing-hooks")]
            if let Some(path) = std::env::var_os("AFT_TEST_SCOPE_WATCH_HEALTH_FILE") {
                let _ = std::fs::write(path, serde_json::to_vec(&watches.health).unwrap());
            }
            while inflight.len() < 4 && !queued.is_empty() {
                let Some(key) = queued
                    .keys()
                    .find(|key| !inflight_refs.contains(&(key.0.clone(), key.1.clone())))
                    .cloned()
                else {
                    break;
                };
                let stamp: ScopeStamp = queued.remove(&key).unwrap();
                if confirmed.contains(&key) {
                    pending.remove(&key);
                    continue;
                }
                // scope.describe selects owner/ref, not an epoch. One answer
                // confirms every retained epoch that existed when it was sent.
                let reference = (key.0.clone(), key.1.clone());
                inflight_refs.insert(reference.clone());
                let mut group: HashMap<_, _> = held_scopes
                    .iter()
                    .filter(|(key, _)| (key.0.clone(), key.1.clone()) == reference)
                    .map(|(key, stamp)| (key.clone(), stamp.clone()))
                    .collect();
                group.entry(key).or_insert(stamp.clone());
                let group: Vec<_> = group
                    .into_iter()
                    .map(|(key, stamp)| {
                        queued.remove(&key);
                        pending.insert(key.clone());
                        let full = inventory.sources[0].contains_key(&key)
                            || inventory.sources[3].contains_key(&key);
                        (key, stamp, full)
                    })
                    .collect();
                if group
                    .iter()
                    .any(|(key, _, _)| verification_keys.contains(key))
                {
                    watches.health.verifications += 1;
                    // One owner/ref probe also verifies its retained epochs.
                    verifications.verified(group.iter().map(|(key, _, _)| key.clone()));
                }
                for (key, _, _) in &group {
                    verification_keys.remove(key);
                }
                let control = control.clone();
                let scope_catalog = catalog.clone();
                // Persisted grants and held rows have only owner/ref/epoch,
                // not a full speech authority stamp. Their ordinary cleanup
                // still requires the daemon's definitive describe answer.
                let speech = speech.clone();
                let task_group = group.clone();
                let job = inflight.spawn(async move {
                    let sent = std::time::Instant::now();
                    let generation = speech
                        .as_ref()
                        .map(|service| service.generation())
                        .unwrap_or_default();
                    let reply = control.describe(&stamp).await;
                    for (_, stamp, full) in &task_group {
                        control
                            .metrics
                            .scope_descriptions
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .record(&stamp, reply.as_ref().ok(), Instant::now());
                        if let Some(service) = speech.as_ref().filter(|_| *full) {
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
                    }
                    // Configuration can change while the original probe is in
                    // flight, so consult the latest catalog after its answer.
                    let probe_counterpart = reply.as_ref().is_ok_and(|reply| {
                        describe_answer_ends_scope(reply)
                            || crate::access_policy::prefrontal::retired(&stamp, reply)
                    })
                        && crate::access_policy::prefrontal::counterpart_in_catalog(
                            &stamp,
                            scope_catalog.borrow().as_slice(),
                        );
                    let counterpart_reply = if probe_counterpart {
                        match crate::access_policy::prefrontal::counterpart(&stamp) {
                            Some(other) => Some(control.describe(&other).await),
                            None => None,
                        }
                    } else {
                        None
                    };
                    (reply, generation, counterpart_reply)
                });
                inflight_jobs.insert(job.id(), (reference, group));
            }
            let routed_now = routed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            // A reopened watch can report a retired owner's stale scope as
            // live. Keep describing held scopes even after watch coverage resumes.
            let wake_at = next_poll
                .iter()
                .filter(|(key, _)| {
                    !confirmed.contains(*key)
                        && !pending.contains(*key)
                        && (!protected_keys.contains(*key) || continuity_holds.contains(*key))
                        && (!routed_now.contains(*key)
                            || secret_authorizations.contains(*key)
                            || retry.contains_key(*key))
                })
                .map(|(_, due)| *due)
                .min()
                .map_or(next_scan, |due| due.min(next_scan));
            let wake_at = if protected.all && continuity_holds.is_empty() {
                next_backstop
            } else {
                wake_at.min(next_backstop)
            };
            let wake_at = watches
                .next_retry()
                .map_or(wake_at, |retry| retry.min(wake_at));
            let wake_at = verifications
                .deadline()
                .map_or(wake_at, |due| due.min(wake_at));
            let wake_at = refresh.at.map_or(wake_at, |due| due.min(wake_at));
            tokio::select! {
                () = &mut changed => { refresh.changed(); }
                result = catalog.changed(), if catalog_open => {
                    catalog_open = result.is_ok();
                }
                Some(stamp) = bindings.recv() => {
                    if let Some(other) = crate::access_policy::prefrontal::counterpart(&stamp) {
                        match rehome_shell_scopes(executor.clone(), other.clone(), stamp.clone()).await {
                            Ok(count) => crate::access_policy::prefrontal::log_continuation(&other, &stamp, if count == 0 { "bind_no_tracked_tasks" } else { "bind_continuation" }),
                            Err(error) => {
                                crate::access_policy::prefrontal::log_continuation(&other, &stamp, "bind_hold_persistence_failed");
                                log::warn!("prefrontal scope tracking rehome failed: {error}");
                                trigger_describe(stamp_key(&other), other, &mut pending, &mut queued, &mut retrigger);
                            }
                        }
                        refresh.immediate();
                    }
                }
                Some(event) = watches.receiver.recv() => {
                    for key in watches.event(event, held_scopes.len()) {
                        if let Some(stamp) = held_scopes.get(&key) {
                            if !confirmed.contains(&key) { trigger_describe(key, stamp.clone(), &mut pending, &mut queued, &mut retrigger); }
                        }
                    }
                }
                () = &mut grant_sweep => unreachable!("saved grant sweep runs until cancelled"),
                Some(stamp)=observations.recv()=>{
                    if completed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&stamp_key(&stamp)) { continue; }
                    let other_configured = crate::access_policy::prefrontal::counterpart_in_catalog(&stamp, catalog.borrow().as_slice());
                    if other_configured && !confirmed.contains(&stamp_key(&stamp)) {
                        trigger_describe(stamp_key(&stamp), stamp, &mut pending, &mut queued, &mut retrigger);
                        continue;
                    }
                    if let Some(service)=&speech { if speech_pending.remove(&stamp_key(&stamp)) || confirmed.insert(stamp_key(&stamp)) {
                        let key = stamp_key(&stamp);
                        cleanup_keys.insert(key.clone());
                        accept_confirmed_end(&executor, run_scopes.as_deref(), &control.metrics, &stamp);
                        let service=service.clone();
                        let executor=executor.clone();
                        let metrics=control.metrics.clone();
                        let notices=ended.clone();
                        fanout.spawn(async move { (key, finish_fenced_notice(executor, stamp.clone(), || service.store.scope_ended(&stamp), metrics, notices).await) });
                    } }
                }
                Some(Ok((key, notice)))=fanout.join_next(), if !fanout.is_empty()=>{
                    // Only finished cleanup is deduplicated. Scopes waiting for
                    // prefrontal's other owner to sync, or continuing under it,
                    // have no completed end to record.
                    completed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(key.clone());
                    cleanup_keys.remove(&key);
                    if let Some(notice) = notice { let _=ended.send(notice); }
                    refresh.immediate();
                }
                Some(stamp) = closes.recv() => {
                    // An idle route needs no RPC; parked consent asks still
                    // need a scope-end determination even without a process.
                    inventory_revision = scope_watch::revisions();
                    let mut current = scope_source().await;
                    current.retain_unread(&inventory);
                    inventory = current;
                    refresh.at = None;
                    let scopes: Vec<_> = inventory.scopes().into_values().collect();
                    let key = stamp_key(&stamp);
                    // Let the test daemon observe the exact Goodbye snapshot,
                    // including a close that found no registered shell task.
                    #[cfg(feature = "test-timing-hooks")]
                    if let Some(path) = std::env::var_os("AFT_TEST_SCOPE_GOODBYE_SAMPLED_FILE") {
                        let matches = scopes.iter().filter(|live| stamp_key(live) == key).count();
                        let _ = std::fs::write(path, matches.to_string());
                    }
                    if !protected_keys.contains(&key) && !confirmed.contains(&key) && scopes.iter().any(|live| stamp_key(live) == key) {
                        #[cfg(feature = "test-timing-hooks")]
                        crate::slog_info!("scope diagnostic phase=goodbye_describe_queued scope_epoch={}", stamp.scope_epoch);
                        trigger_describe(key, stamp, &mut pending, &mut queued, &mut retrigger);
                    }
                }
                _ = tokio::time::sleep_until(wake_at) => {
                    if refresh.due() { continue; }
                    if verifications.deadline().is_some_and(|due| due <= tokio::time::Instant::now()) { continue; }
                    if tokio::time::Instant::now() >= next_backstop {
                        let now = scope_watch::revisions();
                        let mut refreshed = scope_source().await;
                        refreshed.retain_unread(&inventory);
                        watches.health.drift_repairs += refreshed.warn_drift(&inventory, inventory_revision, now);
                        inventory = refreshed;
                        inventory_revision = now;
                        next_backstop = tokio::time::Instant::now() + scope_watch::BACKSTOP;
                        continue;
                    }
                    let scan_due = tokio::time::Instant::now() >= next_scan;
                    if scan_due {
                        inventory_revision = scope_watch::revisions();
                        let mut refreshed = scope_source().await;
                        refreshed.retain_unread(&inventory);
                        inventory = refreshed;
                        #[cfg(unix)]
                        {
                            let executor = executor.clone();
                            secret_authorizations = tokio::task::spawn_blocking(move || executor.actor_contexts().into_iter().flat_map(|ctx| ctx.bash_background().secret_authorization_scope_stamps()).map(|stamp| stamp_key(&stamp)).collect()).await.unwrap_or_default();
                        }
                        next_scan = tokio::time::Instant::now() + interval;
                    }
                    let scopes: Vec<_> = inventory.scopes().into_values().collect();
                    retry.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    next_poll.retain(|key, _| scopes.iter().any(|stamp| &stamp_key(stamp) == key));
                    let routed = routed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                    for stamp in scopes {
                        let key = stamp_key(&stamp);
                        if (!protected_keys.contains(&key) || continuity_holds.contains(&key)) && !confirmed.contains(&key) && !pending.contains(&key) && (!routed.contains(&key) || secret_authorizations.contains(&key) || retry.contains_key(&key))
                            && next_poll.get(&key).map_or(scan_due, |due| tokio::time::Instant::now() >= *due) {
                            // A per-scope deadline can precede the next inventory
                            // scan. Consume it when queuing the probe so no past
                            // deadline remains an immediately-ready timer.
                            next_poll.remove(&key);
                            trigger_describe(key, stamp, &mut pending, &mut queued, &mut retrigger);
                        }
                    }
                }
                Some(result) = inflight.join_next_with_id(), if !inflight.is_empty() => {
                    let (id, reply, generation, counterpart_reply) = match result {
                        Ok((id, (reply, generation, counterpart_reply))) => (id, reply, generation, counterpart_reply),
                        Err(error) => {
                            log::warn!("scope.describe task failed; retaining scope state: {error}");
                            (error.id(), Err(readiness::ControlError::Decode(error.to_string())), String::new(), None)
                        }
                    };
                    if let Some((reference, group)) = inflight_jobs.remove(&id) {
                        inflight_refs.remove(&reference);
                        watches.health.descriptions += 1;
                        for (key, stamp, full_stamp) in group {
                        if let Ok(ModuleControlResponseToModule::ScopeDescribe { status: subc_protocol::scope::ScopeStatus::Live, scope_epoch: Some(epoch), .. }) = &reply {
                            watches.rebind(&key, *epoch);
                        }
                        // A Goodbye query can finish just before the connection's
                        // periodic tick. Wait a full interval after its answer
                        // before rechecking shell tasks with that owner/ref/epoch.
                        next_poll.insert(key.clone(), tokio::time::Instant::now() + interval);
                        let source_ended = reply.as_ref().is_ok_and(describe_answer_ends_scope);
                        let mut ends = source_ended;
                        if source_ended || reply.as_ref().is_ok_and(|reply| prefrontal::retired(&stamp, reply)) {
                            if let Some(other) = prefrontal::counterpart(&stamp) {
                                let answer = match counterpart_reply.as_ref() {
                                    Some(reply) => CounterpartReply::Describe(reply.as_ref().ok()),
                                    None => CounterpartReply::NotInCatalog,
                                };
                                let decision = prefrontal::continuation(&other, answer, source_ended);
                                match decision {
                                    Continuation::Live(live) => {
                                        match rehome_shell_scopes(executor.clone(), stamp.clone(), live.clone()).await {
                                            Ok(_) => {
                                                crate::access_policy::prefrontal::log_continuation(&stamp, &live, "continuation");
                                                continuity_holds.remove(&key);
                                                retry.remove(&key);
                                                retrigger.remove(&key);
                                            }
                                            Err(error) => {
                                                crate::access_policy::prefrontal::log_continuation(&stamp, &live, "hold_persistence_failed");
                                                log::warn!("prefrontal scope tracking rehome failed: {error}");
                                                retry.insert(key.clone(), stamp.clone());
                                                watches.fallback(&key, interval);
                                            }
                                        }
                                        pending.remove(&key);
                                        refresh.immediate();
                                        continue;
                                    }
                                    Continuation::Hold(outcome) => {
                                        prefrontal::log_continuation(&stamp, &other, outcome);
                                        continuity_holds.insert(key.clone());
                                        retry.insert(key.clone(), stamp.clone());
                                        watches.fallback(&key, interval);
                                        pending.remove(&key);
                                        continue;
                                    }
                                    Continuation::End(outcome) => {
                                        prefrontal::log_continuation(&stamp, &other, outcome);
                                        ends = true;
                                    }
                                }
                            }
                        } else if let Some(other) = prefrontal::counterpart(&stamp) {
                            prefrontal::log_continuation(&stamp, &other, prefrontal::retained_outcome(reply.as_ref().ok()));
                        }
                        #[cfg(unix)]
                        if reply.as_ref().is_ok_and(|answer| secret_authorization_rebound(&stamp, answer)) {
                            let executor = executor.clone();
                            let described = stamp.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                for ctx in executor.actor_contexts() { ctx.bash_background().revoke_secret_authorization(&described); }
                            }).await;
                        }
                        let strict_end = speech.as_ref().filter(|_| full_stamp && source_ended).and_then(|service| {
                            let role=if stamp.attributes.run_id.is_some(){crate::gh_shim_speech::ScopeRole::Run}else{crate::gh_shim_speech::ScopeRole::Authority};
                            reply.as_ref().ok().and_then(|reply| match crate::gh_shim_speech::scope::validate_scope_identity(&stamp,role,reply,&service.generation(),&generation,crate::db::call_ledger::now_ms() as u64) {
                                Ok(crate::gh_shim_speech::scope::Answer::Ended(proof)) => Some((service.clone(), proof)),
                                _ => None,
                            })
                        });
                        if ends {
                            #[cfg(feature = "test-timing-hooks")]
                            crate::slog_info!("scope diagnostic phase=describe_ended scope_epoch={}", stamp.scope_epoch);
                            if confirmed.insert(key.clone()) {
                                accept_confirmed_end(&executor, run_scopes.as_deref(), &control.metrics, &stamp);
                                if strict_end.is_some() {
                                    speech_pending.insert(key.clone());
                                } else {
                                    let executor=executor.clone();
                                    let metrics=control.metrics.clone();
                                    let notices=ended.clone();
                                    let stamp=stamp.clone();
                                    cleanup_keys.insert(key.clone());
                                    let cleanup_key = key.clone();
                                    fanout.spawn(async move { (cleanup_key, finish_confirmed_end(executor, stamp, metrics, notices).await) });
                                }
                            }
                            retry.remove(&key);
                        } else if !matches!(reply, Ok(ModuleControlResponseToModule::ScopeDescribe { status: subc_protocol::scope::ScopeStatus::Live, .. })) {
                            retry.insert(key.clone(), stamp.clone());
                        } else {
                            retry.remove(&key);
                            continuity_holds.remove(&key);
                        }
                        // Exact speech proofs retain persistence-before-ledger
                        // cleanup ordering. If a ref moved to a newer live
                        // epoch, its old stamp cannot mint that proof; a later
                        // definitive ref end still closes its ordinary state.
                        if let Some((service, proof)) = strict_end {
                                if let Err(error)=service.record_end(&stamp,proof){
                                    log::warn!("speech scope-end persistence failed scope_ref={:?} scope_epoch={} code={:?} reason={}", stamp.scope_ref, stamp.scope_epoch, error.code, error.reason);
                                    control.metrics.scope_descriptions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).cleanup_result(&stamp, false);
                                    kill_only(executor.clone(),&stamp).await;
                                }
                        }
                        if retry.contains_key(&key) { watches.fallback(&key, interval); }
                        pending.remove(&key);
                        // A terminal push is sent only once. If it raced this
                        // probe, a Live or failed answer may predate the end.
                        if retrigger.remove(&key) && !confirmed.contains(&key) {
                            if let Some(current) = held_scopes.get(&key) {
                                trigger_describe(key, current.clone(), &mut pending, &mut queued, &mut retrigger);
                            }
                        }
                        }
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
#[cfg(test)]
#[path = "scope_watch_tests.rs"]
mod watch_tests;

async fn work_scopes(
    executor: Arc<Executor>,
    run_scopes: Arc<super::run_scope::RunScopes>,
) -> scope_watch::Inventory {
    tokio::task::spawn_blocking(move || {
        let mut scopes = scope_watch::Inventory::default();
        scopes.add(
            scope_watch::Source::Shell,
            executor
                .actor_contexts()
                .into_iter()
                .flat_map(|ctx| ctx.bash_background().live_scope_stamps())
                .collect::<Vec<_>>(),
        );
        scopes.add(scope_watch::Source::Parked, run_scopes.parked_scopes());
        let mut seen = HashSet::new();
        for ctx in executor.actor_contexts() {
            let Some(db) = ctx.db() else { continue };
            if !seen.insert(Arc::as_ptr(&db)) {
                continue;
            }
            let result = (|| -> Result<(), String> {
                let conn = db
                    .lock()
                    .map_err(|_| "database lock poisoned".to_string())?;
                match crate::db::access_grants::saved_scopes(&conn) {
                    Ok(keys) => {
                        for key in keys {
                            match key
                                .ok()
                                .and_then(|key| access_grants::sweep::stamp(&key).ok())
                            {
                                Some(stamp) => scopes.add(scope_watch::Source::Grants, [stamp]),
                                None => scopes.skipped[0] += 1,
                            }
                        }
                    }
                    Err(error) => {
                        scopes.failed.insert(1);
                        log::warn!(
                            "scope.watch selector census read failed source=grants: {error}"
                        );
                    }
                }
                let keys = match crate::db::call_ledger::unsettled_scopes(&conn) {
                    Ok(keys) => keys,
                    Err(error) => {
                        scopes.failed.insert(2);
                        log::warn!("scope.watch selector census read failed source=held: {error}");
                        return Ok(());
                    }
                };
                for key in keys {
                    let Ok(key) = key else {
                        scopes.skipped[1] += 1;
                        continue;
                    };
                    let owner = match key.owner.as_str() {
                        "direct" => subc_protocol::Principal::Direct,
                        "unverified" => subc_protocol::Principal::Unverified,
                        other => subc_protocol::Principal::Reserved {
                            module_id: match other
                                .strip_prefix("reserved:")
                                .filter(|module| !module.is_empty())
                            {
                                Some(module) => module.into(),
                                None => {
                                    scopes.skipped[1] += 1;
                                    continue;
                                }
                            },
                        },
                    };
                    scopes.add(
                        scope_watch::Source::Held,
                        [ScopeStamp::new(
                            owner,
                            key.scope_ref,
                            key.scope_epoch,
                            subc_protocol::scope::ScopeKind::Worker,
                        )],
                    );
                }
                Ok(())
            })();
            if let Err(error) = result {
                scopes.failed.extend([1, 2]);
                log::warn!("scope.watch selector census read failed source=grants,held: {error}");
            }
        }
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

async fn rehome_shell_scopes(
    executor: Arc<Executor>,
    from: ScopeStamp,
    to: ScopeStamp,
) -> Result<usize, String> {
    tokio::task::spawn_blocking(move || {
        let mut changed = 0;
        for ctx in executor.actor_contexts() {
            changed += ctx
                .bash_background()
                .rehome_scope(&from, &to)
                .map_err(|error| error.to_string())?;
        }
        Ok(changed)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn fence_confirmed_end(executor: &Executor, stamp: &ScopeStamp) {
    for ctx in executor.actor_contexts() {
        ctx.bash_background().fence_scope_end(stamp);
    }
}

fn accept_confirmed_end(
    executor: &Executor,
    runs: Option<&super::run_scope::RunScopes>,
    metrics: &DispatchPathMetrics,
    stamp: &ScopeStamp,
) {
    metrics
        .scope_descriptions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .cleanup_started(stamp);
    if let Some(runs) = runs {
        runs.revoke(stamp);
    }
    fence_confirmed_end(executor, stamp);
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
    // A confirmed end remains pending through temporary database failures.
    // Cap retry delays at five seconds even when watch coverage is complete.
    let mut attempt = 0u32;
    loop {
        let delay = match attempt {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 5,
        };
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
        attempt = attempt.saturating_add(1);
    }
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
    #[cfg(unix)]
    let mut pending_kills = Vec::new();
    for (root, _) in entries {
        #[cfg(feature = "test-timing-hooks")]
        let submitted_at = std::time::Instant::now();
        let (result, receiver) = oneshot::channel();
        // The database part runs on the call-ledger lane, where keyed
        // admissions also run, one job at a time in submission order. Every
        // admission queued on this root before the scope ended has therefore
        // committed its row by now, so `end_scope` settles it as not started
        // instead of leaving a held row that a later approval could dispatch.
        let ledger_stamp = stamp.clone();
        let job = executor.submit_async(
            root.clone(),
            Lane::CallLedger,
            "scope-ended".into(),
            Box::new(move |ctx| {
                let stamp = ledger_stamp;
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
                                &crate::db::access_grants::ScopeKey::for_grants(&stamp),
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
        // Kill the scope's shell tasks after the ledger work, as before, even
        // when that work failed. This stays a separate job on the mutating
        // lane because it changes task state, not the call ledger. The ledger
        // job has released the database lock by now, which matters: terminal
        // task writes use the same connection and must not deadlock on it.
        let kill_stamp = stamp.clone();
        let kill = executor.submit_async(
            root,
            Lane::Mutating,
            "scope-ended-kill".into(),
            Box::new(move |ctx| {
                ctx.bash_background().kill_scope(&kill_stamp);
                #[cfg(feature = "test-timing-hooks")]
                crate::slog_info!(
                    "scope diagnostic phase=revoke_finished scope_epoch={} elapsed_ms={}",
                    kill_stamp.scope_epoch,
                    submitted_at.elapsed().as_millis()
                );
                Response::success("scope-ended-kill", json!({}))
            }),
        );
        #[cfg(unix)]
        pending_kills.push(kill);
        #[cfg(not(unix))]
        {
            complete &= await_executor_response(kill, "scope-ended-kill".into())
                .await
                .success;
        }
    }
    // Submit every root's kill before waiting: different actor lanes can
    // give their command groups the grace period concurrently.
    #[cfg(unix)]
    for kill in pending_kills {
        complete &= await_executor_response(kill, "scope-ended-kill".into())
            .await
            .success;
    }
    ScopeRevokeResult { keys, complete }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held keyed call admitted for a scope just before that scope ends must
    /// not keep a Prepared row once the end is processed: an approval could
    /// then still move it to DispatchStarted. The admission is queued first on
    /// the call-ledger lane (held up by another job there), then the scope
    /// ends, so the end's ledger work must run after the admission commits.
    #[tokio::test]
    async fn scope_end_settles_a_keyed_admission_queued_before_it() {
        use crate::db::call_ledger::{self as ledger, State};
        use tool_provider::{LedgerEdge, RouteRole};
        let (dir, root) = test_support::test_root("scope-end-after-admission");
        let ctx = test_support::test_ctx();
        let db = Arc::new(StdMutex::new(
            crate::db::open(&dir.path().join("aft.db")).unwrap(),
        ));
        ctx.set_db(db.clone());
        let executor = Arc::new(Executor::new());
        assert!(executor.register_actor(root.clone(), ctx.clone()));
        let stamp: ScopeStamp = serde_json::from_value(json!({
            "owner":{"kind":"reserved","module_id":"prefrontal-core"},
            "ref":"run:ended-after-admission", "scope_epoch":4,
            "kind":"ephemeral", "owner_authorized":true
        }))
        .unwrap();
        // An untrusted, elicitation-capable route: its keyed bash call is held
        // for permission, so admission records it as Prepared.
        let mut identity = test_support::route_identity(&root, "scoped-runner");
        let data = Arc::get_mut(&mut identity.0).unwrap();
        data.role = RouteRole::ToolProviderV1;
        data.trust = BindTrust::Untrusted;
        data.consumer_elicitation_capable = true;
        data.scope = Some(stamp.clone());
        let key = LedgerEdge::key(&identity, "held-before-end");
        let routes = HashMap::from([(route_key(42, 1), identity)]);

        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = executor.submit_async(
            root.clone(),
            Lane::CallLedger,
            "held-call-ledger-lane".into(),
            Box::new(move |_| {
                let _ = started_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
                Response::success("held-call-ledger-lane", json!({}))
            }),
        );
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("the call-ledger holder never started")
            .unwrap();

        let body = json!({
            "name":"bash", "arguments":{"command":"true"},
            "call_key":"held-before-end", "preset":"head"
        });
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                42,
                1,
                7,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
            phase_trace: crate::run_tool_call::PhaseTrace::new(Instant::now()),
        };
        let (edge, mut ready) = LedgerEdge::new();
        let (writer, _replies) = mpsc::channel(8);
        assert!(edge
            .defer(
                decoded,
                &routes,
                &executor,
                &writer,
                &Arc::new(DispatchPathMetrics::new())
            )
            .is_ok());
        assert_eq!(
            executor.queued_lane_for_test(&root, "ledger-42-7"),
            Some(Lane::CallLedger),
            "the admission must be queued behind the holder"
        );

        let revoking = tokio::spawn({
            let executor = executor.clone();
            let stamp = stamp.clone();
            async move { revoke(executor, &stamp).await }
        });
        // Let the end queue its ledger work, or finish if it does not wait
        // for the call-ledger lane, before the admission is released.
        tokio::time::timeout(Duration::from_secs(5), async {
            while executor
                .queued_lane_for_test(&root, "scope-ended")
                .is_none()
                && !revoking.is_finished()
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the scope end neither queued its ledger work nor finished");
        let _ = release_tx.send(());
        holder.await.unwrap();
        let revoked = tokio::time::timeout(Duration::from_secs(10), revoking)
            .await
            .expect("the scope end did not finish")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), ready.recv())
            .await
            .expect("the admission did not finish")
            .unwrap();

        let conn = db.lock().unwrap();
        let row = ledger::get(&conn, &key).unwrap().unwrap();
        assert_eq!(
            row.state,
            State::Settled,
            "the admission committed after the scope end and left a row an approval could dispatch"
        );
        assert_eq!(row.outcome.as_deref(), Some("not_started"));
        let recorded: Value = serde_json::from_slice(
            &row.frame
                .as_ref()
                .expect("a settled row keeps its frame")
                .body,
        )
        .unwrap();
        assert_eq!(recorded["detail"]["reason"], "scope_ended");
        assert!(revoked.complete);
        assert_eq!(revoked.keys, vec![key.clone()]);
        assert!(
            !ledger::transition(&conn, &key, State::Prepared, State::Authorized).unwrap(),
            "an approval must not be able to advance the settled row"
        );
        assert!(!ledger::begin_approved(&conn, &key).unwrap());
    }

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
        let started_at = tokio::time::Instant::now();
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
        assert_eq!(answered_at, started_at + Duration::from_millis(400));
        tokio::time::advance(Duration::from_millis(100)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(399)).await;
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
        assert_eq!(
            tokio::time::Instant::now(),
            started_at + Duration::from_millis(900)
        );
        assert_eq!(repeated.header.ty, FrameType::Request);
        let body: serde_json::Value = serde_json::from_slice(&repeated.body).unwrap();
        assert_eq!(body["op"], "scope.describe");
        assert_eq!(body["ref"], stamp.scope_ref);
        assert_ne!(repeated.header.corr, first.header.corr);
        assert!(ended.try_recv().is_err());
        let before = scopes.iterations.load(std::sync::atomic::Ordering::Relaxed);
        tokio::time::advance(Duration::from_millis(99)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        let after = scopes.iterations.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after - before <= 16,
            "scope loop spun before the next scan: {} iterations",
            after - before
        );
        assert!(
            frames.try_recv().is_err(),
            "one due deadline queues one poll"
        );
        assert!(
            ended.try_recv().is_err(),
            "polling cannot establish scope end"
        );
        assert!(scopes.completed.lock().unwrap().0.is_empty());
        worker.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn completed_scope_ends_expire_and_evict_oldest_at_capacity() {
        let key = |i| ("direct".to_owned(), format!("scope-{i}"), 1);
        let mut completed = CompletedEnds::default();
        for i in 0..COMPLETED_END_CAP {
            completed.insert(key(i));
        }
        assert!(completed.contains(&key(0)));
        completed.insert(key(COMPLETED_END_CAP));
        assert_eq!(completed.0.len(), COMPLETED_END_CAP);
        assert!(!completed.contains(&key(0)));
        assert!(completed.contains(&key(1)));
        tokio::time::advance(COMPLETED_END_TTL).await;
        assert!(!completed.contains(&key(1)));
        assert!(completed.0.is_empty());
    }
}

async fn kill_only(executor: Arc<Executor>, stamp: &ScopeStamp) {
    // This fallback may wait behind actor work, but authority fencing must not.
    fence_confirmed_end(&executor, stamp);
    #[cfg(unix)]
    let mut pending_kills = Vec::new();
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
        #[cfg(unix)]
        pending_kills.push(job);
        #[cfg(not(unix))]
        let _ = await_executor_response(job, "speech-end-safety-kill".into()).await;
    }
    #[cfg(unix)]
    for job in pending_kills {
        let _ = await_executor_response(job, "speech-end-safety-kill".into()).await;
    }
}
