//! Read-only route ownership diagnostics for management, never lifecycle policy.

use super::*;
use std::collections::BTreeMap;
use subc_protocol::{scope::ScopeStamp, session::ModuleControlResponseToModule};

const ROOT_LIMIT: usize = 256;
const ROUTE_LIMIT: usize = 32;
const DESCRIPTION_LIMIT: usize = 1024;
const PRINCIPAL_LIMIT: usize = 16;
const IDLE_THRESHOLD: Duration = Duration::from_secs(30 * 60);
type ScopeKey = (String, String, u64);

#[derive(Debug)]
pub(super) struct RouteActivity {
    bound_at: Instant,
    pub(super) last_tool_call: Option<Instant>,
}

impl RouteActivity {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            bound_at: now,
            last_tool_call: None,
        }
    }
}

pub(super) fn note_tool_call(meta: &mut RootMeta, route: RouteChannel, now: Instant) {
    meta.last_tool_call = Some(now);
    if let Some(activity) = meta.route_activity.get_mut(&route) {
        activity.last_tool_call = Some(now);
    }
}

#[derive(Default)]
pub(super) struct ScopeDescriptions {
    entries: HashMap<ScopeKey, (Instant, Value)>,
    order: VecDeque<ScopeKey>,
    evicted: u64,
    cleanup_pending: HashSet<ScopeKey>,
    cleanup_failed_attempts: u64,
}

impl ScopeDescriptions {
    pub(super) fn cleanup_started(&mut self, stamp: &ScopeStamp) {
        self.cleanup_pending
            .insert(scope_describe::stamp_key(stamp));
    }

    pub(super) fn cleanup_result(&mut self, stamp: &ScopeStamp, complete: bool) {
        let key = scope_describe::stamp_key(stamp);
        if complete {
            self.cleanup_pending.remove(&key);
        } else {
            self.cleanup_pending.insert(key);
            self.cleanup_failed_attempts = self.cleanup_failed_attempts.saturating_add(1);
        }
    }

    pub(super) fn cleanup_health(&self) -> Value {
        json!({"pending_scopes": self.cleanup_pending.len(), "failed_attempts": self.cleanup_failed_attempts})
    }

    pub(super) fn record(
        &mut self,
        stamp: &ScopeStamp,
        reply: Option<&ModuleControlResponseToModule>,
        now: Instant,
    ) {
        let key = scope_describe::stamp_key(stamp);
        // Keep only protocol status and identity metadata, never the returned scope payload or error text.
        let value = match reply {
            Some(ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch,
                owner_synced,
                owner_configured,
                ..
            }) => json!({
                "status": status, "scope_epoch": scope_epoch,
                "owner_synced": owner_synced, "owner_configured": owner_configured,
            }),
            _ => json!({"status": "error"}),
        };
        self.order.retain(|existing| existing != &key);
        if self.entries.len() == DESCRIPTION_LIMIT && !self.entries.contains_key(&key) {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
                self.evicted += 1;
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, (now, value));
    }

    pub(super) fn get(&self, stamp: &ScopeStamp, now: Instant) -> Value {
        self.entries
            .get(&scope_describe::stamp_key(stamp))
            .map_or(Value::Null, |(at, value)| {
                let mut value = value.clone();
                value["age_ms"] = json!(age(now, *at));
                value["cleanup_pending"] = json!(self
                    .cleanup_pending
                    .contains(&scope_describe::stamp_key(stamp)));
                value
            })
    }
}

fn age(now: Instant, at: Instant) -> u64 {
    now.saturating_duration_since(at)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(super) struct Snapshot {
    roots: Vec<RootRow>,
    summary: Value,
    roots_omitted: usize,
    #[cfg(test)]
    routes_examined: usize,
}

struct RootRow {
    root: ProjectRootId,
    route_count: usize,
    last_tool_call_age_ms: Option<u64>,
    all_routes_idle_over_30m: bool,
    has_ended_scope: bool,
    routes: Vec<RouteRow>,
}

struct RouteRow {
    value: Value,
    harness: String,
    session: String,
    scope: Option<ScopeKey>,
}

#[derive(Default)]
struct TaskPins {
    running: usize,
    watches: usize,
    age_ms: Option<u64>,
}

#[derive(Default)]
struct Counts {
    routes: usize,
    roots: HashSet<ProjectRootId>,
}

impl Counts {
    fn note(&mut self, root: &ProjectRootId) {
        self.routes += 1;
        self.roots.insert(root.clone());
    }

    fn value(&self) -> Value {
        json!({"routes": self.routes, "roots": self.roots.len()})
    }
}

#[derive(Default)]
struct PrincipalCounts {
    all: Counts,
    scoped: Counts,
    unscoped: Counts,
}

pub(super) fn snapshot(
    live_roots: &HashMap<ProjectRootId, RootMeta>,
    root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    active: &ActiveToolCalls,
    executor: &Executor,
    metrics: &DispatchPathMetrics,
    now: Instant,
) -> Snapshot {
    let descriptions = metrics
        .scope_descriptions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut inflight = HashMap::<RouteChannel, usize>::new();
    let read_requests = executor.try_running_read_requests();
    let mut read_leases = HashMap::<RouteChannel, usize>::new();
    let mut requests: HashSet<_> = metrics.in_flight_route_requests().into_iter().collect();
    requests.extend(
        active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied(),
    );
    for (route, corr) in requests {
        *inflight.entry(route).or_default() += 1;
        if read_requests
            .as_ref()
            .is_some_and(|requests| requests.contains(&format!("subc-{}-{corr}", route.channel)))
        {
            *read_leases.entry(route).or_default() += 1;
        }
    }
    let mut bound: Vec<_> = root_channels
        .iter()
        .filter(|(_, channels)| !channels.is_empty())
        .collect();
    bound.sort_unstable_by(|a, b| a.0.as_path().cmp(b.0.as_path()));
    let root_count = bound.len();
    let mut rows = Vec::new();
    let mut idle_roots = 0;
    let mut ended_roots = 0;
    let mut route_count = 0;
    #[cfg(test)]
    let mut routes_examined = 0;
    let mut scoped = Counts::default();
    let mut unscoped = Counts::default();
    let mut principals = BTreeMap::<String, PrincipalCounts>::new();
    for (root, channels) in bound {
        let meta = live_roots.get(root);
        let mut all_idle = true;
        let mut seen_ended = false;
        let mut route_rows = Vec::new();
        let mut channels: Vec<_> = channels.iter().collect();
        channels.sort_unstable_by_key(|route| (route.channel, route.epoch));
        route_count += channels.len();
        for channel in &channels {
            #[cfg(test)]
            {
                routes_examined += 1;
            }
            let Some(identity) = routes.get(channel) else {
                all_idle = false;
                continue;
            };
            let principal = match &identity.spawn_principal {
                AuthenticatedPrincipal::RouteBind { principal_id, .. } => {
                    principal_id.as_deref().unwrap_or("absent")
                }
                AuthenticatedPrincipal::FirstParty => "first_party",
            };
            let counts = principals.entry(principal.to_string()).or_default();
            counts.all.note(root);
            if identity.scope.is_some() {
                scoped.note(root);
                counts.scoped.note(root);
            } else {
                unscoped.note(root);
                counts.unscoped.note(root);
            }
            let activity = meta.and_then(|meta| meta.route_activity.get(channel));
            let idle_since =
                activity.map(|activity| activity.last_tool_call.unwrap_or(activity.bound_at));
            all_idle &=
                idle_since.is_some_and(|at| now.saturating_duration_since(at) > IDLE_THRESHOLD);
            let described = identity
                .scope
                .as_ref()
                .map_or(Value::Null, |scope| descriptions.get(scope, now));
            seen_ended |= described["status"] == "ended";
            if rows.len() < ROOT_LIMIT && route_rows.len() < ROUTE_LIMIT {
                route_rows.push(RouteRow { value: json!({
                    "channel": channel.channel, "epoch": channel.epoch,
                    "harness": identity.harness, "session_id": identity.session,
                    "principal": principal,
                    "scope": identity.scope.as_ref().map_or(json!("none"), |stamp| json!({
                        "owner": principal_id(&Some(stamp.owner.clone())), "ref": stamp.scope_ref, "epoch": stamp.scope_epoch
                    })),
                    "bind_age_ms": activity.map(|activity| age(now, activity.bound_at)),
                    "last_tool_call_age_ms": activity.and_then(|activity| activity.last_tool_call).map(|at| age(now, at)),
                    "idle_age_ms": idle_since.map(|at| age(now, at)),
                    "scope_describe": described,
                    "pins": { "in_flight_call": inflight.get(channel).copied().unwrap_or(0),
                        "read_lease": read_requests.as_ref().map(|_| read_leases.get(channel).copied().unwrap_or(0)) },
                }), harness: identity.harness.clone(), session: identity.session.clone(),
                    scope: identity.scope.as_ref().map(scope_describe::stamp_key) });
            }
        }
        idle_roots += usize::from(all_idle);
        ended_roots += usize::from(seen_ended);
        if rows.len() < ROOT_LIMIT {
            rows.push(RootRow {
                root: root.clone(),
                route_count: channels.len(),
                routes: route_rows,
                all_routes_idle_over_30m: all_idle,
                has_ended_scope: seen_ended,
                last_tool_call_age_ms: meta
                    .and_then(|meta| meta.last_tool_call)
                    .map(|at| age(now, at)),
            });
        }
    }
    let mut by_principal = Vec::new();
    let mut omitted_principals = 0;
    let mut omitted_routes = 0;
    let mut omitted_roots = HashSet::new();
    for (principal, counts) in principals {
        if by_principal.len() < PRINCIPAL_LIMIT {
            by_principal.push(json!({"principal": principal, "bound": counts.all.value(),
                "scoped": counts.scoped.value(), "unscoped": counts.unscoped.value()}));
        } else {
            omitted_principals += 1;
            omitted_routes += counts.all.routes;
            omitted_roots.extend(counts.all.roots);
        }
    }
    Snapshot {
        #[cfg(test)]
        routes_examined,
        roots_omitted: root_count.saturating_sub(rows.len()),
        roots: rows,
        summary: json!({"bound_roots": root_count, "bound_routes": route_count,
            "all_routes_idle_over_30m_roots": idle_roots, "roots_with_ended_scopes": ended_roots,
            "scope_describe_cache_evicted": descriptions.evicted,
            "scope_cleanup": descriptions.cleanup_health(),
            "scoped": scoped.value(), "unscoped": unscoped.value(), "by_principal": by_principal,
            "principal_limit": PRINCIPAL_LIMIT, "principals_omitted": omitted_principals,
            "omitted_principal_routes": omitted_routes, "omitted_principal_roots": omitted_roots.len()}),
    }
}

impl Snapshot {
    pub(super) fn summary(&self) -> Value {
        self.summary.clone()
    }

    pub(super) fn render(self, executor: &Executor) -> Value {
        let contexts = executor
            .try_actor_entries()
            .map(|entries| entries.into_iter().collect::<HashMap<_, _>>());
        let roots: Vec<_> = self
            .roots
            .into_iter()
            .map(|row| {
                // One task-registry scan per root, not one scan per route. Missing or
                // contended state is unknown rather than a false zero-pin claim.
                let tasks = contexts
                    .as_ref()
                    .and_then(|contexts| contexts.get(&row.root))
                    .and_then(|ctx| ctx.bash_background().try_route_census());
                let mut pins =
                    HashMap::<(Option<String>, String, Option<ScopeKey>), TaskPins>::new();
                if let Some(tasks) = &tasks {
                    for task in tasks {
                        let entry = pins
                            .entry((
                                task.harness.clone(),
                                task.session_id.clone(),
                                task.scope.as_ref().map(scope_describe::stamp_key),
                            ))
                            .or_default();
                        entry.running += usize::from(task.running);
                        entry.watches += task.active_watches;
                        entry.age_ms =
                            Some(entry.age_ms.map_or(task.last_work_age_ms, |age| {
                                age.min(task.last_work_age_ms)
                            }));
                    }
                }
                let routes: Vec<_> = row
                    .routes
                    .into_iter()
                    .map(|mut route| {
                        let mut running = 0;
                        let mut watches = 0;
                        let mut last_work: Option<u64> = None;
                        for harness in [Some(route.harness), None] {
                            if let Some(pin) =
                                pins.get(&(harness, route.session.clone(), route.scope.clone()))
                            {
                                running += pin.running;
                                watches += pin.watches;
                                if let Some(age) = pin.age_ms {
                                    last_work =
                                        Some(last_work.map_or(age, |previous| previous.min(age)));
                                }
                            }
                        }
                        route.value["pins"]["running_bash"] =
                            json!(tasks.as_ref().map(|_| running));
                        route.value["pins"]["active_watch"] =
                            json!(tasks.as_ref().map(|_| watches));
                        route.value["last_background_work_age_ms"] = json!(last_work);
                        route.value
                    })
                    .collect();
            json!({"root_id": row.root.as_path(), "route_count": row.route_count,
                "last_tool_call_age_ms": row.last_tool_call_age_ms,
                "all_routes_idle_over_30m": row.all_routes_idle_over_30m, "has_ended_scope": row.has_ended_scope,
                "routes_omitted": row.route_count.saturating_sub(routes.len()), "routes": routes})
            })
            .collect();
        json!({"summary": self.summary, "root_limit": ROOT_LIMIT, "route_limit_per_root": ROUTE_LIMIT,
            "bash_pin_attribution": "harness_session_scope", "read_lease_basis": "executor_running_read_calls",
            "background_work_basis": "retained_bash_tasks_running_or_finished",
            "roots_omitted": self.roots_omitted, "roots": roots})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fixture {
        roots: HashMap<ProjectRootId, RootMeta>,
        channels: HashMap<ProjectRootId, HashSet<RouteChannel>>,
        routes: HashMap<RouteChannel, RouteIdentity>,
        active: ActiveToolCalls,
    }

    impl Fixture {
        fn bind(
            &mut self,
            root: &ProjectRootId,
            channel: u16,
            scoped: bool,
            principal: &str,
            at: Instant,
        ) {
            let mut identity = test_support::route_identity(root, "head-session");
            let data = Arc::get_mut(&mut identity.0).unwrap();
            if scoped {
                data.scope = Some(ScopeStamp::new(
                    Principal::Direct,
                    "scope-id",
                    7,
                    subc_protocol::scope::ScopeKind::Head,
                ));
            }
            if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
                &mut data.spawn_principal
            {
                *principal_id = Some(principal.into());
            }
            let route = route_key(channel, 1);
            insert_route_channel(&mut self.routes, &mut self.channels, route, identity);
            self.roots
                .entry(root.clone())
                .or_insert_with(|| RootMeta::new(at))
                .route_activity
                .insert(route, RouteActivity::new(at));
        }

        fn snapshot(
            &self,
            executor: &Executor,
            metrics: &DispatchPathMetrics,
            at: Instant,
        ) -> Snapshot {
            snapshot(
                &self.roots,
                &self.channels,
                &self.routes,
                &self.active,
                executor,
                metrics,
                at,
            )
        }
    }

    #[test]
    fn route_census_demand_clock_ignores_health_and_resets_only_on_admitted_call() {
        let dir = tempfile::tempdir().unwrap();
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let now = Instant::now();
        let bound = now - Duration::from_secs(4000);
        let mut fixture = Fixture::default();
        let metrics = DispatchPathMetrics::new();
        let executor = Executor::new();
        fixture.bind(&root, 1, false, "direct", bound);
        let before = fixture.snapshot(&executor, &metrics, now).render(&executor);
        assert_eq!(before["summary"]["all_routes_idle_over_30m_roots"], 1);
        assert_eq!(before["roots"][0]["routes"][0]["bind_age_ms"], 4_000_000);
        assert!(before["roots"][0]["routes"][0]["last_tool_call_age_ms"].is_null());
        note_tool_call(fixture.roots.get_mut(&root).unwrap(), route_key(1, 1), now);
        let later = now + Duration::from_secs(60);
        let mut report = build_health_report(
            &HealthRollupCache::new(),
            &executor,
            &HashMap::new(),
            &metrics,
            &App::default_shared(),
        );
        health::with_route_ownership_summary(
            &mut report,
            fixture.snapshot(&executor, &metrics, later).summary(),
        );
        let polled = fixture
            .snapshot(&executor, &metrics, later)
            .render(&executor);
        assert_eq!(polled["roots"][0]["last_tool_call_age_ms"], 60_000);
        assert_eq!(
            polled["roots"][0]["routes"][0]["last_tool_call_age_ms"],
            60_000
        );
        assert_eq!(polled["summary"]["all_routes_idle_over_30m_roots"], 0);
        note_tool_call(
            fixture.roots.get_mut(&root).unwrap(),
            route_key(1, 1),
            later,
        );
        let reset = fixture
            .snapshot(&executor, &metrics, later)
            .render(&executor);
        assert_eq!(reset["roots"][0]["routes"][0]["idle_age_ms"], 0);
    }

    #[test]
    fn route_census_scope_and_principal_totals_include_omitted_routes_and_overlap_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let now = Instant::now();
        let mut fixture = Fixture::default();
        let metrics = DispatchPathMetrics::new();
        let executor = Executor::new();
        for channel in 1..=40 {
            fixture.bind(
                &root,
                channel,
                channel != 1,
                if channel == 1 {
                    "direct"
                } else {
                    "reserved:broca"
                },
                now,
            );
        }
        let scope = fixture.routes[&route_key(2, 1)].scope.clone().unwrap();
        metrics.scope_descriptions.lock().unwrap().record(
            &scope,
            Some(&ModuleControlResponseToModule::ScopeDescribe {
                status: subc_protocol::scope::ScopeStatus::Ended,
                scope_epoch: Some(7),
                daemon_incarnation: "not-exposed".into(),
                owner_synced: true,
                owner_configured: true,
                scope: None,
            }),
            now - Duration::from_secs(4),
        );
        let value = fixture.snapshot(&executor, &metrics, now).render(&executor);
        assert_eq!(value["roots"][0]["route_count"], 40);
        assert_eq!(
            value["roots"][0]["routes"].as_array().unwrap().len(),
            ROUTE_LIMIT
        );
        assert_eq!(value["roots"][0]["routes_omitted"], 8);
        assert_eq!(value["summary"]["scoped"], json!({"routes":39,"roots":1}));
        assert_eq!(value["summary"]["unscoped"], json!({"routes":1,"roots":1}));
        assert_eq!(value["summary"]["roots_with_ended_scopes"], 1);
        assert_eq!(
            value["summary"]["by_principal"][1]["principal"],
            "reserved:broca"
        );
        assert_eq!(
            value["summary"]["by_principal"][1]["bound"],
            json!({"routes":39,"roots":1})
        );
        assert_eq!(value["roots"][0]["routes"][0]["scope"], "none");
        assert_eq!(
            value["roots"][0]["routes"][1]["scope"],
            json!({"owner":"direct","ref":"scope-id","epoch":7})
        );
        assert_eq!(
            value["roots"][0]["routes"][1]["scope_describe"]["status"],
            "ended"
        );
        assert_eq!(
            value["roots"][0]["routes"][1]["scope_describe"]["age_ms"],
            4000
        );
        assert!(!value.to_string().contains("not-exposed"));
    }

    #[test]
    fn route_census_shows_running_bash_and_watch_without_reading_output_or_waiting_on_state() {
        use crate::bash_background::registry::default_kill_tests::wait_fixture;
        use crate::bash_background::watches::WatchPattern;
        let (dir, ctx, task) = wait_fixture(None);
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let registry = ctx.bash_background();
        std::fs::write(&task.paths.stdout, "private task output").unwrap();
        std::fs::write(&task.paths.stderr, "").unwrap();
        let watch = registry
            .register_watch(
                task.task_id.clone(),
                WatchPattern::Substring("secret pattern".into()),
                false,
            )
            .unwrap();
        std::fs::remove_file(&task.paths.stdout).unwrap();
        std::fs::remove_file(&task.paths.stderr).unwrap();
        let ctx = Arc::new(ctx);
        let executor = Executor::new();
        executor.register_actor(root.clone(), ctx.clone());
        let mut fixture = Fixture::default();
        let now = Instant::now();
        fixture.bind(&root, 1, false, "direct", now);
        fixture.bind(&root, 2, false, "direct", now);
        let metrics = DispatchPathMetrics::new();
        let value = fixture.snapshot(&executor, &metrics, now).render(&executor);
        for route in value["roots"][0]["routes"].as_array().unwrap() {
            assert_eq!(route["pins"]["running_bash"], 1);
            assert_eq!(route["pins"]["active_watch"], 1);
            assert_eq!(route["last_background_work_age_ms"], 0);
        }
        assert!(!value.to_string().contains("secret pattern"));
        assert!(!value.to_string().contains("fake running command"));
        assert!(!value.to_string().contains("private task output"));
        let guard = task.state.lock().unwrap();
        let busy = fixture.snapshot(&executor, &metrics, now).render(&executor);
        assert!(busy["roots"][0]["routes"][0]["pins"]["running_bash"].is_null());
        drop(guard);
        ctx.bash_background()
            .unregister_watch(&task.task_id, &watch);
    }

    #[test]
    fn route_census_attributes_inflight_and_read_pins_to_the_calling_route() {
        let dir = tempfile::tempdir().unwrap();
        let root = ProjectRootId::from_path(dir.path()).unwrap();
        let executor = Executor::new();
        executor.register_actor(
            root.clone(),
            Arc::new(AppContext::from_app(
                App::default_shared(),
                Config::default(),
            )),
        );
        let mut fixture = Fixture::default();
        let metrics = DispatchPathMetrics::new();
        let now = Instant::now();
        fixture.bind(&root, 1, false, "direct", now);
        fixture.bind(&root, 2, false, "direct", now);
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let response = submit_active_tool_call(
            &executor,
            &fixture.active,
            route_key(1, 1),
            70,
            root,
            Lane::PureRead,
            "subc-1-70".into(),
            RouteDetachPolicy::RetainForReplay,
            "read",
            RequestFrameMeta {
                ver: PROTOCOL_VERSION,
                flags: control_flags(),
                deadline: None,
            },
            Box::new(move |_| {
                started_tx.send(()).unwrap();
                let _ = release_rx.recv();
                Response::success("subc-1-70", json!({}))
            }),
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The census reads the executor's running read requests with a
        // try-lock and reports `read_lease: null` (unknown) when the scheduler
        // holds that lock at that instant. Take snapshots until one is not
        // contended; the parked job keeps both pins in place meanwhile.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let value = loop {
            let value = fixture.snapshot(&executor, &metrics, now).render(&executor);
            if !value["roots"][0]["routes"][0]["pins"]["read_lease"].is_null()
                || std::time::Instant::now() >= deadline
            {
                break value;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        drop(release_tx);
        assert!(response.blocking_recv().unwrap().success);
        assert_eq!(value["roots"][0]["routes"][0]["pins"]["in_flight_call"], 1);
        assert_eq!(value["roots"][0]["routes"][0]["pins"]["read_lease"], 1);
        assert_eq!(value["roots"][0]["routes"][1]["pins"]["in_flight_call"], 0);
        assert_eq!(value["roots"][0]["routes"][1]["pins"]["read_lease"], 0);
    }

    #[test]
    fn route_census_200_roots_500_routes_is_a_single_join_pass() {
        let dir = tempfile::tempdir().unwrap();
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let now = Instant::now();
        let mut fixture = Fixture::default();
        let mut channel = 1;
        for index in 0..200 {
            let path = dir.path().join(format!("root-{index}"));
            std::fs::create_dir(&path).unwrap();
            let root = ProjectRootId::from_path(&path).unwrap();
            for _ in 0..(if index < 100 { 3 } else { 2 }) {
                fixture.bind(&root, channel, false, "direct", now);
                channel += 1;
            }
        }
        let joined = fixture.snapshot(&executor, &metrics, now);
        assert_eq!(
            joined.routes_examined, 500,
            "the join must visit each route once, not once per root"
        );
        let value = joined.render(&executor);
        assert_eq!(value["roots"].as_array().unwrap().len(), 200);
        assert_eq!(value["summary"]["bound_routes"], 500);
        assert_eq!(
            value["roots"]
                .as_array()
                .unwrap()
                .iter()
                .map(|root| root["routes"].as_array().unwrap().len())
                .sum::<usize>(),
            500
        );
    }

    #[test]
    fn route_census_caps_roots_principals_and_description_history_with_counts() {
        let dir = tempfile::tempdir().unwrap();
        let executor = Executor::new();
        let metrics = DispatchPathMetrics::new();
        let now = Instant::now();
        let mut fixture = Fixture::default();
        for index in 0..ROOT_LIMIT + 2 {
            let path = dir.path().join(format!("root-{index}"));
            std::fs::create_dir(&path).unwrap();
            let root = ProjectRootId::from_path(&path).unwrap();
            fixture.bind(
                &root,
                (index + 1) as u16,
                false,
                &format!("reserved:owner-{index:03}"),
                now,
            );
        }
        let value = fixture.snapshot(&executor, &metrics, now).render(&executor);
        assert_eq!(value["roots"].as_array().unwrap().len(), ROOT_LIMIT);
        assert_eq!(value["roots_omitted"], 2);
        assert_eq!(value["summary"]["bound_roots"], ROOT_LIMIT + 2);
        assert_eq!(
            value["summary"]["principals_omitted"],
            ROOT_LIMIT + 2 - PRINCIPAL_LIMIT
        );
        assert_eq!(
            value["summary"]["omitted_principal_routes"],
            ROOT_LIMIT + 2 - PRINCIPAL_LIMIT
        );
        assert_eq!(
            value["summary"]["omitted_principal_roots"],
            ROOT_LIMIT + 2 - PRINCIPAL_LIMIT
        );
        let mut history = metrics.scope_descriptions.lock().unwrap();
        for epoch in 0..=DESCRIPTION_LIMIT {
            let scope = ScopeStamp::new(
                Principal::Direct,
                "one-scope",
                epoch as u64,
                subc_protocol::scope::ScopeKind::Head,
            );
            history.record(&scope, None, now);
        }
        assert_eq!(history.entries.len(), DESCRIPTION_LIMIT);
        assert_eq!(history.evicted, 1);
    }
}
