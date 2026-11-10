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
fn enqueue_work(work: impl FnOnce() + Send + 'static) -> bool {
    type Work = Box<dyn FnOnce() + Send>;
    static QUEUE: OnceLock<Result<std::sync::mpsc::SyncSender<Work>, String>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Work>(1024);
        let receiver = Arc::new(std::sync::Mutex::new(receiver));
        for _ in 0..16 {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name("aft-runner-notice".into())
                .spawn(move || loop {
                    let work = receiver.lock().unwrap().recv();
                    let Ok(work) = work else { return };
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).is_err() {
                        log::error!(
                            "runner notice worker panicked; persisted completion remains pending"
                        );
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(sender)
    });
    match queue {
        Ok(sender) => sender.try_send(Box::new(work)).is_ok(),
        Err(error) => {
            log::error!("runner notice workers unavailable: {error}");
            false
        }
    }
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
        if !enqueue_work(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            deliver(
                &request.send_id,
                0,
                |_| Some(runtime.block_on(send(&path, &address, &request))),
                std::thread::sleep,
            );
        }) {
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

#[cfg(test)]
pub(super) mod test_endpoint {
    use super::*;
    struct Endpoint {
        sender: std::sync::mpsc::Sender<Received>,
        stored: std::sync::Mutex<Option<Vec<u8>>>,
        lose_response: AtomicBool,
        effects: Arc<AtomicU64>,
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
        assert!(endpoints()
            .lock()
            .unwrap()
            .insert(
                key.clone(),
                Arc::new(Endpoint {
                    sender,
                    stored: Default::default(),
                    lose_response: AtomicBool::new(lose_response),
                    effects: effects.clone()
                })
            )
            .is_none());
        Capture {
            key,
            receiver,
            effects,
        }
    }
    pub(super) fn send(address: &Address, request: &SendRequest) -> Option<Outcome> {
        let endpoint = endpoints().lock().unwrap().get(&request.send_id).cloned()?;
        let body: Value = serde_json::from_slice(&envelope(request)).unwrap();
        assert_eq!(body["method"], "session.send");
        assert_eq!(body.as_object().unwrap().len(), 2);
        let decoded: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
        decoded.check_prompt().unwrap();
        assert_eq!(body["params"]["notice"], true);
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
        let _ = endpoint.sender.send(Received {
            harness: address.identity.harness.clone(),
            session: address.identity.session.clone(),
            scope: address.scope.clone(),
            request: body,
        });
        if endpoint.lose_response.swap(false, Ordering::Relaxed) {
            return Some(Outcome::Retry);
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
    Retry,
}

fn refusal(code: &str) -> Outcome {
    match code {
        "scope_owner_mismatch"
        | "expired"
        | "session_released"
        | "send_id_reuse"
        | "delivery_unsupported"
        | "invalid_params" => Outcome::Final(code.into()),
        _ => Outcome::Retry,
    }
}

fn deliver(
    send_id: &str,
    first: usize,
    mut send: impl FnMut(usize) -> Option<Outcome>,
    mut pause: impl FnMut(Duration),
) -> bool {
    for attempt in first..MAX_ATTEMPTS {
        match send(attempt) {
            Some(Outcome::Accepted) => return true,
            Some(Outcome::Final(code)) => {
                if code == "send_id_reuse" {
                    log::error!("runner notice {send_id} refused: {code}");
                } else {
                    log::warn!("runner notice {send_id} refused: {code}");
                }
                return true;
            }
            Some(Outcome::Retry) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    pause(Duration::from_millis(
                        (250u64 << attempt.min(8)).min(60_000),
                    ));
                }
            }
            None => return false,
        }
    }
    log::warn!("runner notice {send_id} retired: retry budget exhausted");
    true
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
            Err(_) => Outcome::Retry,
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
        return Err(Outcome::Retry);
    };
    let Ok(consumer) = SubcConsumer::connect(path, ConsumerOptions::default()).await else {
        return Err(Outcome::Retry);
    };
    let target = RouteTarget::ManagementSurface {
        module_id: address.module.clone(),
    };
    let options = CallOptions {
        timeout: Duration::from_secs(5),
        route_retry_deadline: Duration::from_secs(5),
        consumer_identity: Some(principal),
        role_versions: Some(std::collections::BTreeMap::from([(
            "llm-runner".into(),
            "v1".into(),
        )])),
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
                Err(_) => Err(Outcome::Retry),
            }
        }
        Err(CallError::Module(error)) => Err(refusal(&error.code)),
        Err(_) => Err(Outcome::Retry),
    };
    let _ = consumer.close().await;
    result
}

pub(super) fn enqueue(registry: crate::bash_background::BgTaskRegistry, frame: PushFrame) -> bool {
    let (task_id, owner) = match &frame {
        PushFrame::BashCompleted(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashLongRunning(frame) => (&frame.task_id, &frame.launching_identity),
        PushFrame::BashPatternMatch(frame) => (&frame.task_id, &frame.launching_identity),
        _ => return false,
    };
    let Some(owner) = owner.clone().filter(is_runner) else {
        return false;
    };
    let path = CONNECTION.get().map(|path| path.read().unwrap().clone());
    #[cfg(test)]
    let path =
        path.or_else(|| test_endpoint::contains(&format!("aft:bash:{task_id}")).then(PathBuf::new));
    let Some(path) = path else {
        return false;
    };
    let task_id = task_id.clone();
    let queued = enqueue_work(move || {
        let _identity = crate::bash_background::task_identity_scope(Some(owner.clone()));
        let Some(metadata) = registry.notice_metadata(&task_id) else {
            return;
        };
        let raw = metadata
            .launching_raw_harness
            .clone()
            .unwrap_or_else(|| owner.harness.clone());
        let Some(address) = Address::new(&owner, raw, metadata.scope.as_ref()) else {
            return;
        };
        let (id, text, completion) = match &frame {
            PushFrame::BashCompleted(_) => {
                let value = serde_json::to_value(&frame).unwrap();
                (
                    format!("aft:bash:{task_id}"),
                    super::bash_notice_text::completion(&value),
                    true,
                )
            }
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
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let no_idle_grant = completion
            && !address.worker
            && metadata.runner_completion_notice.is_none()
            && runtime.block_on(read_idle_start_grant(&path, &address)) == Some(false);
        let request = request(id, text, completion, address.worker || no_idle_grant);
        let request = if completion {
            match registry.freeze_runner_completion_notice(&owner.session, &task_id, request) {
                Ok(request) => request,
                Err(error) => {
                    log::warn!("runner notice task={task_id} could not freeze send: {error}");
                    return;
                }
            }
        } else {
            request
        };
        let first = if completion {
            metadata.runner_completion_offers
        } else {
            0
        };
        let settled = deliver(
            &request.send_id,
            first,
            |_| {
                if completion {
                    let age = crate::bash_background::persistence::unix_millis()
                        .saturating_sub(metadata.finished_at.unwrap_or(metadata.started_at));
                    if age >= MAX_AGE_MS {
                        return Some(Outcome::Final("expired".into()));
                    }
                    let Some(offers) = registry.reserve_runner_completion_offer(&task_id) else {
                        return None;
                    };
                    if registry
                        .persist_runner_completion_offer(&owner.session, &task_id, offers)
                        .is_err()
                    {
                        return None;
                    }
                }
                Some(runtime.block_on(send(&path, &address, &request)))
            },
            std::thread::sleep,
        );
        if completion && settled {
            registry.ack_completions_for_session(Some(&owner.session), &[task_id]);
        }
    });
    queued
}
