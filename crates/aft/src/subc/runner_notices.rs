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
static ACTIVE: AtomicU64 = AtomicU64::new(0);

struct Active;
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}
fn reserve() -> Option<Active> {
    ACTIVE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < 32).then_some(active + 1)
        })
        .ok()
        .map(|_| Active)
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
        let Some(active) = reserve() else {
            log::warn!("runner warning deferred: notice workers busy");
            return;
        };
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
        let _ = std::thread::Builder::new()
            .name("aft-runner-warning".into())
            .spawn(move || {
                let _active = active;
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
            });
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
    let Some(principal) = crate::launch_nonce::consumer_identity() else {
        return Outcome::Retry;
    };
    let Ok(consumer) = SubcConsumer::connect(path, ConsumerOptions::default()).await else {
        return Outcome::Retry;
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
                    envelope(request),
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
                Ok(bytes) => match cortexkit_role_llm_runner::wire::read_reply::<SendReply>(&bytes)
                {
                    Ok(_) => Outcome::Accepted,
                    Err(_) => Outcome::Retry,
                },
                Err(CallError::Module(error)) => refusal(&error.code),
                Err(_) => Outcome::Retry,
            }
        }
        Err(CallError::Module(error)) => refusal(&error.code),
        Err(_) => Outcome::Retry,
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
    let Some(path) = CONNECTION.get().map(|path| path.read().unwrap().clone()) else {
        return false;
    };
    let Some(active) = reserve() else {
        return false;
    };
    let task_id = task_id.clone();
    let spawned = std::thread::Builder::new()
        .name("aft-runner-notice".into())
        .spawn(move || {
            let _active = active;
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
                    let mut value = serde_json::to_value(&frame).unwrap();
                    value["duration_ms"] = json!(metadata.duration_ms);
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
                        "aft:bash-watch:{}:{}:{}",
                        frame.watch_id, frame.match_offset, frame.reason
                    ),
                    super::bash_notice_text::watch(&serde_json::to_value(frame).unwrap()),
                    false,
                ),
                _ => unreachable!(),
            };
            let request = request(id, text, completion, address.worker);
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
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
                        let Some(offers) = registry.reserve_runner_completion_offer(&task_id)
                        else {
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
    if spawned.is_err() {
        return false;
    }
    true
}
