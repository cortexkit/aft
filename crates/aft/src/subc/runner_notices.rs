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

pub(super) fn known_runner(module_id: &str) -> bool {
    RUNNER_CARRIERS
        .iter()
        .any(|principal| principal.strip_prefix("reserved:") == Some(module_id))
}
static CONNECTION: OnceLock<std::sync::RwLock<PathBuf>> = OnceLock::new();
// These weak references are registered only for live runner routes. Closing a
// route removes its reference; disconnected sessions are not harness evidence.
static BOUND_ROUTES: OnceLock<
    std::sync::Mutex<HashMap<usize, std::sync::Weak<RouteIdentityData>>>,
> = OnceLock::new();
static SCHEDULER: OnceLock<Result<Arc<Scheduler>, String>> = OnceLock::new();
static EXPIRED: AtomicU64 = AtomicU64::new(0);
static IDLE_START_FALLBACK: AtomicU64 = AtomicU64::new(0);
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

pub(super) fn health() -> (usize, u64, u64) {
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
    (
        waiting,
        EXPIRED.load(Ordering::Relaxed),
        IDLE_START_FALLBACK.load(Ordering::Relaxed),
    )
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

fn concrete_route_harness(principal: Option<&str>, session: &str) -> Option<String> {
    let mut routes = BOUND_ROUTES.get_or_init(Default::default).lock().unwrap();
    routes.retain(|_, route| route.strong_count() > 0);
    let mut harness = None;
    for route in routes.values().filter_map(std::sync::Weak::upgrade) {
        let AuthenticatedPrincipal::RouteBind {
            principal_id,
            session_id,
            harness: raw,
            ..
        } = &route.spawn_principal
        else {
            continue;
        };
        if principal_id.as_deref() != principal || session_id != session || raw == "runner" {
            continue;
        }
        if harness.as_ref().is_some_and(|previous| previous != raw) {
            // Conflicting live bindings cannot safely identify a host harness.
            return None;
        }
        harness = Some(raw.clone());
    }
    harness
}

/// Resolve the concrete host used when binding back to a runner session.
/// The `reserved:broca` principal maps the legacy `runner` label to `broca`;
/// other principals need an unambiguous live binding before the host is known.
pub(super) fn concrete_runner_harness(
    principal: Option<&str>,
    session: &str,
    raw_harness: &str,
) -> Option<String> {
    if raw_harness != "runner" {
        Some(raw_harness.to_owned())
    } else if principal == Some("reserved:broca") {
        Some("broca".into())
    } else {
        concrete_route_harness(principal, session)
    }
}

pub(super) fn route_unbound(route: &RouteIdentity) {
    if let Some(routes) = BOUND_ROUTES.get() {
        routes
            .lock()
            .unwrap()
            .remove(&(Arc::as_ptr(&route.0) as usize));
    }
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
        let raw_harness =
            concrete_runner_harness(owner.principal.as_deref(), &owner.session, &raw_harness)?;
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
        retries: std::sync::Mutex<u64>,
        refusals: std::sync::Mutex<std::collections::VecDeque<Value>>,
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
        pub bytes: Vec<u8>,
    }
    pub(in crate::subc) struct Capture {
        key: String,
        pub receiver: std::sync::mpsc::Receiver<Received>,
        pub effects: Arc<AtomicU64>,
        endpoint: Arc<Endpoint>,
    }
    impl Capture {
        pub(super) fn retry(&self, count: u64) {
            *self.endpoint.retries.lock().unwrap() = count;
        }
        pub(super) fn refuse(&self, code: &str, field: Option<&str>) {
            let mut error = json!({"code": code, "message": "notice refused"});
            if let Some(field) = field {
                error["detail"] = json!({"field": field});
            }
            self.endpoint
                .refusals
                .lock()
                .unwrap()
                .push_back(json!({"error": error}));
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
            retries: Default::default(),
            refusals: Default::default(),
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
    pub(super) fn call(address: &Address, bytes: &[u8]) -> Option<Result<Vec<u8>, Outcome>> {
        let body: Value = serde_json::from_slice(bytes).unwrap();
        let key = body["params"]["send_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("aft:bash:{}", address.identity.session));
        let endpoint = endpoints().lock().unwrap().get(&key).cloned()?;
        let active = endpoint.active.fetch_add(1, Ordering::Relaxed) + 1;
        endpoint.max_active.fetch_max(active, Ordering::Relaxed);
        struct Active<'a>(&'a AtomicU64);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let _active = Active(&endpoint.active);
        assert_eq!(body.as_object().unwrap().len(), 2);
        let _ = endpoint.sender.send(Received {
            harness: address.identity.harness.clone(),
            session: address.identity.session.clone(),
            scope: address.scope.clone(),
            request: body.clone(),
            bytes: bytes.to_vec(),
        });
        if body["method"] != "session.send" {
            assert_eq!(body["method"], "session.head");
            return Some(Err(refusal("invalid_params")));
        }
        let decoded: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
        decoded.check_prompt().unwrap();
        assert_eq!(body["params"]["notice"], true);
        let gate = endpoint.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        if let Some(refusal) = endpoint.refusals.lock().unwrap().pop_front() {
            return Some(Ok(serde_json::to_vec(&refusal).unwrap()));
        }
        let should_retry = {
            let mut remaining = endpoint.retries.lock().unwrap();
            let retry = *remaining > 0;
            *remaining = remaining.saturating_sub(1);
            retry
        };
        if should_retry {
            return Some(Err(retry(
                FailureClass::TestEndpoint,
                "endpoint unavailable",
            )));
        }
        let bytes = serde_json::to_vec(&decoded).unwrap();
        let mut stored = endpoint.stored.lock().unwrap();
        if let Some(first) = stored.as_ref() {
            if first != &bytes {
                return Some(Err(refusal("send_id_reuse")));
            }
        } else {
            *stored = Some(bytes);
            endpoint.effects.fetch_add(1, Ordering::Relaxed);
        }
        if endpoint.lose_response.swap(false, Ordering::Relaxed) {
            return Some(Err(retry(FailureClass::TestEndpoint, "response lost")));
        }
        let reply = cortexkit_role_llm_runner::wire::reply_body(
            &SendReply::new("pending").with_submission_id("notice-1"),
        )
        .unwrap();
        cortexkit_role_llm_runner::wire::read_reply::<SendReply>(&reply).unwrap();
        Some(Ok(reply))
    }
}

#[derive(Debug)]
enum Outcome {
    Accepted,
    Final(Failure),
    Retry(Failure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    ConsumerIdentity,
    Connect,
    RouteOpen,
    Request,
    ReplyDecode,
    PeerRefusal,
    Runtime,
    Address,
    Persistence,
    Expired,
    #[cfg(test)]
    TestEndpoint,
}

#[derive(Debug)]
struct Failure {
    class: FailureClass,
    code: Option<String>,
    field: Option<String>,
    detail: String,
}

impl Failure {
    fn new(class: FailureClass, detail: impl ToString) -> Self {
        Self {
            class,
            code: None,
            field: None,
            detail: detail.to_string(),
        }
    }

    fn coded(class: FailureClass, code: &str, detail: impl ToString) -> Self {
        Self {
            class,
            code: Some(code.into()),
            field: None,
            detail: detail.to_string(),
        }
    }
}

fn retry(class: FailureClass, detail: impl ToString) -> Outcome {
    Outcome::Retry(Failure::new(class, detail))
}

fn refusal(code: &str) -> Outcome {
    match code {
        "scope_owner_mismatch"
        | "expired"
        | "session_released"
        | "send_id_reuse"
        | "delivery_unsupported"
        | "invalid_params" => Outcome::Final(Failure::coded(FailureClass::PeerRefusal, code, code)),
        _ => Outcome::Retry(Failure::coded(FailureClass::PeerRefusal, code, code)),
    }
}

fn deliver(
    send_id: &str,
    attempt: usize,
    completion: bool,
    outcome: Outcome,
    retry_logged: &mut bool,
) -> Next {
    if completion && !*retry_logged {
        if let Outcome::Final(failure) | Outcome::Retry(failure) = &outcome {
            log::info!(
                "runner notice {send_id} first failure: class={:?} code={} detail={}",
                failure.class,
                failure.code.as_deref().unwrap_or("none"),
                failure.detail
            );
            *retry_logged = true;
        }
    }
    match outcome {
        Outcome::Accepted => Next::Done,
        Outcome::Final(failure) => {
            let code = failure.code.as_deref().unwrap_or("unknown");
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
        Outcome::Retry(_) => {
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

#[cfg(feature = "test-scope-owner")]
#[path = "runner_notice_recovery_fixture.rs"]
mod recovery_fixture;
#[cfg(feature = "test-scope-owner")]
pub use recovery_fixture::RunnerNoticeRecoveryForTest;

async fn send(path: &Path, address: &Address, request: &SendRequest) -> Outcome {
    let outcome = match call(path, address, envelope(request)).await {
        Ok(bytes) => decode_send_reply(&bytes),
        Err(outcome) => outcome,
    };
    if matches!(outcome, Outcome::Accepted) {
        log::info!(
            "runner notice {} delivered to {} harness={} session={}",
            request.send_id,
            address.module,
            address.identity.harness,
            address.identity.session
        );
    }
    outcome
}

fn decode_send_reply(bytes: &[u8]) -> Outcome {
    match cortexkit_role_llm_runner::wire::read_reply::<SendReply>(bytes) {
        Ok(_) => Outcome::Accepted,
        Err(error) => {
            if let Ok(body) = serde_json::from_slice::<Value>(bytes) {
                let refusal_body = body.get("error").unwrap_or(&body);
                if let Some(code) = refusal_body.get("code").and_then(Value::as_str) {
                    let mut outcome = refusal(code);
                    if let Outcome::Final(failure) | Outcome::Retry(failure) = &mut outcome {
                        failure.field = refusal_body
                            .get("detail")
                            .and_then(|detail| detail.get("field"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        failure.detail = refusal_body.to_string();
                    }
                    return outcome;
                }
            }
            retry(FailureClass::ReplyDecode, error)
        }
    }
}

fn idle_start_fallback(request: &SendRequest, outcome: &Outcome) -> Option<SendRequest> {
    let Outcome::Final(failure) = outcome else {
        return None;
    };
    if failure.class != FailureClass::PeerRefusal
        || failure.code.as_deref() != Some("invalid_params")
        || failure.field.as_deref() != Some("delivery")
    {
        return None;
    }
    let mut order = request.delivery_order().ok()??;
    if !order.modes.contains(&Mode::IdleStart) {
        return None;
    }
    order.modes.retain(|mode| *mode != Mode::IdleStart);
    Some(request.clone().with_delivery_order(&order))
}

fn request_failure(error: CallError) -> Outcome {
    match error {
        CallError::Module(error) => {
            let mut outcome = refusal(&error.code);
            if let Outcome::Final(failure) | Outcome::Retry(failure) = &mut outcome {
                failure.field = error
                    .detail
                    .as_ref()
                    .and_then(|detail| detail.get("field"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                failure.detail = error.message;
            }
            outcome
        }
        error => retry(FailureClass::Request, error),
    }
}

#[cfg(feature = "test-scope-owner")]
pub(super) async fn transport_for_test(path: &Path, session: &str, scope: ScopeSelector) -> Value {
    let address = Address::new(
        &crate::bash_background::TaskIdentity {
            principal: Some("reserved:broca".into()),
            harness: "runner".into(),
            session: session.into(),
        },
        "runner".into(),
        None,
    )
    .unwrap();
    let address = Address {
        scope: Some(scope),
        ..address
    };
    let request = request(
        format!("aft:bash:{session}"),
        "completion".into(),
        true,
        true,
    );
    let outcome = send(path, &address, &request).await;
    let result = match &outcome {
        Outcome::Accepted => json!({"outcome":"accepted"}),
        Outcome::Final(failure) | Outcome::Retry(failure) => json!({
            "outcome": if matches!(outcome, Outcome::Final(_)) { "final" } else { "retry" },
            "class": format!("{:?}", failure.class), "code": failure.code, "detail": failure.detail,
        }),
    };
    deliver(&request.send_id, 0, true, outcome, &mut false);
    result
}

fn route_failure(address: &Address, error: CallError) -> Outcome {
    let body = error.route_open_refusal().or_else(|| match &error {
        CallError::Module(body) => Some(body),
        _ => None,
    });
    if let Some(body) = body {
        let failure = Failure::coded(FailureClass::RouteOpen, &body.code, &error);
        if subc_protocol::error_codes::is_retryable_route_open(&body.code) {
            return Outcome::Retry(failure);
        }
        log_route_refusal(address, &failure);
        return Outcome::Final(failure);
    }
    retry(FailureClass::RouteOpen, error)
}

fn log_route_refusal(address: &Address, failure: &Failure) {
    static LOGGED: OnceLock<std::sync::Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let key = (address.module.clone(), address.identity.session.clone());
    if LOGGED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(key)
    {
        log::error!("runner notice route refused: session={} module={} scope={} class={:?} code={} detail={}",
            address.identity.session, address.module,
            address.scope.as_ref().map(|scope| scope.scope_ref.as_str()).unwrap_or("unscoped"),
            failure.class, failure.code.as_deref().unwrap_or("none"), failure.detail);
    }
}

async fn call(path: &Path, address: &Address, body: Vec<u8>) -> Result<Vec<u8>, Outcome> {
    #[cfg(test)]
    if let Some(result) = test_endpoint::call(address, &body) {
        return result;
    }
    let Some(principal) = crate::launch_nonce::consumer_identity() else {
        return Err(retry(
            FailureClass::ConsumerIdentity,
            "AFT launch has no consumer identity",
        ));
    };
    let consumer = match SubcConsumer::connect(path, ConsumerOptions::default()).await {
        Ok(consumer) => consumer,
        Err(error) => return Err(retry(FailureClass::Connect, error)),
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
                Err(error) => Err(request_failure(error)),
            }
        }
        Err(error) => Err(route_failure(address, error)),
    };
    let _ = consumer.close().await;
    result
}

pub(super) fn enqueue(registry: crate::bash_background::BgTaskRegistry, frame: PushFrame) -> bool {
    enqueue_inner(registry, frame, false)
}

/// Offer only the undelivered completions explicitly recovered at startup;
/// their launching identity supplies authority even before a session rebinds.
pub(crate) fn offer_replayed(
    registry: crate::bash_background::BgTaskRegistry,
    replayed: &[(String, Vec<String>)],
) {
    for (session, tasks) in replayed {
        for task in tasks {
            let Some(metadata) = registry.notice_metadata(task) else {
                continue;
            };
            let Some(owner) = metadata.launching_identity.filter(is_runner) else {
                continue;
            };
            if owner.session != *session || metadata.completion_delivered {
                continue;
            }
            let _identity = crate::bash_background::task_identity_scope(Some(owner));
            if let Some(completion) = registry
                .pending_completions_for_session(session)
                .into_iter()
                .find(|completion| completion.task_id == *task)
            {
                enqueue(registry.clone(), completion.into_push_frame());
            }
        }
    }
}

pub(super) fn route_bound(registry: crate::bash_background::BgTaskRegistry, route: &RouteIdentity) {
    if !runner_route(route) || !route.trust.allows_bash_observation() {
        return;
    }
    if matches!(&route.spawn_principal, AuthenticatedPrincipal::RouteBind { harness, .. } if harness != "runner")
    {
        let mut routes = BOUND_ROUTES.get_or_init(Default::default).lock().unwrap();
        routes.retain(|_, route| route.strong_count() > 0);
        routes.insert(Arc::as_ptr(&route.0) as usize, Arc::downgrade(&route.0));
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
    let (_task_id, owner) = match &frame {
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
    let path = path
        .or_else(|| test_endpoint::contains(&format!("aft:bash:{_task_id}")).then(PathBuf::new));
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
                Outcome::Final(Failure::coded(
                    FailureClass::Expired,
                    "expired",
                    "task finish age reached 24 hours",
                ))
            } else {
                let outcome = (|| {
                    let raw = metadata
                        .launching_raw_harness
                        .clone()
                        .unwrap_or_else(|| owner.harness.clone());
                    let Some(address) = Address::new(&owner, raw, metadata.scope.as_ref()) else {
                        return retry(FailureClass::Address, "task has no runner notice address");
                    };
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => return retry(FailureClass::Runtime, error),
                    };
                    let request = request(id.clone(), text.clone(), completion, address.worker);
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
                                return retry(FailureClass::Persistence, error);
                            }
                        }
                    } else {
                        request
                    };
                    if completion {
                        let Some(offers) = registry.reserve_runner_completion_offer(&task_id)
                        else {
                            return retry(
                                FailureClass::Persistence,
                                "could not reserve completion offer",
                            );
                        };
                        if let Err(error) = registry.persist_runner_completion_offer(
                            &owner.session,
                            &task_id,
                            offers,
                        ) {
                            return retry(FailureClass::Persistence, error);
                        }
                    }
                    let send_request = |request: &SendRequest| {
                        if completion {
                            registry.send_runner_completion(&task_id, || runtime.block_on(send(&path, &address, request)))
                                .unwrap_or(Outcome::Accepted)
                        } else {
                            runtime.block_on(send(&path, &address, request))
                        }
                    };
                    let outcome = send_request(&request);
                    if completion && !address.worker {
                        if let Some(request) = idle_start_fallback(&request, &outcome) {
                            // A delivery refusal records no send_id on the runner, so the
                            // frozen request can be replaced before retrying the send.
                            if let Err(error) = registry.replace_runner_completion_notice(
                                &owner.session,
                                &task_id,
                                request.clone(),
                            ) {
                                return retry(FailureClass::Persistence, error);
                            }
                            IDLE_START_FALLBACK.fetch_add(1, Ordering::Relaxed);
                            log::info!(
                                "runner notice task={task_id} session={} retrying without idle_start after delivery refusal",
                                owner.session
                            );
                            return send_request(&request);
                        }
                    }
                    outcome
                })();
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
