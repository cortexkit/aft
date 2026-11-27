//! AFT's module-authenticated connection to the agent orchestrator.

use cortexkit_role_agent_orchestrator::{params::*, replies::*, request::Request, AWAITED_WORK};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;
use subc_client_rs::{CallError, CallOptions, ConsumerOptions, RouteHandle, SubcConsumer};
use subc_protocol::scope::{ScopeKind, ScopeSelector, ScopeStamp};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio_util::sync::CancellationToken;

use super::rich_ui::display::AwaitedCards;
use crate::bash_background::registry::awaited_work::{Event, OpenReplay, SessionScope, Sink};
use crate::bash_background::registry::BgTask;
use crate::bash_background::{BgTaskRegistry, OpenOutcome};
use crate::protocol::PushFrame;

const ORCHESTRATOR: &str = "prefrontal";
const CALL_TIMEOUT: Duration = Duration::from_secs(1);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
const FIRST_CAPACITY_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_CAPACITY_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const CARD_PREPARATION_LIMIT: Duration = Duration::from_millis(50);

struct RetryLog(Option<tokio::time::Instant>);

impl RetryLog {
    fn new(already_logged: bool) -> Self {
        Self(already_logged.then(tokio::time::Instant::now))
    }

    fn due(&mut self, now: tokio::time::Instant) -> bool {
        if self
            .0
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(60))
        {
            self.0 = Some(now);
            true
        } else {
            false
        }
    }

    fn failure(&mut self, message: std::fmt::Arguments<'_>) {
        if self.due(tokio::time::Instant::now()) {
            log::warn!("{message}");
        }
    }
}

static SERVICE: parking_lot::RwLock<Option<Arc<Service>>> = parking_lot::RwLock::new(None);

#[derive(Debug)]
enum Failure {
    Refused(String),
    Transport(String),
    Unknown(String),
}

impl Failure {
    fn retry_discovery(&self) -> bool {
        match self {
            Self::Refused(code) => subc_protocol::error_codes::is_retryable_route_open(code),
            Self::Transport(_) | Self::Unknown(_) => true,
        }
    }
}

impl From<CallError> for Failure {
    fn from(error: CallError) -> Self {
        match error {
            CallError::Module(body) => Self::Refused(body.code),
            other
                if other.never_forwarded() == Some(true)
                    || matches!(
                        other,
                        CallError::StaleRouteHandle(_) | CallError::DaemonTooOld { .. }
                    ) =>
            {
                Self::Transport(other.to_string())
            }
            other => Self::Unknown(other.to_string()),
        }
    }
}

fn open_request(open: &OpenReplay, instance: &str, key: &str) -> Request {
    Request::Open(OpenParams {
        instance: instance.into(),
        key: key.into(),
        label: open.label.clone(),
    })
}

#[async_trait::async_trait]
trait Wire: Send + Sync {
    async fn request(
        &self,
        body: Vec<u8>,
        scope: Option<&SessionScope>,
    ) -> Result<Vec<u8>, Failure>;
}

struct DaemonWire {
    consumer: Arc<SubcConsumer>,
    management: RouteHandle,
}

#[async_trait::async_trait]
impl Wire for DaemonWire {
    async fn request(
        &self,
        body: Vec<u8>,
        scope: Option<&SessionScope>,
    ) -> Result<Vec<u8>, Failure> {
        let Some(scope) = scope else {
            return self
                .consumer
                .request(&self.management, body, options())
                .await
                .map_err(Into::into);
        };
        let route = self
            .consumer
            .open_route_scoped(
                target(),
                scope.identity.clone(),
                scope.scope.clone(),
                options(),
            )
            .await
            .map_err(|error| match error {
                other
                    if other.route_open_refusal().is_some_and(|body| {
                        body.code == subc_protocol::error_codes::SCOPE_ENDED
                    }) =>
                {
                    Failure::Refused("session_ended".into())
                }
                CallError::Module(body) => Failure::Refused(body.code),
                other => Failure::Transport(other.to_string()),
            })?;
        let result = self.consumer.request(&route, body, options()).await;
        let _ = tokio::time::timeout(
            CALL_TIMEOUT,
            self.consumer.close_handle(&route, Default::default()),
        )
        .await;
        result.map_err(Into::into)
    }
}

fn target() -> RouteTarget {
    RouteTarget::ManagementSurface {
        module_id: ORCHESTRATOR.into(),
    }
}

fn options() -> CallOptions {
    CallOptions {
        timeout: CALL_TIMEOUT,
        route_retry_deadline: CALL_TIMEOUT,
        ..CallOptions::default()
    }
}

fn process_instance() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut random = [0; 16];
        getrandom::fill(&mut random).expect("random awaited-work instance");
        format!("aft-{:032x}", u128::from_ne_bytes(random))
    })
}

pub(crate) struct Service {
    instance: String,
    runtime: tokio::runtime::Handle,
    wire: parking_lot::RwLock<Option<Arc<dyn Wire>>>,
    registrations: tokio::sync::Mutex<()>,
    // True for pending cleanups that stay tracked when sync returns unknown.
    terminal: parking_lot::Mutex<HashMap<String, bool>>,
    cleanup_wake: tokio::sync::watch::Sender<()>,
    progress: parking_lot::Mutex<HashMap<String, CancellationToken>>,
    #[cfg(all(test, unix))]
    cards: AwaitedCards,
}

impl Service {
    fn new(instance: &str) -> Self {
        Self {
            instance: instance.into(),
            runtime: tokio::runtime::Handle::current(),
            wire: Default::default(),
            registrations: Default::default(),
            terminal: Default::default(),
            cleanup_wake: tokio::sync::watch::Sender::new(()),
            progress: Default::default(),
            #[cfg(all(test, unix))]
            cards: Default::default(),
        }
    }

    fn wire(&self) -> Option<Arc<dyn Wire>> {
        self.wire.read().clone()
    }

    fn cards(&self) -> AwaitedCards {
        #[cfg(all(test, unix))]
        return self.cards.clone();
        #[cfg(not(all(test, unix)))]
        AwaitedCards::Module
    }

    async fn call<T: DeserializeOwned>(
        wire: &dyn Wire,
        req: &Request,
        scope: Option<&SessionScope>,
    ) -> Result<T, Failure> {
        req.validate()
            .map_err(|error| Failure::Refused(error.code.as_str().to_owned()))?;
        let body = serde_json::to_vec(req).expect("serializable awaited-work request");
        let reply = wire.request(body, scope).await?;
        serde_json::from_slice(&reply).map_err(|error| Failure::Unknown(error.to_string()))
    }

    pub(crate) fn submit(
        self: &Arc<Self>,
        registry: BgTaskRegistry,
        task: Arc<BgTask>,
        key: String,
        event: Event,
        notice: Option<PushFrame>,
        mut persisted: bool,
    ) {
        if let Event::CleanupOpen { replay } = event {
            self.submit_cleanup(registry, task, key, replay, persisted);
            return;
        }
        let terminal = !matches!(event, Event::Progress(_));
        let token = CancellationToken::new();
        let mut pending_progress = None;
        if terminal {
            {
                let mut terminal = self.terminal.lock();
                let std::collections::hash_map::Entry::Vacant(entry) = terminal.entry(key.clone())
                else {
                    return;
                };
                entry.insert(false);
            }
            if matches!(event, Event::Cancel) {
                if let Some(progress) = self.progress.lock().remove(&key) {
                    progress.cancel();
                }
            } else {
                pending_progress = self.progress.lock().get(&key).cloned();
            }
        } else if let Some(older) = self.progress.lock().insert(key.clone(), token.clone()) {
            older.cancel();
        }
        let service = Arc::clone(self);
        self.runtime.spawn(async move {
            // Give any in-flight progress update a chance to arrive before finish.
            if let Some(progress) = pending_progress {
                let _ = tokio::time::timeout(CALL_TIMEOUT, progress.cancelled()).await;
            }
            let mut delay = FIRST_RETRY_DELAY;
            let mut retry_log = RetryLog::new(!persisted);
            let mut request = None;
            let mut card_choice = None;
            let mut activation = None;
            loop {
                if token.is_cancelled()
                    || !check_send_state(&registry, &task, &key, &mut persisted, &mut retry_log)
                        .await
                {
                    break;
                }
                let wire = service.wire().filter(|_| persisted);
                if wire.is_some() && request.is_none() {
                    match service
                        .request_for(
                            &registry,
                            &task,
                            &key,
                            event.clone(),
                            notice.clone(),
                            &mut card_choice,
                        )
                        .await
                    {
                        Ok(prepared) => {
                            activation = activate_linked_card(service.cards(), &prepared);
                            request = Some(prepared);
                        }
                        Err(error) => retry_log.failure(format_args!(
                            "could not freeze awaited-work result for {key}: {error}"
                        )),
                    }
                }
                if let (Some(wire), Some(req)) = (wire, request.as_ref()) {
                    let result = tokio::select! {
                        _ = token.cancelled() => break,
                        result = Service::call::<ItemReply>(wire.as_ref(), req, None) => result,
                    };
                    match result {
                        Ok(_) => {
                            if terminal {
                                service.cleanup_wake.send_replace(());
                            }
                            break;
                        }
                        Err(Failure::Refused(code))
                            if terminal
                                && subc_protocol::error_codes::is_retryable_route_open(&code) =>
                        {
                            retry_log.failure(format_args!(
                                "{} route refused for {key}, will retry: {code}",
                                req.method()
                            ));
                        }
                        Err(Failure::Refused(code)) => {
                            log::warn!("{} refused for {key}: {code}", req.method());
                            break;
                        }
                        Err(Failure::Transport(error) | Failure::Unknown(error)) => {
                            retry_log.failure(format_args!(
                                "{} transport failed for {key}: {error}",
                                req.method()
                            ));
                        }
                    }
                } else if persisted && service.wire().is_none() {
                    retry_log.failure(format_args!("awaited-work transport unavailable for {key}"));
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {},
                }
                delay = (delay * 2).min(MAX_RETRY_DELAY);
            }
            if terminal {
                // A confirmed link can be announced after the worker is released.
                if let Some(job) = activation {
                    let _ = job.await;
                }
                let _ =
                    tokio::task::spawn_blocking(move || registry.retire_awaited_work(&task)).await;
                service.terminal.lock().remove(&key);
            } else {
                let mut progress = service.progress.lock();
                if !token.is_cancelled() {
                    progress.remove(&key);
                }
                token.cancel();
            }
        });
    }

    fn submit_cleanup(
        self: &Arc<Self>,
        registry: BgTaskRegistry,
        task: Arc<BgTask>,
        key: String,
        mut replay: Option<OpenReplay>,
        mut persisted: bool,
    ) {
        {
            let mut terminal = self.terminal.lock();
            let std::collections::hash_map::Entry::Vacant(entry) = terminal.entry(key.clone())
            else {
                return;
            };
            entry.insert(true);
        }
        if let Some(progress) = self.progress.lock().remove(&key) {
            progress.cancel();
        }
        let mut wake = self.cleanup_wake.subscribe();
        let scope_ends = registry.watch_awaited_scope_ends();
        let service = self.clone();
        self.runtime.spawn(async move {
            let cancel = Request::Cancel(CancelParams { instance: service.instance.clone(), key: key.clone() });
            let mut log = RetryLog::new(!persisted);
            let mut delay = FIRST_RETRY_DELAY;
            let mut capacity_delay = FIRST_CAPACITY_RETRY_DELAY;
            let work = async {
                loop {
                    if !check_send_state(&registry, &task, &key, &mut persisted, &mut log).await { break; }
                    if persisted {
                        if let Some(open) = &replay {
                            let result = {
                                let _registration = service.registrations.lock().await;
                                if let Some(wire) = service.wire() {
                                    let _ = wake.borrow_and_update();
                                    let body = open_request(open, &service.instance, &key);
                                    Some(Service::call::<ItemReply>(wire.as_ref(), &body, Some(&open.scope)).await)
                                } else {
                                    None
                                }
                            };
                            match result {
                                Some(Ok(reply)) if reply.item.key == key => {
                                    let (registry_write, task_write) = (registry.clone(), task.clone());
                                    let saved = tokio::task::spawn_blocking(move || registry_write.record_confirmed_cleanup_open(&task_write)).await;
                                    if !matches!(saved, Ok(Ok(()))) {
                                        log.failure(format_args!("could not persist confirmation of awaited open for {key}: {saved:?}"));
                                    }
                                    // If the write failed, a restart can replay open.
                                    // The orchestrator refuses an open whose key was cancelled.
                                    replay = None;
                                    delay = FIRST_RETRY_DELAY;
                                    continue;
                                }
                                Some(Err(Failure::Refused(code))) if matches!(code.as_str(), "key_reused" | "session_ended") => break,
                                Some(Err(Failure::Refused(code))) if code == "too_many_open" => {
                                    // A slot can also free without an AFT finish or cancel.
                                    tokio::select! {
                                        changed = wake.changed() => {
                                            if changed.is_err() { break; }
                                            capacity_delay = FIRST_CAPACITY_RETRY_DELAY;
                                        }
                                        _ = tokio::time::sleep(capacity_delay) => {
                                            capacity_delay = (capacity_delay * 2).min(MAX_CAPACITY_RETRY_DELAY);
                                        }
                                    }
                                    delay = FIRST_RETRY_DELAY;
                                    continue;
                                }
                                Some(Ok(_)) => log.failure(format_args!("awaited open replay returned a different key for {key}")),
                                Some(Err(error)) => log.failure(format_args!("awaited open replay failed for {key}: {error:?}")),
                                None => {},
                            }
                        } else if let Some(wire) = service.wire() {
                            match Service::call::<ItemReply>(wire.as_ref(), &cancel, None).await {
                                Ok(reply) if reply.item.key == key => {
                                    service.cleanup_wake.send_replace(());
                                    break;
                                }
                                Err(Failure::Refused(code)) if code == "session_ended" => break,
                                Ok(_) => log.failure(format_args!("awaited cleanup cancel returned a different key for {key}")),
                                Err(error) => log.failure(format_args!("awaited cleanup cancel failed for {key}: {error:?}")),
                            }
                        }
                    }
                    tokio::select! {
                        changed = wake.changed() => {
                            if changed.is_err() { break; }
                            delay = FIRST_RETRY_DELAY;
                            capacity_delay = FIRST_CAPACITY_RETRY_DELAY;
                        }
                        _ = tokio::time::sleep(delay) => delay = (delay * 2).min(MAX_RETRY_DELAY),
                    }
                }
            };
            tokio::select! {
                biased;
                _ = wait_for_revocation(&registry, &task, scope_ends) => {},
                _ = work => {},
            }
            let _ = tokio::task::spawn_blocking(move || registry.retire_awaited_work(&task)).await;
            service.terminal.lock().remove(&key);
        });
    }

    async fn request_for(
        &self,
        registry: &BgTaskRegistry,
        task: &Arc<BgTask>,
        key: &str,
        event: Event,
        notice: Option<PushFrame>,
        card_choice: &mut Option<Option<String>>,
    ) -> std::io::Result<Request> {
        let is_finish = matches!(event, Event::Finish { .. });
        let saved = if is_finish {
            let (registry, task) = (registry.clone(), task.clone());
            tokio::task::spawn_blocking(move || registry.awaited_finish_notice(&task))
                .await
                .map_err(std::io::Error::other)?
        } else {
            None
        };
        let id = if let Some(id) = saved {
            id
        } else {
            if card_choice.is_none() {
                let id = if let Some(frame) = notice {
                    let cards = self.cards();
                    card_before_deadline(
                        cards.clone(),
                        stage_notice_record(cards, registry.clone(), key.into(), frame),
                    )
                    .await
                } else {
                    None
                };
                *card_choice = Some(id);
            }
            let id = card_choice.clone().flatten();
            if is_finish {
                // A restart must replay the same pointer, even when there was
                // no room for a card on the original attempt.
                let (registry, task) = (registry.clone(), task.clone());
                tokio::task::spawn_blocking(move || {
                    registry.freeze_awaited_finish_notice(&task, id)
                })
                .await
                .map_err(std::io::Error::other)??
            } else {
                id
            }
        };
        if let Some(unlinked) = card_choice
            .take()
            .flatten()
            .filter(|proposed| Some(proposed) != id.as_ref())
        {
            let cards = self.cards();
            tokio::spawn(async move { cards.discard(unlinked).await });
        }
        Ok(self.request_body(key, event, id))
    }

    fn request_body(&self, key: &str, event: Event, id: Option<String>) -> Request {
        let notice_record = id.map(|id| NoticeRecord {
            writer: "aft".into(),
            id,
        });
        match event {
            Event::Finish { outcome, summary } => Request::Finish(FinishParams {
                instance: self.instance.clone(),
                key: key.into(),
                outcome,
                summary,
                more: Some(format!("full output: bash_status({{ taskId: \"{key}\" }})")),
                notice_record,
            }),
            Event::Cancel | Event::CleanupOpen { .. } => Request::Cancel(CancelParams {
                instance: self.instance.clone(),
                key: key.into(),
            }),
            Event::Progress(text) => Request::Progress(ProgressParams {
                instance: self.instance.clone(),
                key: key.into(),
                text,
                notice_record,
            }),
        }
    }
}

impl Sink for Service {
    fn submit(
        self: Arc<Self>,
        registry: BgTaskRegistry,
        task: Arc<BgTask>,
        key: String,
        event: Event,
        notice: Option<PushFrame>,
        persisted: bool,
    ) {
        Service::submit(&self, registry, task, key, event, notice, persisted);
    }

    fn replay_terminal(&self, run: Box<dyn FnOnce() + Send>) {
        self.runtime.spawn_blocking(run);
    }
}

async fn check_send_state(
    registry: &BgTaskRegistry,
    task: &Arc<BgTask>,
    key: &str,
    persisted: &mut bool,
    log: &mut RetryLog,
) -> bool {
    let (registry, task, already_saved) = (registry.clone(), task.clone(), *persisted);
    let Ok((live, saved)) = tokio::task::spawn_blocking(move || {
        let live = registry.awaited_work_live(&task);
        let saved = if live && !already_saved {
            registry.persist_awaited_decision(&task)
        } else {
            Ok(())
        };
        (live, saved)
    })
    .await
    else {
        return false;
    };
    *persisted = saved.is_ok();
    if let Err(error) = saved {
        log.failure(format_args!(
            "could not persist awaited-work decision for {key}: {error}"
        ));
    }
    live
}

async fn wait_for_revocation(
    registry: &BgTaskRegistry,
    task: &Arc<BgTask>,
    mut changes: tokio::sync::watch::Receiver<()>,
) {
    while changes.changed().await.is_ok() {
        let (registry, task) = (registry.clone(), task.clone());
        if !tokio::task::spawn_blocking(move || registry.awaited_work_live(&task))
            .await
            .unwrap_or(false)
        {
            return;
        }
    }
}

async fn card_before_deadline(
    cards: AwaitedCards,
    prepare: impl std::future::Future<Output = Option<String>> + Send + 'static,
) -> Option<String> {
    let mut job = tokio::spawn(prepare);
    match tokio::time::timeout(CARD_PREPARATION_LIMIT, &mut job).await {
        Ok(result) => result.ok().flatten(),
        Err(_) => {
            // SQLite can finish a blocking write after the deadline. The
            // caller has chosen no pointer; discard any card the job returns.
            tokio::spawn(async move {
                if let Ok(Some(id)) = job.await {
                    cards.discard(id).await;
                }
            });
            None
        }
    }
}

fn activate_linked_card(
    cards: AwaitedCards,
    request: &Request,
) -> Option<tokio::task::JoinHandle<()>> {
    let pointer = match request {
        Request::Finish(params) => params.notice_record.as_ref(),
        Request::Progress(params) => params.notice_record.as_ref(),
        _ => None,
    }?;
    Some(cards.announce(pointer.id.clone()))
}

async fn stage_notice_record(
    cards: AwaitedCards,
    registry: BgTaskRegistry,
    key: String,
    frame: PushFrame,
) -> Option<String> {
    let record = tokio::task::spawn_blocking(move || {
        let metadata = registry.notice_metadata(&key)?;
        super::rich_ui::notices::for_awaited_work(
            &registry,
            &frame,
            &metadata,
            crate::bash_background::persistence::unix_millis(),
        )
    })
    .await
    .ok()??;
    cards.commit(record).await
}

pub(super) fn current_kill_reply_scope() -> Option<ScopeStamp> {
    super::secret_consent::current_identity().and_then(|caller| {
        caller
            .run_owner
            .as_ref()
            .and_then(|owner| owner.owner_route.scope.as_ref())
            .or(caller.scope.as_ref())
            .cloned()
    })
}

#[derive(Clone)]
pub(super) struct Candidate {
    service: Arc<Service>,
    scope: SessionScope,
    label: String,
}

impl Candidate {
    pub(super) fn for_call(
        scope: Option<&ScopeStamp>,
        session: &str,
        args: &Value,
    ) -> Option<Self> {
        let scope = scope.filter(|scope| scope.kind == ScopeKind::Worker)?;
        if !["background", "pty"].iter().any(|key| {
            args.get(*key)
                .and_then(crate::subc_translate::model_boolean)
                == Some(true)
        }) {
            return None;
        }
        let service = current_service()?;
        service.wire()?;
        let label = args
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| {
                args.get("command")
                    .and_then(Value::as_str)
                    .and_then(|cmd| cmd.lines().next())
            })
            .unwrap_or_default();
        let label: String = label
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}'))
            .scan(0, |count, c| {
                *count += 1;
                (*count <= 200).then_some(c)
            })
            .collect::<String>();
        Some(Self {
            service,
            scope: SessionScope {
                identity: BindIdentity::new("aft-awaited-work", session),
                scope: ScopeSelector::new(
                    scope.owner.clone(),
                    scope.scope_ref.clone(),
                    Some(scope.scope_epoch),
                ),
            },
            label: if label.trim().is_empty() {
                "Background shell".into()
            } else {
                label
            },
        })
    }

    pub(super) fn prepare(
        &self,
        registry: &BgTaskRegistry,
    ) -> crate::bash_background::AwaitedSpawn {
        registry.prepare_awaited_spawn(self.service.clone())
    }

    pub(super) async fn open(
        &self,
        registry: &BgTaskRegistry,
        key: &str,
        caller_waits: bool,
    ) -> bool {
        let task_registry = registry.clone();
        let task = key.to_owned();
        if !tokio::task::spawn_blocking(move || task_registry.has_awaited_open_pending(&task))
            .await
            .unwrap_or(false)
        {
            return false;
        }
        let gate = if caller_waits {
            tokio::time::timeout(CALL_TIMEOUT, self.service.registrations.lock())
                .await
                .ok()
        } else {
            None
        };
        let mut outcome = OpenOutcome::NotOpened;
        if let Some(wire) = self.service.wire().filter(|_| gate.is_some()) {
            let open = OpenReplay {
                scope: self.scope.clone(),
                label: self.label.clone(),
            };
            let (registry_write, attempt_key, saved_open) =
                (registry.clone(), key.to_owned(), open.clone());
            // Save the scoped request before it can reach Prefrontal. A restart
            // during the call then has the exact key, session, scope and label.
            let saved = tokio::task::spawn_blocking(move || {
                registry_write.record_awaited_open_attempt(&attempt_key, saved_open)
            })
            .await;
            if matches!(saved, Ok(Ok(()))) {
                let request = open_request(&open, &self.service.instance, key);
                match Service::call::<ItemReply>(wire.as_ref(), &request, Some(&open.scope)).await {
                    Ok(reply) if reply.item.key != key => outcome = OpenOutcome::Unknown,
                    Ok(reply) if !reply.item.state.is_ended() => outcome = OpenOutcome::Opened,
                    Ok(_) => {}
                    Err(Failure::Refused(code)) => {
                        log::warn!("{} refused for {key}: {code}", request.method())
                    }
                    Err(Failure::Transport(error)) => {
                        log::debug!("{} transport failed for {key}: {error}", request.method())
                    }
                    Err(Failure::Unknown(error)) => {
                        log::warn!("{} outcome unknown for {key}: {error}", request.method());
                        outcome = OpenOutcome::Unknown;
                    }
                }
            } else {
                log::warn!("could not persist awaited-work open attempt for {key}: {saved:?}");
            }
        }
        let registry = registry.clone();
        let key = key.to_owned();
        // Defer orchestrator reconciliation until the launch reply and the
        // saved state agree on whether the worker gets the ordinary held result.
        tokio::task::spawn_blocking(move || registry.settle_awaited_open(&key, outcome))
            .await
            .unwrap_or(false)
    }
}

fn current_service() -> Option<Arc<Service>> {
    #[cfg(test)]
    if let Some(service) = tests::TEST_SERVICE.with(|slot| slot.borrow().clone()) {
        return Some(service);
    }
    SERVICE.read().clone()
}

pub(super) fn start(path: &Path, executor: Arc<super::Executor>) {
    if super::registration::module_id().is_none() || SERVICE.read().is_some() {
        return;
    }
    let service = Arc::new(Service::new(process_instance()));
    let weak = Arc::downgrade(&service);
    *SERVICE.write() = Some(service);
    tokio::spawn(discover(path.to_path_buf(), executor, weak));
}

async fn discover(path: PathBuf, executor: Arc<super::Executor>, service: Weak<Service>) {
    let mut consumer = None;
    let mut observed = None;
    let mut active = None;
    loop {
        let Some(current) = service.upgrade() else {
            return;
        };
        if consumer.is_none() {
            consumer = super::registration::connect_consumer(
                "awaited_work",
                &path,
                ConsumerOptions::default(),
            )
            .await
            .ok()
            .map(Arc::new);
        }
        if let Some(connection) = &consumer {
            if let Ok(Ok(snapshot)) =
                tokio::time::timeout(CALL_TIMEOUT, connection.spawn_snapshot()).await
            {
                let generation = snapshot
                    .live
                    .iter()
                    .find(|spawn| spawn.module_id == ORCHESTRATOR)
                    .map(|spawn| {
                        (
                            snapshot.cursor.daemon_incarnation.clone(),
                            spawn.spawn_generation,
                        )
                    });
                let mut modules: Vec<_> = snapshot
                    .live
                    .into_iter()
                    .map(|spawn| (spawn.module_id, spawn.spawn_generation))
                    .collect();
                modules.sort_unstable();
                let catalog = (snapshot.cursor.daemon_incarnation, modules);
                if active != generation {
                    *current.wire.write() = None;
                    active = None;
                }
                if generation.is_none() {
                    observed = Some(catalog);
                } else if let Some(supported) = discover_catalog(
                    &mut observed,
                    catalog,
                    refresh(connection, &current, &executor, generation == active),
                )
                .await
                {
                    active = if supported { generation } else { None };
                }
            } else {
                consumer = None;
                observed = None;
                active = None;
                *current.wire.write() = None;
            }
        }
        drop(current);
        tokio::time::sleep(DISCOVERY_INTERVAL).await;
    }
}

async fn discover_catalog<C: PartialEq>(
    observed: &mut Option<C>,
    catalog: C,
    refresh: impl std::future::Future<Output = Result<bool, Failure>>,
) -> Option<bool> {
    if observed.as_ref() == Some(&catalog) {
        return None;
    }
    match refresh.await {
        Ok(supported) => {
            *observed = Some(catalog);
            Some(supported)
        }
        Err(error) if error.retry_discovery() => {
            log::debug!("awaited-work discovery will retry: {error:?}");
            None
        }
        Err(error) => {
            log::warn!("awaited-work discovery refused: {error:?}");
            *observed = Some(catalog);
            None
        }
    }
}

async fn refresh(
    consumer: &Arc<SubcConsumer>,
    service: &Arc<Service>,
    executor: &Arc<super::Executor>,
    already_synced: bool,
) -> Result<bool, Failure> {
    let route = consumer
        .open_route(
            target(),
            BindIdentity::new("aft-awaited-work", "aft:awaited-work"),
            options(),
        )
        .await?;
    let wire = Arc::new(DaemonWire {
        consumer: consumer.clone(),
        management: route,
    });
    let reply = wire
        .request(br#"{"method":"role.describe","params":{}}"#.to_vec(), None)
        .await?;
    let reply: Value =
        serde_json::from_slice(&reply).map_err(|error| Failure::Transport(error.to_string()))?;
    let supported = reply
        .get("capabilities")
        .and_then(Value::as_array)
        .is_some_and(|tags| tags.iter().any(|tag| tag == AWAITED_WORK));
    if !supported {
        *service.wire.write() = None;
    } else if !already_synced || service.wire().is_none() {
        reconcile(service, wire, executor).await?;
    }
    Ok(supported)
}

async fn reconcile(
    service: &Arc<Service>,
    wire: Arc<dyn Wire>,
    executor: &Arc<super::Executor>,
) -> Result<(), Failure> {
    let _gate = service.registrations.lock().await;
    let listed: ListReply =
        Service::call(wire.as_ref(), &Request::List(ListParams::default()), None).await?;
    let executor = executor.clone();
    let (known, registries, mut cleanups) = tokio::task::spawn_blocking(move || {
        let registries: Vec<_> = executor
            .actor_contexts()
            .iter()
            .map(|actor| actor.bash_background().clone())
            .collect();
        let mut known = listed
            .items
            .into_iter()
            .filter_map(|item| {
                registries
                    .iter()
                    .find(|registry| registry.knows_awaited_work(&item.key))
                    .map(|registry| (item.key, registry.clone()))
            })
            .collect::<HashMap<_, _>>();
        let mut cleanups = HashSet::new();
        for registry in &registries {
            for key in registry.pending_awaited_open_cleanups() {
                cleanups.insert(key.clone());
                known.insert(key, registry.clone());
            }
        }
        (known, registries, cleanups)
    })
    .await
    .map_err(|error| Failure::Transport(error.to_string()))?;
    let mut keep = HashSet::new();
    {
        let terminal = service.terminal.lock();
        for (key, cleanup) in terminal.iter() {
            keep.insert(key.clone());
            if *cleanup {
                cleanups.insert(key.clone());
            }
        }
    }
    // Keep every uncertain key, including after a capacity refusal. An earlier
    // open can commit between list and sync; omission would end it as lost.
    // Keeping an absent key only returns unknown and does not end its cleanup.
    keep.extend(known.keys().cloned());
    let mut open: Vec<_> = keep.iter().cloned().collect();
    open.sort_unstable();
    let reply: SyncReply = Service::call(
        wire.as_ref(),
        &Request::Sync(SyncParams {
            instance: service.instance.clone(),
            open,
        }),
        None,
    )
    .await?;
    let unknown: HashSet<_> = reply.unknown.into_iter().collect();
    keep.retain(|key| !unknown.contains(key) || cleanups.contains(key));
    settle_omitted_items(registries, Arc::new(keep)).await;
    let service_for_replay = service.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for (key, registry) in known {
            if !unknown.contains(&key) || cleanups.contains(&key) {
                registry.resume_awaited_work(&key, service_for_replay);
            }
        }
    })
    .await;
    *service.wire.write() = Some(wire);
    service.cleanup_wake.send_replace(());
    Ok(())
}

// A successful list is complete: Prefrontal commits each item before answering
// open, and refuses as busy until its store is ready after restart. After both
// list and sync have succeeded, an omitted item has already ended (or its open
// never committed). Unresolved open replays are deliberately retained by the
// caller; an absent item alone cannot prove that an earlier open won't commit.
async fn settle_omitted_items(registries: Vec<BgTaskRegistry>, retained: Arc<HashSet<String>>) {
    for registry in registries {
        registry.settle_missing_awaited(retained.clone()).await;
    }
}

#[cfg(test)]
#[path = "awaited_work_tests.rs"]
mod tests;
