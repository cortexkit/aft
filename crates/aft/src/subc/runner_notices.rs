//! The runner records each notice's send_id durably, so retries cannot deliver
//! the same notice twice. Runner sessions do not acknowledge AFT push frames.

use super::*;
use cortexkit_role_llm_runner::{
    delivery_order::{DeliveryOrder, Mode},
    send::{SendReply, SendRequest},
};
use subc_client_rs::{CallError, CallOptions, CloseRouteOptions, ConsumerOptions, SubcConsumer};
use subc_protocol::{scope::ScopeSelector, BindIdentity, RouteTarget};

const MAX_ATTEMPTS: usize = 10;
const MAX_AGE_MS: u64 = 24 * 60 * 60 * 1000;
const RUNNER_CARRIERS: &[&str] = &["reserved:thalamus-harness", "reserved:broca"];
static CONNECTION: OnceLock<std::sync::RwLock<PathBuf>> = OnceLock::new();
static SCHEDULER: OnceLock<Result<Arc<Scheduler>, String>> = OnceLock::new();
static EXPIRED: AtomicU64 = AtomicU64::new(0);
const QUEUE_CAPACITY: usize = 1024;
type Work = Box<dyn FnMut() -> Next + Send>;

enum Next {
    Done,
    Retry(Duration),
}

struct Scheduled {
    // Taking work leaves its send_id reserved until the attempt returns, so a
    // reconnect cannot start a second send for the same completion.
    work: Option<Work>,
    due: std::time::Instant,
    completion: bool,
    retrying: bool,
    expedite: bool,
}

#[derive(Default)]
struct Scheduler {
    // Delayed and in-flight notices count toward the same queue capacity limit.
    // Idle workers wait on the earliest deadline, not on one notice's backoff.
    entries: std::sync::Mutex<HashMap<String, Scheduled>>,
    wake: std::sync::Condvar,
}

impl Scheduler {
    fn offer(&self, id: String, completion: bool, work: Work, expedite: bool) -> bool {
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(&id) {
            if expedite {
                if entry.work.is_some() {
                    entry.due = std::time::Instant::now();
                } else {
                    entry.expedite = true;
                }
                self.wake.notify_all();
            }
            return true;
        }
        if entries.len() >= QUEUE_CAPACITY {
            return false;
        }
        entries.insert(
            id,
            Scheduled {
                work: Some(work),
                due: std::time::Instant::now(),
                completion,
                retrying: false,
                expedite: false,
            },
        );
        self.wake.notify_all();
        true
    }

    fn take(
        entries: &mut HashMap<String, Scheduled>,
        now: std::time::Instant,
    ) -> Option<(String, Work)> {
        let (id, entry) = entries
            .iter_mut()
            .filter(|(_, entry)| entry.work.is_some() && entry.due <= now)
            .min_by_key(|(_, entry)| entry.due)?;
        Some((id.clone(), entry.work.take().unwrap()))
    }

    fn finish(&self, id: &str, work: Work, next: Next, now: std::time::Instant) {
        let mut entries = self.entries.lock().unwrap();
        match next {
            Next::Done => {
                entries.remove(id);
            }
            Next::Retry(delay) => {
                let entry = entries.get_mut(id).unwrap();
                entry.work = Some(work);
                entry.due = if entry.expedite { now } else { now + delay };
                entry.expedite = false;
                entry.retrying = true;
            }
        }
        self.wake.notify_all();
    }

    fn run(&self) {
        loop {
            let (id, mut work) = {
                let mut entries = self.entries.lock().unwrap();
                loop {
                    let now = std::time::Instant::now();
                    if let Some(work) = Self::take(&mut entries, now) {
                        break work;
                    }
                    let due = entries
                        .values()
                        .filter(|entry| entry.work.is_some())
                        .map(|entry| entry.due)
                        .min();
                    entries = match due {
                        Some(due) => {
                            self.wake
                                .wait_timeout(entries, due.saturating_duration_since(now))
                                .unwrap()
                                .0
                        }
                        None => self.wake.wait(entries).unwrap(),
                    };
                }
            };
            let next = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut work))
                .unwrap_or_else(|_| {
                    log::error!(
                        "runner notice worker panicked; persisted completion remains pending"
                    );
                    Next::Done
                });
            self.finish(&id, work, next, std::time::Instant::now());
        }
    }
}

fn scheduler() -> Option<&'static Arc<Scheduler>> {
    match SCHEDULER.get_or_init(|| {
        let scheduler = Arc::new(Scheduler::default());
        for _ in 0..16 {
            let scheduler = scheduler.clone();
            std::thread::Builder::new()
                .name("aft-runner-notice".into())
                .spawn(move || scheduler.run())
                .map_err(|error| error.to_string())?;
        }
        Ok(scheduler)
    }) {
        Ok(scheduler) => Some(scheduler),
        Err(error) => {
            log::error!("runner notice workers unavailable: {error}");
            None
        }
    }
}

pub(super) fn health() -> (usize, u64) {
    let waiting = SCHEDULER
        .get()
        .and_then(|result| result.as_ref().ok())
        .map(|scheduler| {
            scheduler
                .entries
                .lock()
                .unwrap()
                .values()
                .filter(|entry| entry.completion && entry.retrying && entry.work.is_some())
                .count()
        })
        .unwrap_or(0);
    (waiting, EXPIRED.load(Ordering::Relaxed))
}

pub(super) fn install(path: PathBuf) {
    *CONNECTION.get_or_init(Default::default).write().unwrap() = path;
}

pub(super) fn is_runner(identity: &crate::bash_background::TaskIdentity) -> bool {
    identity.harness.parse::<crate::harness::Harness>() == Ok(crate::harness::Harness::Runner)
        || identity
            .principal
            .as_deref()
            .is_some_and(|principal| RUNNER_CARRIERS.contains(&principal))
        || retired(identity)
}

pub(super) fn retired(identity: &crate::bash_background::TaskIdentity) -> bool {
    identity.principal.as_deref() == Some("reserved:thalamus-pi")
}

pub(super) fn runner_route(route: &RouteIdentity) -> bool {
    is_runner(&route.task_identity())
}

pub(super) fn warning(
    route: &RouteIdentity,
    root: &ProjectRootId,
    warning: &crate::protocol::ConfigureWarningsFrame,
) {
    let owner = route.task_identity();
    let raw = match &route.spawn_principal {
        AuthenticatedPrincipal::RouteBind { harness, .. } => harness.clone(),
        _ => return,
    };
    let Some(address) = Address::new(&owner, raw, route.scope.as_ref()) else {
        return;
    };
    let Some(path) = CONNECTION.get().map(|path| path.read().unwrap().clone()) else {
        return;
    };
    for warning in &warning.warnings {
        let root_key = blake3::hash(root.as_path().to_string_lossy().as_bytes());
        let warning_key = blake3::hash(&serde_json::to_vec(warning).unwrap());
        let id = format!("aft:warning:{root_key}:{warning_key}");
        let request = request(
            id,
            super::bash_notice_text::warning(warning),
            false,
            address.worker,
        );
        let address = address.clone();
        let path = path.clone();
        let mut attempt = 0;
        let mut retry_logged = false;
        let Some(scheduler) = scheduler() else { return };
        if !scheduler.offer(
            request.send_id.clone(),
            false,
            Box::new(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return Next::Done;
                };
                let next = deliver(
                    &request.send_id,
                    attempt,
                    false,
                    runtime.block_on(send(&path, &address, &request)),
                    &mut retry_logged,
                );
                attempt += 1;
                next
            }),
            false,
        ) {
            log::warn!("runner warning deferred: notice queue full");
            return;
        }
    }
}

#[derive(Clone)]
struct Address {
    module: String,
    identity: BindIdentity,
    scope: Option<ScopeSelector>,
    worker: bool,
}

impl Address {
    fn new(
        owner: &crate::bash_background::TaskIdentity,
        raw_harness: String,
        stamp: Option<&subc_protocol::scope::ScopeStamp>,
    ) -> Option<Self> {
        if retired(owner) {
            return None;
        }
        let module = owner
            .principal
            .as_deref()?
            .strip_prefix("reserved:")?
            .to_string();
        Some(Self {
            worker: module == "broca"
                || stamp.is_some_and(|stamp| stamp.kind == subc_protocol::scope::ScopeKind::Worker),
            module,
            identity: BindIdentity::new(raw_harness, owner.session.clone()),
            scope: stamp.map(|stamp| {
                ScopeSelector::new(
                    stamp.owner.clone(),
                    stamp.scope_ref.clone(),
                    Some(stamp.scope_epoch),
                )
            }),
        })
    }
}

fn request(id: String, text: String, completion: bool, worker: bool) -> SendRequest {
    let mut modes = vec![Mode::ToolResult, Mode::Steer];
    if completion && !worker {
        modes.push(Mode::IdleStart);
    }
    SendRequest::new(text, id)
        .with_notice(true)
        .with_delivery_order(&DeliveryOrder::new(modes))
}

fn envelope(request: &SendRequest) -> Vec<u8> {
    serde_json::to_vec(&json!({"method":"session.send", "params":request}))
        .expect("notice request is JSON")
}

fn target(address: &Address) -> RouteTarget {
    RouteTarget::ManagementSurface {
        module_id: address.module.clone(),
    }
}

fn role_versions() -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([(
        cortexkit_role_llm_runner::ROLE.into(),
        cortexkit_role_llm_runner::VERSION.into(),
    )])
}

#[cfg(test)]
pub(super) mod test_endpoint {
    use super::*;
    struct Endpoint {
        sender: std::sync::mpsc::Sender<Received>,
        stored: std::sync::Mutex<Option<Vec<u8>>>,
        lose_response: AtomicBool,
        effects: Arc<AtomicU64>,
        retries: AtomicU64,
        active: AtomicU64,
        max_active: AtomicU64,
        gate:
            std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    }
    fn endpoints() -> &'static std::sync::Mutex<HashMap<String, Arc<Endpoint>>> {
        static ENDPOINTS: OnceLock<std::sync::Mutex<HashMap<String, Arc<Endpoint>>>> =
            OnceLock::new();
        ENDPOINTS.get_or_init(Default::default)
    }
    pub(super) fn contains(id: &str) -> bool {
        endpoints().lock().unwrap().contains_key(id)
    }
    pub(in crate::subc) struct Received {
        pub harness: String,
        pub session: String,
        pub scope: Option<ScopeSelector>,
        pub request: Value,
    }
    pub(in crate::subc) struct Capture {
        key: String,
        pub receiver: std::sync::mpsc::Receiver<Received>,
        pub effects: Arc<AtomicU64>,
        endpoint: Arc<Endpoint>,
    }
    impl Capture {
        pub(super) fn retry(&self, count: u64) {
            self.endpoint.retries.store(count, Ordering::Relaxed);
        }
        pub(super) fn block_next(
            &self,
        ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
            let (entered, wait_entered) = std::sync::mpsc::channel();
            let (release, wait_release) = std::sync::mpsc::channel();
            *self.endpoint.gate.lock().unwrap() = Some((entered, wait_release));
            (wait_entered, release)
        }
        pub(super) fn max_active(&self) -> u64 {
            self.endpoint.max_active.load(Ordering::Relaxed)
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            endpoints().lock().unwrap().remove(&self.key);
        }
    }
    pub(in crate::subc) fn capture(task: &str, lose_response: bool) -> Capture {
        let key = format!("aft:bash:{task}");
        let (sender, receiver) = std::sync::mpsc::channel();
        let effects = Arc::new(AtomicU64::new(0));
        let endpoint = Arc::new(Endpoint {
            sender,
            stored: Default::default(),
            lose_response: AtomicBool::new(lose_response),
            effects: effects.clone(),
            retries: AtomicU64::new(0),
            active: AtomicU64::new(0),
            max_active: AtomicU64::new(0),
            gate: Default::default(),
        });
        assert!(endpoints()
            .lock()
            .unwrap()
            .insert(key.clone(), endpoint.clone())
            .is_none());
        Capture {
            key,
            receiver,
            effects,
            endpoint,
        }
    }
    pub(super) fn send(address: &Address, request: &SendRequest) -> Option<Outcome> {
        let endpoint = endpoints().lock().unwrap().get(&request.send_id).cloned()?;
        let active = endpoint.active.fetch_add(1, Ordering::Relaxed) + 1;
        endpoint.max_active.fetch_max(active, Ordering::Relaxed);
        struct Active<'a>(&'a AtomicU64);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _active = Active(&endpoint.active);
        let body: Value = serde_json::from_slice(&envelope(request)).unwrap();
        assert_eq!(body["method"], "session.send");
        assert_eq!(body.as_object().unwrap().len(), 2);
        let decoded: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
        decoded.check_prompt().unwrap();
        assert_eq!(body["params"]["notice"], true);
        let _ = endpoint.sender.send(Received {
            harness: address.identity.harness.clone(),
            session: address.identity.session.clone(),
            scope: address.scope.clone(),
            request: body,
        });
        let gate = endpoint.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        if endpoint
            .retries
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Some(Outcome::Retry("endpoint_unavailable"));
        }
        let bytes = serde_json::to_vec(&decoded).unwrap();
        let mut stored = endpoint.stored.lock().unwrap();
        if let Some(first) = stored.as_ref() {
            if first != &bytes {
                return Some(refusal("send_id_reuse"));
            }
        } else {
            *stored = Some(bytes);
            endpoint.effects.fetch_add(1, Ordering::Relaxed);
        }
        if endpoint.lose_response.swap(false, Ordering::Relaxed) {
            return Some(Outcome::Retry("response_lost"));
        }
        let reply = cortexkit_role_llm_runner::wire::reply_body(
            &SendReply::new("pending").with_submission_id("notice-1"),
        )
        .unwrap();
        cortexkit_role_llm_runner::wire::read_reply::<SendReply>(&reply).unwrap();
        Some(Outcome::Accepted)
    }
}

#[derive(Debug)]
enum Outcome {
    Accepted,
    Final(String),
    Retry(&'static str),
}

fn refusal(code: &str) -> Outcome {
    match code {
        "scope_owner_mismatch"
        | "expired"
        | "session_released"
        | "send_id_reuse"
        | "delivery_unsupported"
        | "invalid_params" => Outcome::Final(code.into()),
        _ => Outcome::Retry("retryable_refusal"),
    }
}

fn deliver(
    send_id: &str,
    attempt: usize,
    completion: bool,
    outcome: Outcome,
    retry_logged: &mut bool,
) -> Next {
    match outcome {
        Outcome::Accepted => Next::Done,
        Outcome::Final(code) => {
            if completion && code == "expired" {
                EXPIRED.fetch_add(1, Ordering::Relaxed);
            }
            if code == "send_id_reuse" {
                log::error!("runner notice {send_id} refused: {code}");
            } else {
                log::warn!("runner notice {send_id} refused: {code}");
            }
            Next::Done
        }
        Outcome::Retry(reason) => {
            if completion && !*retry_logged {
                log::info!("runner notice {send_id} awaiting retry: {reason}");
                *retry_logged = true;
            }
            if !completion && attempt.saturating_add(1) >= MAX_ATTEMPTS {
                log::warn!("runner notice {send_id} retired: retry budget exhausted");
                return Next::Done;
            }
            Next::Retry(Duration::from_millis(
                (250u64 << attempt.min(8)).min(60_000),
            ))
        }
    }
}

#[cfg(test)]
#[path = "runner_notices_tests.rs"]
mod tests;

async fn send(path: &Path, address: &Address, request: &SendRequest) -> Outcome {
    #[cfg(test)]
    if let Some(outcome) = test_endpoint::send(address, request) {
        return outcome;
    }
    match call(path, address, envelope(request)).await {
        Ok(bytes) => match cortexkit_role_llm_runner::wire::read_reply::<SendReply>(&bytes) {
            Ok(_) => Outcome::Accepted,
            Err(_) => Outcome::Retry("invalid_reply"),
        },
        Err(outcome) => outcome,
    }
}

fn idle_start_grant(head: &cortexkit_role_llm_runner::read::HeadMeta) -> Option<bool> {
    head.sender
        .as_ref()
        .map(|sender| sender.modes.iter().any(|mode| mode == "idle_start"))
}

async fn read_idle_start_grant(path: &Path, address: &Address) -> Option<bool> {
    let body = serde_json::to_vec(&json!({"method":"session.head", "params":cortexkit_role_llm_runner::read::HeadRequest::default()})).ok()?;
    let bytes = call(path, address, body).await.ok()?;
    let head = cortexkit_role_llm_runner::wire::read_reply::<
        cortexkit_role_llm_runner::read::HeadMeta,
    >(&bytes)
    .ok()?;
    idle_start_grant(&head)
}

async fn call(path: &Path, address: &Address, body: Vec<u8>) -> Result<Vec<u8>, Outcome> {
    let Some(principal) = crate::launch_nonce::consumer_identity() else {
        return Err(Outcome::Retry("identity_unavailable"));
    };
    let Ok(consumer) = SubcConsumer::connect(path, ConsumerOptions::default()).await else {
        return Err(Outcome::Retry("connection_unavailable"));
    };
    let target = target(address);
    let options = CallOptions {
        timeout: Duration::from_secs(5),
        route_retry_deadline: Duration::from_secs(5),
        consumer_identity: Some(principal),
        role_versions: Some(role_versions()),
        ..CallOptions::default()
    };
    let route = match &address.scope {
        Some(scope) => {
            consumer
                .open_route_scoped(target, address.identity.clone(), scope.clone(), options)
                .await
        }
        None => {
            consumer
                .open_route(target, address.identity.clone(), options)
                .await
        }
    };
    let result = match route {
        Ok(route) => {
            let result = consumer
                .request(
                    &route,
                    body,
                    CallOptions {
                        timeout: Duration::from_secs(5),
                        ..CallOptions::default()
                    },
                )
                .await;
            let _ = consumer
                .close_handle(&route, CloseRouteOptions::default())
                .await;
            match result {
                Ok(bytes) => Ok(bytes),
                Err(CallError::Module(error)) => Err(refusal(&error.code)),
                Err(_) => Err(Outcome::Retry("request_failed")),
            }
        }
        Err(CallError::Module(error)) => Err(refusal(&error.code)),
        Err(_) => Err(Outcome::Retry("route_unavailable")),
    };
    let _ = consumer.close().await;
    result
}

pub(super) fn enqueue(registry: crate::bash_background::BgTaskRegistry, frame: PushFrame) -> bool {
    enqueue_inner(registry, frame, false)
}

pub(super) fn route_bound(registry: crate::bash_background::BgTaskRegistry, route: &RouteIdentity) {
    if !runner_route(route) || !route.trust.allows_bash_observation() {
        return;
    }
    let owner = route.task_identity();
    let _identity = crate::bash_background::task_identity_scope(Some(owner.clone()));
    for completion in registry.pending_completions_for_session(&owner.session) {
        if completion.launching_identity.as_ref() == Some(&owner) {
            enqueue_inner(registry.clone(), completion.into_push_frame(), true);
        }
    }
}

fn enqueue_inner(
    registry: crate::bash_background::BgTaskRegistry,
    frame: PushFrame,
    expedite: bool,
) -> bool {
    let (task_id, owner) = match &frame {
        PushFrame::BashCompleted(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashLongRunning(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashPatternMatch(frame) => (&frame.task_id, &frame.launching_identity),
        _ => return false,
    };
    if !owner.as_ref().is_some_and(is_runner) {
        return false;
    }
    let path = CONNECTION.get().map(|path| path.read().unwrap().clone());
    #[cfg(test)]
    let path =
        path.or_else(|| test_endpoint::contains(&format!("aft:bash:{task_id}")).then(PathBuf::new));
    let Some(path) = path else {
        return false;
    };
    let Some(scheduler) = scheduler() else {
        return false;
    };
    offer(
        scheduler,
        registry,
        frame,
        path,
        expedite,
        crate::bash_background::persistence::unix_millis,
    )
}

fn offer(
    scheduler: &Scheduler,
    registry: crate::bash_background::BgTaskRegistry,
    frame: PushFrame,
    path: PathBuf,
    expedite: bool,
    now: impl Fn() -> u64 + Send + 'static,
) -> bool {
    let (task_id, owner) = match &frame {
        PushFrame::BashCompleted(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashLongRunning(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashPatternMatch(frame) => (&frame.task_id, &frame.launching_identity),
        _ => return false,
    };
    let Some(owner) = owner.clone().filter(is_runner) else {
        return false;
    };
    let task_id = task_id.clone();
    let (id, text, completion) = match &frame {
        PushFrame::BashCompleted(_) => (
            format!("aft:bash:{task_id}"),
            super::bash_notice_text::completion(&serde_json::to_value(&frame).unwrap()),
            true,
        ),
        PushFrame::BashLongRunning(frame) => (
            format!("aft:bash-reminder:{task_id}:{}", frame.elapsed_ms),
            super::bash_notice_text::long_running(&serde_json::to_value(frame).unwrap()),
            false,
        ),
        PushFrame::BashPatternMatch(frame) => (
            format!(
                "aft:bash-watch:{}:{}",
                frame.watch_id,
                blake3::hash(&serde_json::to_vec(frame).unwrap())
            ),
            super::bash_notice_text::watch(&serde_json::to_value(frame).unwrap()),
            false,
        ),
        _ => unreachable!(),
    };
    let mut attempt = None;
    let mut retry_logged = false;
    let mut settled = false;
    let queued = scheduler.offer(
        id.clone(),
        completion,
        Box::new(move || {
            let _identity = crate::bash_background::task_identity_scope(Some(owner.clone()));
            let Some(metadata) = registry.notice_metadata(&task_id) else {
                return Next::Done;
            };
            if completion && metadata.completion_delivered {
                return Next::Done;
            }
            if settled {
                return if registry
                    .ack_completions_for_session(
                        Some(&owner.session),
                        std::slice::from_ref(&task_id),
                    )
                    .is_empty()
                {
                    Next::Retry(Duration::from_secs(60))
                } else {
                    Next::Done
                };
            }
            let attempt = attempt.get_or_insert(if completion {
                metadata.runner_completion_offers
            } else {
                0
            });
            let age = now().saturating_sub(metadata.finished_at.unwrap_or(metadata.started_at));
            let outcome = if completion && age >= MAX_AGE_MS {
                Outcome::Final("expired".into())
            } else {
                let outcome = (|| {
                    let raw = metadata
                        .launching_raw_harness
                        .clone()
                        .unwrap_or_else(|| owner.harness.clone());
                    let Some(address) = Address::new(&owner, raw, metadata.scope.as_ref()) else {
                        return None;
                    };
                    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return Some(Outcome::Retry("runtime_unavailable"));
                    };
                    let no_idle_grant = completion
                        && !address.worker
                        && metadata.runner_completion_notice.is_none()
                        && runtime.block_on(read_idle_start_grant(&path, &address)) == Some(false);
                    let request = request(
                        id.clone(),
                        text.clone(),
                        completion,
                        address.worker || no_idle_grant,
                    );
                    let request = if let Some(request) = metadata
                        .runner_completion_notice
                        .clone()
                        .filter(|_| completion)
                    {
                        request
                    } else if completion {
                        match registry.freeze_runner_completion_notice(
                            &owner.session,
                            &task_id,
                            request,
                        ) {
                            Ok(request) => request,
                            Err(error) => {
                                log::warn!(
                                    "runner notice task={task_id} could not freeze send: {error}"
                                );
                                return Some(Outcome::Retry("notice_persistence"));
                            }
                        }
                    } else {
                        request
                    };
                    if completion {
                        let Some(offers) = registry.reserve_runner_completion_offer(&task_id)
                        else {
                            return None;
                        };
                        if registry
                            .persist_runner_completion_offer(&owner.session, &task_id, offers)
                            .is_err()
                        {
                            return Some(Outcome::Retry("offer_persistence"));
                        }
                    }
                    Some(runtime.block_on(send(&path, &address, &request)))
                })();
                let Some(outcome) = outcome else {
                    return Next::Done;
                };
                outcome
            };
            settled = completion && !matches!(outcome, Outcome::Retry(_));
            let next = deliver(&id, *attempt, completion, outcome, &mut retry_logged);
            *attempt = attempt.saturating_add(1);
            if settled {
                if registry
                    .ack_completions_for_session(
                        Some(&owner.session),
                        std::slice::from_ref(&task_id),
                    )
                    .is_empty()
                {
                    return Next::Retry(Duration::from_secs(60));
                }
            }
            match next {
                Next::Retry(delay) if completion => {
                    Next::Retry(delay.min(Duration::from_millis(MAX_AGE_MS.saturating_sub(age))))
                }
                next => next,
            }
        }),
        expedite,
    );
    if !queued {
        log::warn!("runner notice deferred: notice queue full");
    }
    queued
}
