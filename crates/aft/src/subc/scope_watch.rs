//! Scope lifecycle subscriptions on AFT's existing registered connection.
use super::*;
use subc_client_rs::{ScopeWatchEvent, ScopeWatchSelector};
use subc_protocol::scope::ScopeStamp;

pub(super) type Key = (String, String, u64);
const SELECTORS_PER_WATCH: usize = 256;
const WATCHES_PER_CONNECTION: usize = 16;
pub(super) const BACKSTOP: Duration = Duration::from_secs(5 * 60);
pub(super) const VERIFY_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub(super) const DEBOUNCE: Duration = Duration::from_millis(50);

/// A fixed window coalesces mutation hints without postponing reads forever
/// when calls keep arriving. Explicit end cleanup can request an immediate read.
pub(super) struct Refresh {
    pub(super) at: Option<tokio::time::Instant>,
}
impl Refresh {
    pub(super) fn new() -> Self {
        Self {
            at: Some(tokio::time::Instant::now()),
        }
    }
    pub(super) fn changed(&mut self) {
        self.at
            .get_or_insert(tokio::time::Instant::now() + DEBOUNCE);
    }
    pub(super) fn immediate(&mut self) {
        self.at = Some(tokio::time::Instant::now());
    }
    pub(super) fn due(&self) -> bool {
        self.at.is_some_and(|at| at <= tokio::time::Instant::now())
    }
}

/// Spread verification across a quarter-hour instead of probing all healthy
/// selectors on one timer tick. A delayed worker never catches up in a burst.
#[derive(Default)]
pub(super) struct Verifications {
    due: std::collections::BTreeMap<Key, tokio::time::Instant>,
    active: HashSet<Key>,
    slot: Option<tokio::time::Instant>,
}
impl Verifications {
    pub(super) fn reconcile(&mut self, keys: &HashSet<Key>, held: &HashMap<Key, ScopeStamp>) {
        self.due.retain(|key, _| held.contains_key(key));
        self.active = keys.clone();
        if keys.is_empty() {
            self.slot = None;
            return;
        }
        let mut keys: Vec<_> = keys.iter().cloned().collect();
        keys.sort();
        let spacing = VERIFY_INTERVAL / keys.len() as u32;
        let now = tokio::time::Instant::now();
        for (index, key) in keys.into_iter().enumerate() {
            self.due
                .entry(key)
                .or_insert(now + spacing * (index as u32 + 1));
        }
    }
    pub(super) fn deadline(&self) -> Option<tokio::time::Instant> {
        self.due
            .iter()
            .filter(|(key, _)| self.active.contains(*key))
            .map(|(_, due)| *due)
            .min()
            .map(|due| self.slot.map_or(due, |slot| due.max(slot)))
    }
    pub(super) fn take_due(&mut self) -> Option<Key> {
        let now = tokio::time::Instant::now();
        if self.deadline()? > now {
            return None;
        }
        let key = self
            .due
            .iter()
            .filter(|(key, _)| self.active.contains(*key))
            .min_by_key(|(_, due)| *due)?
            .0
            .clone();
        self.due.insert(key.clone(), now + VERIFY_INTERVAL);
        self.slot = Some(now + VERIFY_INTERVAL / self.active.len() as u32);
        Some(key)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Source {
    Shell,
    Grants,
    Held,
    Parked,
}
impl Source {
    pub(super) const ALL: [Self; 4] = [Self::Shell, Self::Grants, Self::Held, Self::Parked];
    fn index(self) -> usize {
        self as usize
    }
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Grants => "grants",
            Self::Held => "held",
            Self::Parked => "parked",
        }
    }
}
static CHANGED: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);
static REVISIONS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Coalesced hints carry no scope identity or authority. The worker reads the
/// committed state again, so duplicate hints and reconnects are harmless.
pub(crate) fn changed(source: Source) {
    REVISIONS[source.index()].fetch_add(1, Ordering::Release);
    CHANGED.notify_waiters();
}
pub(crate) fn context_changed() {
    for source in Source::ALL {
        changed(source);
    }
}
pub(super) fn notifier() -> &'static tokio::sync::Notify {
    &CHANGED
}
pub(super) fn revisions() -> [u64; 4] {
    std::array::from_fn(|i| REVISIONS[i].load(Ordering::Acquire))
}
#[cfg(all(test, unix))]
pub(crate) fn revision(source: Source) -> u64 {
    REVISIONS[source.index()].load(Ordering::Acquire)
}

#[derive(Default)]
pub(super) struct Inventory {
    pub(super) sources: [HashMap<Key, ScopeStamp>; 4],
    pub(super) failed: HashSet<usize>,
    pub(super) skipped: [u64; 2],
}
impl Inventory {
    pub(super) fn add(&mut self, source: Source, stamps: impl IntoIterator<Item = ScopeStamp>) {
        self.sources[source.index()].extend(
            stamps
                .into_iter()
                .map(|stamp| (scope_describe::stamp_key(&stamp), stamp)),
        );
    }
    pub(super) fn scopes(&self) -> HashMap<Key, ScopeStamp> {
        self.sources
            .iter()
            .rev()
            .flat_map(|source| {
                source
                    .iter()
                    .map(|(key, stamp)| (key.clone(), stamp.clone()))
            })
            .collect()
    }
    pub(super) fn retain_unread(&mut self, previous: &Self) {
        for &source in &self.failed {
            self.sources[source].extend(previous.sources[source].clone());
        }
    }
    pub(super) fn warn_drift(&self, previous: &Self, before: [u64; 4], now: [u64; 4]) -> u64 {
        let mut repaired = 0;
        for source in Source::ALL {
            let i = source.index();
            if before[i] == now[i]
                && (self.sources[i].len() != previous.sources[i].len()
                    || self.sources[i]
                        .keys()
                        .any(|key| !previous.sources[i].contains_key(key)))
            {
                log::warn!(
                    "scope.watch selector census repaired drift source={}",
                    source.name()
                );
                repaired += 1;
            }
        }
        repaired
    }
}

pub(super) struct Subscription {
    receiver: mpsc::Receiver<Frame>,
    pending: Arc<StdMutex<HashMap<u64, mpsc::Sender<Frame>>>>,
    corr: u64,
    writer: WriterSender,
    metrics: Arc<DispatchPathMetrics>,
    version: u8,
    cancelled: Option<oneshot::Sender<()>>,
    initial: Vec<subc_client_rs::ScopeWatchState>,
}
impl Subscription {
    pub(super) fn new(
        receiver: mpsc::Receiver<Frame>,
        pending: Arc<StdMutex<HashMap<u64, mpsc::Sender<Frame>>>>,
        corr: u64,
        writer: WriterSender,
        metrics: Arc<DispatchPathMetrics>,
        version: u8,
        cancelled: oneshot::Sender<()>,
    ) -> Self {
        Self {
            receiver,
            pending,
            corr,
            writer,
            metrics,
            version,
            cancelled: Some(cancelled),
            initial: vec![],
        }
    }
    async fn frame(&mut self) -> Result<Frame, readiness::ControlError> {
        let frame = self
            .receiver
            .recv()
            .await
            .ok_or(readiness::ControlError::ConnectionClosed)?;
        if frame.header.ty == FrameType::Error {
            let error: ErrorBody = serde_json::from_slice(&frame.body)
                .map_err(|error| readiness::ControlError::Decode(error.to_string()))?;
            return Err(readiness::ControlError::Refused {
                code: error.code,
                message: error.message,
            });
        }
        Ok(frame)
    }
    pub(super) async fn open(
        &mut self,
        selectors: &[ScopeWatchSelector],
    ) -> Result<(), readiness::ControlError> {
        let frame = tokio::time::timeout(scope_describe::describe_timeout(), self.frame())
            .await
            .map_err(|_| readiness::ControlError::Timeout)??;
        if frame.header.ty != FrameType::Response {
            return Err(readiness::ControlError::Decode(
                "scope.watch missing snapshot".into(),
            ));
        }
        // Every selector is described after opening, including terminal and
        // unknown initial states. A snapshot is not itself an end proof.
        let snapshot = serde_json::from_slice::<subc_client_rs::ScopeWatchResponse>(&frame.body)
            .map_err(|error| readiness::ControlError::Decode(error.to_string()))?;
        if snapshot.scopes.len() != selectors.len()
            || !selectors.iter().all(|selector| {
                snapshot.scopes.iter().any(|state| {
                    state.scope_ref == selector.scope_ref && state.epoch == selector.epoch
                })
            })
        {
            return Err(readiness::ControlError::Decode(
                "scope.watch snapshot does not cover its selectors".into(),
            ));
        }
        self.initial = snapshot.scopes;
        Ok(())
    }
    async fn next(&mut self) -> Result<subc_client_rs::ScopeWatchPush, readiness::ControlError> {
        let frame = self.frame().await?;
        if frame.header.ty != FrameType::StreamData {
            return Err(readiness::ControlError::ConnectionClosed);
        }
        serde_json::from_slice(&frame.body)
            .map_err(|error| readiness::ControlError::Decode(error.to_string()))
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.corr);
        if let Ok(frame) = Frame::build_with_version(
            self.version,
            FrameType::Cancel,
            control_flags(),
            0,
            0,
            self.corr,
            vec![],
        ) {
            let writer = self.writer.clone();
            let metrics = self.metrics.clone();
            let cancelled = self.cancelled.take();
            tokio::spawn(async move {
                let _ = send_reliable_writer_frame(&writer, &metrics, frame, "scope.watch cancel")
                    .await;
                if let Some(cancelled) = cancelled {
                    let _ = cancelled.send(());
                }
            });
        }
    }
}

pub(super) enum Event {
    Opened(usize, u64),
    Observe(usize, u64, Key),
    Failed(usize, u64, String),
}
struct Shard {
    keys: Vec<Key>,
    generation: u64,
    healthy: bool,
    retry_at: tokio::time::Instant,
    task: Option<JoinHandle<()>>,
    cancelled: Option<oneshot::Receiver<()>>,
}
impl Drop for Shard {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Default, serde::Serialize)]
pub(super) struct Health {
    pub(super) watches: usize,
    pub(super) watched: usize,
    pub(super) polled: usize,
    pub(super) opens: u64,
    pub(super) fallbacks: u64,
    pub(super) descriptions: u64,
    pub(super) drift_repairs: u64,
    pub(super) verifications: u64,
    pub(super) skipped_grant_rows: u64,
    pub(super) skipped_held_rows: u64,
    pub(super) poll_keys: usize,
    pub(super) confirmed_cleanup: usize,
}

#[derive(Clone, Default)]
pub(super) struct Coverage {
    pub(super) keys: HashSet<Key>,
    pub(super) all: bool,
}

pub(super) struct Watches {
    shards: Vec<Shard>,
    next_generation: u64,
    sender: mpsc::UnboundedSender<Event>,
    pub(super) receiver: mpsc::UnboundedReceiver<Event>,
    pub(super) health: Health,
    warned_capacity: bool,
    warned_fallback: bool,
    epochs: HashMap<Key, u64>,
    metrics: Arc<DispatchPathMetrics>,
    retiring: Vec<oneshot::Receiver<()>>,
}
impl Watches {
    pub(super) fn new(metrics: Arc<DispatchPathMetrics>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            shards: vec![],
            next_generation: 0,
            sender,
            receiver,
            health: Health::default(),
            warned_capacity: false,
            warned_fallback: false,
            epochs: HashMap::new(),
            metrics,
            retiring: vec![],
        }
    }
    #[cfg(test)]
    pub(super) fn healthy_keys(&self) -> HashSet<Key> {
        self.shards
            .iter()
            .filter(|shard| shard.healthy)
            .flat_map(|shard| shard.keys.clone())
            .collect()
    }
    pub(super) fn coverage(&self, total: usize) -> Coverage {
        let keys: HashSet<_> = self
            .shards
            .iter()
            .filter(|shard| shard.healthy && shard.task.is_some())
            .flat_map(|shard| shard.keys.clone())
            .collect();
        let all = keys.len() == total;
        Coverage { keys, all }
    }
    pub(super) fn fallback(&mut self, key: &Key, interval: Duration) {
        for shard in &mut self.shards {
            if shard.keys.contains(key) {
                if let Some(task) = shard.task.take() {
                    task.abort();
                }
                if let Some(cancelled) = shard.cancelled.take() {
                    self.retiring.push(cancelled);
                }
                shard.healthy = false;
                shard.retry_at = tokio::time::Instant::now() + interval;
            }
        }
        if !self.warned_fallback {
            log::warn!("scope.watch confirmation indeterminate; falling back to scope polling");
            self.warned_fallback = true;
            self.health.fallbacks += 1;
        }
    }
    pub(super) fn reconcile(
        &mut self,
        scopes: &HashMap<Key, ScopeStamp>,
        control: &Arc<readiness::DaemonControl>,
        interval: Duration,
    ) {
        self.epochs.retain(|key, _| scopes.contains_key(key));
        let mut unassigned: std::collections::BTreeSet<_> = scopes.keys().cloned().collect();
        for shard in &mut self.shards {
            let old = shard.keys.clone();
            shard.keys.retain(|key| scopes.contains_key(key));
            for key in &shard.keys {
                unassigned.remove(key);
            }
            if old != shard.keys {
                if let Some(task) = shard.task.take() {
                    task.abort();
                }
                if let Some(cancelled) = shard.cancelled.take() {
                    self.retiring.push(cancelled);
                }
                shard.healthy = false;
                shard.retry_at = tokio::time::Instant::now();
            }
        }
        for index in 0..WATCHES_PER_CONNECTION {
            if index == self.shards.len() {
                if unassigned.is_empty() {
                    break;
                }
                self.shards.push(Shard {
                    keys: vec![],
                    generation: 0,
                    healthy: false,
                    retry_at: tokio::time::Instant::now(),
                    task: None,
                    cancelled: None,
                });
            }
            let shard = &mut self.shards[index];
            let old_len = shard.keys.len();
            while shard.keys.len() < SELECTORS_PER_WATCH {
                let Some(key) = unassigned.pop_first() else {
                    break;
                };
                shard.keys.push(key);
            }
            if old_len != shard.keys.len() {
                if let Some(task) = shard.task.take() {
                    task.abort();
                }
                if let Some(cancelled) = shard.cancelled.take() {
                    self.retiring.push(cancelled);
                }
                shard.healthy = false;
                shard.retry_at = tokio::time::Instant::now();
            }
            if shard.keys.is_empty()
                || shard.task.is_some()
                || shard.retry_at > tokio::time::Instant::now()
            {
                continue;
            }
            if !control.advertises(subc_control::ops::SCOPE_WATCH) {
                if !self.warned_fallback {
                    log::warn!("scope.watch unavailable; falling back to scope polling");
                    self.warned_fallback = true;
                    self.health.fallbacks += 1;
                }
                shard.retry_at = tokio::time::Instant::now() + interval;
                continue;
            }
            self.next_generation += 1;
            shard.generation = self.next_generation;
            let generation = shard.generation;
            let mut selectors = Vec::new();
            let mut keys: HashMap<Key, Vec<Key>> = HashMap::new();
            for key in &shard.keys {
                let stamp = &scopes[key];
                let epoch = *self.epochs.get(key).unwrap_or(&stamp.scope_epoch);
                let selector = ScopeWatchSelector {
                    scope_ref: subc_client_rs::ScopeRef {
                        owner: stamp.owner.clone(),
                        scope_ref: stamp.scope_ref.clone(),
                    },
                    epoch,
                };
                if !selectors.contains(&selector) {
                    selectors.push(selector);
                }
                keys.entry((key.0.clone(), key.1.clone(), epoch))
                    .or_default()
                    .push(key.clone());
            }
            let control = control.clone();
            let sender = self.sender.clone();
            let retiring = std::mem::take(&mut self.retiring);
            let (cancelled, closed) = oneshot::channel();
            shard.cancelled = Some(closed);
            shard.task = Some(tokio::spawn(async move {
                // Queue cancellation before replacement requests, including at
                // the 16-watch ceiling. Opening first can transiently exceed
                // the daemon's connection limit even when the final set fits.
                for closed in retiring {
                    let _ = closed.await;
                }
                let result: Result<(), readiness::ControlError> = async {
                    let mut subscription = control.scope_watch(selectors, cancelled).await?;
                    let _ = sender.send(Event::Opened(index, generation));
                    for state in &subscription.initial {
                        if state.status != subc_client_rs::ScopeWatchStatus::Live {
                            let identity = (
                                principal_id(&Some(state.scope_ref.owner.clone()))
                                    .unwrap_or_default(),
                                state.scope_ref.scope_ref.clone(),
                                state.epoch,
                            );
                            if let Some(keys) = keys.get(&identity) {
                                for key in keys {
                                    let _ =
                                        sender.send(Event::Observe(index, generation, key.clone()));
                                }
                            }
                        }
                    }
                    loop {
                        let event = subscription.next().await?;
                        if !matches!(
                            event.event,
                            ScopeWatchEvent::Ended | ScopeWatchEvent::Superseded
                        ) {
                            return Err(readiness::ControlError::Decode(
                                "unknown scope.watch event".into(),
                            ));
                        }
                        let identity = (
                            principal_id(&Some(event.scope_ref.owner)).unwrap_or_default(),
                            event.scope_ref.scope_ref,
                            event.epoch,
                        );
                        if let Some(keys) = keys.get(&identity) {
                            for key in keys {
                                let _ = sender.send(Event::Observe(index, generation, key.clone()));
                            }
                        }
                    }
                }
                .await;
                if let Err(error) = result {
                    let _ = sender.send(Event::Failed(index, generation, error.to_string()));
                }
            }));
            shard.retry_at = tokio::time::Instant::now() + interval;
        }
        if !unassigned.is_empty() && !self.warned_capacity {
            log::warn!(
                "scope.watch connection capacity reached; polling {} excess scopes",
                unassigned.len()
            );
            self.warned_capacity = true;
        }
        self.refresh(scopes.len());
    }
    pub(super) fn rebind(&mut self, key: &Key, epoch: u64) -> bool {
        if self.epochs.get(key).copied().unwrap_or(key.2) == epoch {
            return false;
        }
        self.epochs.insert(key.clone(), epoch);
        for shard in &mut self.shards {
            if shard.keys.contains(key) {
                if let Some(task) = shard.task.take() {
                    task.abort();
                }
                if let Some(cancelled) = shard.cancelled.take() {
                    self.retiring.push(cancelled);
                }
                shard.healthy = false;
                shard.retry_at = tokio::time::Instant::now();
            }
        }
        true
    }
    pub(super) fn event(&mut self, event: Event, total: usize) -> Vec<Key> {
        let (index, generation) = match &event {
            Event::Opened(i, g) | Event::Observe(i, g, _) | Event::Failed(i, g, _) => (*i, *g),
        };
        let Some(shard) = self
            .shards
            .get_mut(index)
            .filter(|shard| shard.generation == generation && shard.task.is_some())
        else {
            return vec![];
        };
        let keys = match event {
            Event::Opened(..) => {
                shard.healthy = true;
                self.health.opens += 1;
                self.warned_fallback = false;
                log::info!("scope.watch opened scope_epochs={}", shard.keys.len());
                shard.keys.clone()
            }
            Event::Observe(_, _, key) => vec![key],
            Event::Failed(_, _, error) => {
                shard.healthy = false;
                shard.task.take();
                if let Some(cancelled) = shard.cancelled.take() {
                    self.retiring.push(cancelled);
                }
                self.health.fallbacks += 1;
                if !self.warned_fallback {
                    log::warn!("scope.watch failed; falling back to scope polling: {error}");
                    self.warned_fallback = true;
                }
                vec![]
            }
        };
        self.refresh(total);
        keys
    }
    fn refresh(&mut self, total: usize) {
        self.health.watches = self.shards.iter().filter(|shard| shard.healthy).count();
        self.health.watched = self
            .shards
            .iter()
            .filter(|shard| shard.healthy)
            .map(|shard| shard.keys.len())
            .sum();
        self.health.polled = total.saturating_sub(self.health.watched);
    }
    pub(super) fn next_retry(&self) -> Option<tokio::time::Instant> {
        self.shards
            .iter()
            .filter(|shard| !shard.keys.is_empty() && shard.task.is_none())
            .map(|shard| shard.retry_at)
            .min()
    }
}

impl Drop for Watches {
    fn drop(&mut self) {
        if self.shards.iter().any(|shard| shard.task.is_some()) && !self.warned_fallback {
            log::warn!("scope.watch connection stopped; falling back to lifecycle reconciliation on re-registration");
            self.health.fallbacks += 1;
        }
        self.health.polled += self.health.watched;
        self.health.watches = 0;
        self.health.watched = 0;
        self.metrics
            .scope_descriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .watch_health(&self.health);
    }
}
