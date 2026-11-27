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
use crate::bash_background::registry::{BgCompletion, BgTask};
use crate::bash_background::{BgTaskRegistry, OpenOutcome};
use crate::protocol::PushFrame;

const ORCHESTRATOR: &str = "prefrontal";
const CALL_TIMEOUT: Duration = Duration::from_secs(1);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
const CARD_PREPARATION_LIMIT: Duration = Duration::from_millis(50);
const CLEANUP_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
enum CleanupObservation {
    Unconfirmed,
    Open,
    Ended,
}

#[derive(Default)]
struct UnknownWindow {
    elapsed: Duration,
    previous: Option<tokio::time::Instant>,
}

impl UnknownWindow {
    fn unknown(&mut self, now: tokio::time::Instant) -> bool {
        if let Some(previous) = self.previous.replace(now) {
            self.elapsed += now.saturating_duration_since(previous);
        }
        self.elapsed >= CLEANUP_LIFETIME
    }

    fn interrupted(&mut self) {
        self.previous = None;
    }
}

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

#[derive(Clone)]
struct SessionScope {
    identity: BindIdentity,
    scope: ScopeSelector,
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
    terminal:
        parking_lot::Mutex<HashMap<String, Option<tokio::sync::watch::Sender<CleanupObservation>>>>,
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
        let terminal = !matches!(event, Event::Progress(_));
        let cleanup = matches!(event, Event::CleanupOpen { .. });
        let scope_ends = cleanup.then(|| registry.watch_awaited_scope_ends());
        let (signal, mut observations) = match &event {
            Event::CleanupOpen { listed } => {
                let (tx, rx) = tokio::sync::watch::channel(if *listed {
                    CleanupObservation::Open
                } else {
                    CleanupObservation::Unconfirmed
                });
                (Some(tx), Some(rx))
            }
            _ => (None, None),
        };
        let until_ended = observations.clone();
        let token = CancellationToken::new();
        let mut pending_progress = None;
        if terminal {
            match self.terminal.lock().entry(key.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(signal);
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    if matches!(event, Event::CleanupOpen { listed: true }) {
                        if let Some(signal) = entry.get() {
                            signal.send_replace(CleanupObservation::Open);
                        }
                    }
                    return;
                }
            }
            if matches!(event, Event::Cancel | Event::CleanupOpen { .. }) {
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
            // A final output scan can report a match just before exit. Give
            // that progress call a chance to complete before sending finish.
            if let Some(progress) = pending_progress {
                let _ = tokio::time::timeout(CALL_TIMEOUT, progress.cancelled()).await;
            }
            let mut delay = FIRST_RETRY_DELAY;
            let mut retry_log = RetryLog::new(!persisted);
            let mut request = None;
            let mut card_choice = None;
            let mut activation = None;
            let mut unknown = UnknownWindow::default();
            let sending = async {
            loop {
                let guard = registry.clone();
                let retained = task.clone();
                let Ok((live, saved)) = tokio::task::spawn_blocking(move || {
                    let live = guard.awaited_work_live(&retained);
                    let saved = if live && !persisted {
                        guard.persist_awaited_decision(&retained)
                    } else {
                        Ok(())
                    };
                    (live, saved)
                })
                .await
                else {
                    break;
                };
                if token.is_cancelled() || !live {
                    break;
                }
                persisted = saved.is_ok();
                if let Err(error) = saved {
                    retry_log.failure(format_args!(
                        "could not persist awaited-work decision for {key}: {error}"
                    ));
                }
                let wire = service.wire().filter(|_| persisted);
                if wire.is_some() && request.is_none() {
                    match service
                         .request_for(&registry, &task, &key, event.clone(), notice.clone(), &mut card_choice)
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
                        Ok(_) => break,
                         Err(Failure::Refused(code)) if cleanup && code == "unknown_key" => {
                            // On another route the timed-out open can still be
                            // committing while its cleanup arrives first.
                             retry_log.failure(format_args!(
                                "{} waiting for uncertain open of {key}: {code}",
                                req.method()
                             ));
                             let may_expire = observations.as_ref().is_some_and(|rx| *rx.borrow() == CleanupObservation::Unconfirmed);
                             if may_expire && unknown.unknown(tokio::time::Instant::now()) {
                                  // Reconciliation may have found that the
                                  // orchestrator still holds the item. Wait for
                                  // that observation before expiring its cancel.
                                 let fence = service.registrations.lock().await;
                                 if observations.as_ref().is_some_and(|rx| *rx.borrow() == CleanupObservation::Unconfirmed) {
                                     log::warn!("abandoning uncertain awaited open cleanup for {key}: unknown-key retry window expired");
                                     return Some(fence);
                                 }
                             }
                        }
                        Err(Failure::Refused(code)) => {
                            log::warn!("{} refused for {key}: {code}", req.method());
                            break;
                        }
                         Err(Failure::Transport(error) | Failure::Unknown(error)) => {
                             unknown.interrupted();
                             retry_log.failure(format_args!(
                                 "{} transport failed for {key}: {error}",
                                 req.method()
                             ));
                         }
                    }
                 } else if persisted && service.wire().is_none() {
                     unknown.interrupted();
                    retry_log.failure(format_args!("awaited-work transport unavailable for {key}"));
                }
                 tokio::select! {
                     _ = token.cancelled() => break,
                     _ = wait_for_cleanup_change(observations.as_mut()), if cleanup => {
                         unknown = UnknownWindow::default();
                         delay = FIRST_RETRY_DELAY;
                         continue;
                     }
                     _ = tokio::time::sleep(delay) => {},
                }
                delay = (delay * 2).min(MAX_RETRY_DELAY);
            }
            None };
            let _expiry_fence = if let Some(scope_ends) = scope_ends {
                tokio::select! {
                    biased;
                    _ = wait_for_cleanup_end(until_ended.expect("cleanup observation")) => None,
                    _ = wait_for_revocation(&registry, &task, scope_ends) => None,
                    expiry_fence = sending => expiry_fence,
                }
            } else {
                sending.await
            };
            if terminal {
                // A confirmed link may be announced after finish. Its storage
                // never delays the orchestrator's release of the worker.
                if let Some(job) = activation { let _ = job.await; }
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

    pub(crate) fn replay_terminal(&self, run: impl FnOnce() + Send + 'static) {
        self.runtime.spawn_blocking(run);
    }
}

async fn wait_for_cleanup_change(
    changes: Option<&mut tokio::sync::watch::Receiver<CleanupObservation>>,
) {
    if let Some(changes) = changes {
        let _ = changes.changed().await;
    }
}

async fn wait_for_cleanup_end(mut changes: tokio::sync::watch::Receiver<CleanupObservation>) {
    loop {
        if *changes.borrow_and_update() == CleanupObservation::Ended
            || changes.changed().await.is_err()
        {
            return;
        }
    }
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

#[derive(Clone)]
pub(crate) enum Event {
    Finish { outcome: Outcome, summary: String },
    Cancel,
    CleanupOpen { listed: bool },
    Progress(String),
}

pub(crate) fn completion_summary(completion: &BgCompletion) -> String {
    let value = serde_json::to_value(completion).expect("serializable bash completion");
    let text = super::bash_notice_text::completion(&value);
    bounded(&text, 2000)
}

pub(crate) fn bounded(text: &str, limit: usize) -> String {
    let end = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(index, _)| index);
    text[..end].to_owned()
}

pub(crate) fn owns_kill_reply(task_scope: &ScopeStamp) -> bool {
    super::secret_consent::current_identity().is_some_and(|caller| {
        let scope = caller
            .run_owner
            .as_ref()
            .and_then(|owner| owner.owner_route.scope.as_ref())
            .or(caller.scope.as_ref());
        scope.is_some_and(|scope| {
            scope.kind == ScopeKind::Worker
                && scope.owner == task_scope.owner
                && scope.scope_ref == task_scope.scope_ref
                && scope.scope_epoch == task_scope.scope_epoch
        })
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
        registry.prepare_awaited_spawn(&self.service)
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
            let request = Request::Open(OpenParams {
                instance: self.service.instance.clone(),
                key: key.into(),
                label: self.label.clone(),
            });
            match Service::call::<ItemReply>(wire.as_ref(), &request, Some(&self.scope)).await {
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
    {
        let terminal = service.terminal.lock();
        for item in &listed.items {
            if let Some(Some(observation)) = terminal.get(&item.key) {
                observation.send_replace(CleanupObservation::Open);
            }
        }
    }
    let executor = executor.clone();
    let (known, registries) = tokio::task::spawn_blocking(move || {
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
        for registry in &registries {
            known.extend(
                registry
                    .pending_awaited_open_cleanups()
                    .into_iter()
                    .map(|key| (key, registry.clone())),
            );
        }
        (known, registries)
    })
    .await
    .map_err(|error| Failure::Transport(error.to_string()))?;
    // Terminal retries own their decisions even after removal from the task
    // index. Keep them, and keep uncertain opens that have not appeared in list.
    let mut keep: HashSet<_> = service.terminal.lock().keys().cloned().collect();
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
    {
        let terminal = service.terminal.lock();
        for key in &keep {
            if let Some(Some(observation)) = terminal.get(key) {
                observation.send_replace(if unknown.contains(key) {
                    CleanupObservation::Ended
                } else {
                    CleanupObservation::Open
                });
            }
        }
    }
    keep.retain(|key| !unknown.contains(key));
    settle_omitted_items(registries, Arc::new(keep)).await;
    let service_for_replay = service.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for (key, registry) in known {
            if !unknown.contains(&key) {
                registry.resume_awaited_work(&key, &service_for_replay);
            }
        }
    })
    .await;
    *service.wire.write() = Some(wire);
    Ok(())
}

// A successful list is complete: Prefrontal commits each item before answering
// open, and refuses as busy until its store is ready after restart. After both
// list and sync have succeeded, an omitted item has already ended (or its open
// never committed); settling locally must not send another notice.
async fn settle_omitted_items(registries: Vec<BgTaskRegistry>, retained: Arc<HashSet<String>>) {
    for registry in registries {
        registry.settle_missing_awaited(retained.clone()).await;
    }
}

#[cfg(test)]
#[path = "awaited_work_tests.rs"]
mod tests;
