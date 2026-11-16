//! Optional bus wakes. Answer ownership stays with the original consent call.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Weak,
    },
    time::Duration,
};

use cortexkit_bus_client::{BusClient, EventItem};
use cortexkit_bus_naming::EVENT_CONSUMERS;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch, Notify};

use super::{backoff, HEALTH, IO_TIMEOUT};

pub(crate) const BACKSTOP: Duration = Duration::from_secs(600);
pub(crate) const LIVE_BACKSTOP: Duration = Duration::from_secs(30);
// Cingulate authors incoming card message ids as {elicitation_id}:aft.
const CARD_ID_SUFFIX: &str = ":aft";
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
}

pub(crate) fn cards_attached() -> bool {
    *ATTACHED.borrow()
}
pub(crate) fn sendstate_attached() -> bool {
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
    if reason.is_some() && reason != health.reason {
        log::warn!("bus {} reader failure: {}", kind.label(), reason.unwrap());
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
                crate::subc::runner_notices::bus_attachment_changed(value);
            }
        }
    }
}

#[derive(Default)]
struct Wake {
    pending: parking_lot::Mutex<Vec<oneshot::Sender<()>>>,
    ready: Notify,
}

pub(crate) struct CardWatch {
    id: String,
    wake: Arc<Wake>,
}

pub(crate) fn watch_card(id: &str) -> CardWatch {
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
    }
}

impl CardWatch {
    /// Detach interrupts even an attached wait, restoring the long-poll path.
    pub(crate) async fn wait(&self) {
        let mut changes = ATTACHED.subscribe();
        if !*changes.borrow_and_update() || !self.wake.pending.lock().is_empty() {
            return;
        }
        tokio::select! {
            _ = self.wake.ready.notified() => {},
            _ = changes.changed() => {},
            _ = tokio::time::sleep(LIVE_BACKSTOP) => {},
        }
    }

    pub(crate) fn receipt(&self) -> Receipt {
        Receipt(std::mem::take(&mut *self.wake.pending.lock()))
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

pub(crate) struct Receipt(Vec<oneshot::Sender<()>>);
impl Receipt {
    pub(crate) fn applied(self) {
        for receipt in self.0 {
            let _ = receipt.send(());
        }
    }
}

async fn wake_live(id: &str) -> Result<bool, &'static str> {
    let waits: Vec<_> = CARDS
        .lock()
        .get(id)
        .into_iter()
        .flatten()
        .filter_map(Weak::upgrade)
        .map(|wake| {
            let (done, wait) = oneshot::channel();
            wake.pending.lock().push(done);
            wake.ready.notify_one();
            wait
        })
        .collect();
    let found = !waits.is_empty();
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

async fn card_wake(body: &[u8], msg_id: &str) -> Result<(), &'static str> {
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
    if msg_id.strip_suffix(CARD_ID_SUFFIX) != Some(&event.elicitation_id) {
        HEALTH.lock().cards.mismatched_ids += 1;
        attached(cards_attached(), Some("card_message_id_mismatch"));
    }
    if !wake_live(&event.elicitation_id).await? {
        crate::subc::consent::bus_sweep(Some(&event.elicitation_id)).await?;
    }
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
    crate::subc::runner_notices::bus_wake(send_id(body).as_deref()).await?;
    HEALTH.lock().sendstate.events_handled += 1;
    Ok(())
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
        .expect("naming declares AFT cards");
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
                Ok(EventItem::Delivery(event)) => match match kind {
                    Kind::Cards => card_wake(&event.body, &event.msg_id).await,
                    Kind::SendState => send_wake(&event.body).await,
                } {
                    Ok(()) => event.ack().await.map_err(|_| "event_ack"),
                    Err(code) => Err(code),
                },
                Ok(EventItem::Malformed(event)) => {
                    kind.health(&mut HEALTH.lock()).malformed_events += 1;
                    status(kind, true, Some("event_envelope_malformed"));
                    event.term().await.map_err(|_| "event_term")
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

pub(super) struct Services(Vec<tokio::task::JoinHandle<()>>);
pub(super) async fn sweep_parked() {
    loop {
        let _ = crate::subc::consent::bus_sweep(None).await;
        tokio::time::sleep(BACKSTOP).await;
    }
}
impl Services {
    pub(super) fn start(client: Arc<BusClient>) -> Self {
        Self(vec![
            tokio::spawn(reader(client.clone(), Kind::Cards)),
            tokio::spawn(reader(client, Kind::SendState)),
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
    }
}
