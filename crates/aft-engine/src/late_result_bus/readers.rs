//! Optional bus wakes. Answer ownership stays with the original consent call.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Weak,
    },
    time::Duration,
};

use cortexkit_bus_client::{BusClient, EventDelivery, EventItem};
use cortexkit_bus_naming::EVENT_CONSUMERS;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch, Notify};

use super::{backoff, HEALTH, IO_TIMEOUT};

#[doc(hidden)]
pub const BACKSTOP: Duration = Duration::from_secs(600);
pub(crate) const LIVE_BACKSTOP: Duration = Duration::from_secs(30);
// Cingulate authors incoming card message ids as {elicitation_id}:aft:{seq}.
const CARD_ID_READER: &str = "aft";
static ATTACHED: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);
static SEND_ATTACHED: AtomicBool = AtomicBool::new(false);
static CARDS: LazyLock<parking_lot::Mutex<HashMap<String, Vec<Weak<Wake>>>>> =
    LazyLock::new(Default::default);

#[derive(Default, Serialize)]
pub(super) struct ReaderHealth {
    reader_attached: bool,
    reason: Option<&'static str>,
    events_handled: u64,
    malformed_events: u64,
    mismatched_ids: u64,
    #[serde(skip)]
    warned: std::collections::HashSet<&'static str>,
}

pub(crate) fn cards_attached() -> bool {
    *ATTACHED.borrow()
}
#[doc(hidden)]
pub fn sendstate_attached() -> bool {
    SEND_ATTACHED.load(Ordering::Relaxed)
}

#[derive(Clone, Copy)]
enum Kind {
    Cards,
    SendState,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Cards => "cards",
            Self::SendState => "sendstate",
        }
    }
    fn health(self, health: &mut super::BusHealth) -> &mut ReaderHealth {
        match self {
            Self::Cards => &mut health.cards,
            Self::SendState => &mut health.sendstate,
        }
    }
}

fn attached(value: bool, reason: Option<&'static str>) {
    status(Kind::Cards, value, reason);
}
pub(super) fn unavailable(reason: &'static str) {
    status(Kind::Cards, false, Some(reason));
    status(Kind::SendState, false, Some(reason));
    super::views::unavailable(reason);
    super::status::unavailable(reason);
}
fn status(kind: Kind, value: bool, reason: Option<&'static str>) {
    let mut snapshot = HEALTH.lock();
    let health = kind.health(&mut snapshot);
    let changed = value != health.reader_attached;
    if value != health.reader_attached {
        log::info!(
            "bus {} reader {}",
            kind.label(),
            if value { "attached" } else { "detached" }
        );
    }
    if let Some(reason) = reason {
        if health.warned.insert(reason) {
            log::warn!("bus {} reader failure: {}", kind.label(), reason);
        }
    }
    health.reader_attached = value;
    health.reason = reason;
    drop(snapshot);
    match kind {
        Kind::Cards => {
            ATTACHED.send_if_modified(|old| {
                let changed = *old != value;
                *old = value;
                changed
            });
        }
        Kind::SendState => {
            SEND_ATTACHED.store(value, Ordering::Relaxed);
            if changed {
                crate::bash_background::runner_notice_scheduler::bus_attachment_changed(value);
            }
        }
    }
}

#[derive(Default)]
struct Wake {
    pending: parking_lot::Mutex<HashMap<u64, Vec<oneshot::Sender<()>>>>,
    applied: parking_lot::Mutex<std::collections::HashSet<u64>>,
    ready: Notify,
}

#[doc(hidden)]
pub struct CardWatch {
    id: String,
    wake: Arc<Wake>,
    attachment: watch::Receiver<bool>,
}

#[doc(hidden)]
pub fn watch_card(id: &str) -> CardWatch {
    let wake = Arc::new(Wake::default());
    let mut cards = CARDS.lock();
    cards.retain(|_, watches| {
        watches.retain(|w| w.strong_count() > 0);
        !watches.is_empty()
    });
    cards
        .entry(id.into())
        .or_default()
        .push(Arc::downgrade(&wake));
    CardWatch {
        id: id.into(),
        wake,
        attachment: attachment(id),
    }
}

impl CardWatch {
    #[doc(hidden)]
    pub fn attached(&self) -> bool {
        *self.attachment.borrow()
    }
    /// Detach interrupts even an attached wait, restoring the long-poll path.
    #[doc(hidden)]
    pub async fn wait(&self) {
        let mut changes = self.attachment.clone();
        if !*changes.borrow_and_update() {
            return;
        }
        tokio::select! {
            _ = self.wake.ready.notified() => {},
            _ = changes.changed() => {},
            _ = tokio::time::sleep(LIVE_BACKSTOP) => {},
        }
    }

    #[doc(hidden)]
    pub fn receipt(&self) -> Receipt {
        Receipt {
            wake: self.wake.clone(),
            pending: std::mem::take(&mut *self.wake.pending.lock()),
        }
    }
}

fn attachment(_id: &str) -> watch::Receiver<bool> {
    #[cfg(test)]
    if let Some(sender) = TEST_ATTACHMENTS.lock().get(_id) {
        return sender.subscribe();
    }
    ATTACHED.subscribe()
}

#[cfg(test)]
static TEST_ATTACHMENTS: LazyLock<parking_lot::Mutex<HashMap<String, watch::Sender<bool>>>> =
    LazyLock::new(Default::default);
#[cfg(test)]
pub(crate) struct TestAttachment {
    id: String,
    pub(crate) state: watch::Sender<bool>,
}
#[cfg(test)]
impl TestAttachment {
    pub(crate) fn new(id: &str, value: bool) -> Self {
        let state = watch::channel(value).0;
        TEST_ATTACHMENTS.lock().insert(id.into(), state.clone());
        Self {
            id: id.into(),
            state,
        }
    }
}
#[cfg(test)]
impl Drop for TestAttachment {
    fn drop(&mut self) {
        TEST_ATTACHMENTS.lock().remove(&self.id);
    }
}

impl Drop for CardWatch {
    fn drop(&mut self) {
        if let Some(watches) = CARDS.lock().get_mut(&self.id) {
            watches.retain(|w| !w.ptr_eq(&Arc::downgrade(&self.wake)));
        }
        // Removing the waiter fails its outstanding receipts, allowing replay
        // to find the persisted ask or recognise that no call owns it now.
        self.wake.pending.lock().clear();
    }
}

pub(crate) struct Receipt {
    wake: Arc<Wake>,
    pending: HashMap<u64, Vec<oneshot::Sender<()>>>,
}
impl Receipt {
    pub(crate) fn applied(self) {
        for (seq, receipts) in self.pending {
            self.wake.applied.lock().insert(seq);
            for receipt in receipts {
                let _ = receipt.send(());
            }
        }
    }
}

async fn wake_live(id: &str, seq: u64) -> Result<bool, &'static str> {
    let watches: Vec<_> = CARDS
        .lock()
        .get(id)
        .into_iter()
        .flatten()
        .filter_map(Weak::upgrade)
        .collect();
    let found = !watches.is_empty();
    let waits: Vec<_> = watches
        .into_iter()
        .filter_map(|wake| {
            if wake.applied.lock().contains(&seq) {
                return None;
            }
            let (done, wait) = oneshot::channel();
            wake.pending.lock().entry(seq).or_default().push(done);
            wake.ready.notify_one();
            Some(wait)
        })
        .collect();
    for wait in waits {
        wait.await.map_err(|_| "card_apply_interrupted")?;
    }
    Ok(found)
}

#[derive(Deserialize)]
struct CardEvent {
    elicitation_id: String,
    feed: String,
    seq: u64,
}

pub(crate) async fn card_wake(body: &[u8], msg_id: &str) -> Result<(), &'static str> {
    let body = serde_json::from_slice::<CardEvent>(body)
        .ok()
        .filter(|event| {
            !event.elicitation_id.is_empty() && event.feed == "answers" && event.seq > 0
        });
    let Some(event) = body else {
        HEALTH.lock().cards.malformed_events += 1;
        attached(cards_attached(), Some("card_body_malformed"));
        return Ok(());
    };
    let expected_id = format!("{}:{CARD_ID_READER}:{}", event.elicitation_id, event.seq);
    if msg_id != expected_id {
        HEALTH.lock().cards.mismatched_ids += 1;
        attached(cards_attached(), Some("card_message_id_mismatch"));
    }
    wake_live(&event.elicitation_id, event.seq).await?;
    HEALTH.lock().cards.events_handled += 1;
    Ok(())
}

/// Extract only a send identity, never delivery state. An absent or unusable id
/// wakes the bounded follow queue for authoritative session.send_status reads.
fn send_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .as_object()?
        .get("send_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

async fn send_wake(body: &[u8]) -> Result<(), &'static str> {
    if !serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|value| value.is_object()) {
        HEALTH.lock().sendstate.malformed_events += 1;
        status(
            Kind::SendState,
            sendstate_attached(),
            Some("send_state_body_malformed"),
        );
    }
    crate::bash_background::runner_notice_scheduler::bus_wake(send_id(body).as_deref()).await?;
    HEALTH.lock().sendstate.events_handled += 1;
    Ok(())
}

trait Delivery {
    fn body(&self) -> &[u8];
    fn msg_id(&self) -> &str;
    async fn ack(&self) -> Result<(), &'static str>;
}
impl Delivery for EventDelivery {
    fn body(&self) -> &[u8] {
        &self.body
    }
    fn msg_id(&self) -> &str {
        &self.msg_id
    }
    async fn ack(&self) -> Result<(), &'static str> {
        EventDelivery::ack(self).await.map_err(|_| "event_ack")
    }
}
async fn apply_delivery(delivery: &impl Delivery, kind: Kind) -> Result<(), &'static str> {
    match kind {
        Kind::Cards => card_wake(delivery.body(), delivery.msg_id()).await?,
        Kind::SendState => send_wake(delivery.body()).await?,
    }
    tokio::time::timeout(IO_TIMEOUT, delivery.ack())
        .await
        .map_err(|_| "event_ack_timeout")?
}

async fn reader(client: Arc<BusClient>, kind: Kind) {
    let row = EVENT_CONSUMERS
        .iter()
        .find(|row| {
            row.module_id == "aft"
                && row.filter_subjects.iter().any(|s| {
                    s.contains(match kind {
                        Kind::Cards => "card_settled",
                        Kind::SendState => "send_state",
                    })
                })
        })
        .expect("naming declares AFT readers");
    let durable = row.durable_name().expect("reviewed durable");
    let mut delay = Duration::from_secs(1);
    loop {
        let result = tokio::time::timeout(IO_TIMEOUT, client.event_reader(&durable)).await;
        let mut reader = match result {
            Ok(Ok(reader)) => reader,
            _ => {
                status(kind, false, Some("reader_attach"));
                tokio::time::sleep(delay).await;
                delay = backoff(delay);
                continue;
            }
        };
        status(kind, true, None);
        delay = Duration::from_secs(1);
        loop {
            let next = reader.next();
            tokio::pin!(next);
            let result = loop {
                tokio::select! {
                    result = &mut next => break result,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if client.connection().client().connection_state() != async_nats::connection::State::Connected { break Err(cortexkit_bus_client::Error::Transport("disconnected".into())); }
                    }
                }
            };
            let result = match result {
                Ok(EventItem::Delivery(event)) => apply_delivery(&event, kind).await,
                Ok(EventItem::Malformed(event)) => {
                    kind.health(&mut HEALTH.lock()).malformed_events += 1;
                    status(kind, true, Some("event_envelope_malformed"));
                    tokio::time::timeout(IO_TIMEOUT, event.term())
                        .await
                        .map_err(|_| "event_term_timeout")
                        .and_then(|result| result.map_err(|_| "event_term"))
                }
                _ => Err("event_read"),
            };
            if let Err(reason) = result {
                status(kind, false, Some(reason));
                break;
            }
        }
        tokio::time::sleep(delay).await;
        delay = backoff(delay);
    }
}

#[cfg(test)]
mod tests;

pub(super) struct Services(Vec<tokio::task::JoinHandle<()>>);
pub(super) async fn sweep_cards() {
    loop {
        let ids: Vec<_> = CARDS.lock().keys().cloned().collect();
        for id in ids {
            if let Some(watches) = CARDS.lock().get(&id) {
                for wake in watches.iter().filter_map(Weak::upgrade) {
                    wake.ready.notify_one();
                }
            }
        }
        tokio::time::sleep(BACKSTOP).await;
    }
}
impl Services {
    pub(super) fn start(client: Arc<BusClient>) -> Self {
        Self(vec![
            tokio::spawn(reader(client.clone(), Kind::Cards)),
            tokio::spawn(reader(client.clone(), Kind::SendState)),
            tokio::spawn(super::views::run(client.clone())),
            tokio::spawn(super::status::run(client)),
        ])
    }
}
impl Drop for Services {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
        attached(false, Some("bus_disconnected"));
        status(Kind::SendState, false, Some("bus_disconnected"));
        super::views::detached();
        super::status::unavailable("bus_disconnected");
    }
}
