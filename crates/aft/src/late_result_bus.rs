//! Optional wakes for terminal entries in the durable `late_results` feed.
//!
//! The ledger remains authoritative. Bus acknowledgements live only in memory:
//! restart replays retained entries with the same message id. A duplicate after
//! JetStream's deduplication window is harmless because the notice is only a wake.
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock,
    },
    time::Duration,
};

use cortexkit_bus_client::{BusClient, BusClientOptions};
use cortexkit_role_tool_provider::{
    late_result_ready_msg_id, late_result_ready_reader, LateResultReady, LATE_RESULT_READY_EVENT,
    LATE_RESULT_READY_VERSION,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use subc_client_rs::{ConnectionState, ConsumerOptions, SubcConsumer};
use tokio::sync::{oneshot, Notify};

use crate::context::App;
use crate::db::call_ledger::{Key, LATE_RESULTS_PAGE_MAX, UNACKED_RESULTS_MAX};

mod display;
#[cfg(test)]
pub(crate) use display::with_test_reader;
pub(crate) mod readers;
mod views;

const SCAN_BATCH: usize = 64;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const OVERLAP_MS: i64 = 10_000;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const LIVE_SCAN_SQL: &str = "SELECT rowid,settled_at,seq,CASE WHEN acked=0 AND late_entry IS NOT NULL AND seq>COALESCE((SELECT through_seq FROM call_ledger_ack WHERE custodian=call_ledger.custodian),0) THEN custodian END,carrier,call_key,event_id,outcome FROM call_ledger WHERE state='Settled' AND (settled_at,rowid)>(?1,?2) ORDER BY settled_at,rowid LIMIT ?3";
static SETTLED: LazyLock<Notify> = LazyLock::new(Notify::new);
static DISPLAY: LazyLock<Notify> = LazyLock::new(Notify::new);
static HEALTH: LazyLock<parking_lot::Mutex<BusHealth>> = LazyLock::new(Default::default);

#[derive(Default, Serialize)]
struct BusHealth {
    connected: bool,
    connects: u64,
    published: u64,
    publish_failures: u64,
    last_error: Option<&'static str>,
    display_published: u64,
    display_publish_failures: u64,
    display_last_error: Option<&'static str>,
    display_read_evicted_early: u64,
    cards: readers::ReaderHealth,
    sendstate: readers::ReaderHealth,
    views: views::ViewsHealth,
    #[serde(skip)]
    warned: std::collections::HashSet<&'static str>,
}

pub(crate) fn health() -> serde_json::Value {
    serde_json::to_value(&*HEALTH.lock()).expect("bus health is JSON")
}

fn failure(code: &'static str) {
    let mut health = HEALTH.lock();
    if health.warned.insert(code) {
        log::warn!("bus failure: {code}");
    }
    health.connected = false;
    health.publish_failures = health.publish_failures.saturating_add(1);
    health.last_error = Some(code);
    drop(health);
    if matches!(
        code,
        "bus_runtime"
            | "bus_worker"
            | "bus_consumer_connect"
            | "bus_consumer_disconnected"
            | "bus_connect_timeout"
            | "bus_refused"
            | "bus_transport"
            | "bus_reply"
            | "bus_naming"
            | "bus_client"
    ) {
        readers::unavailable(code);
    }
}

pub(crate) fn settled() {
    // Coalesced hints carry no payload and never wait for the publisher.
    SETTLED.notify_one();
}

pub(crate) fn display_committed() {
    DISPLAY.notify_one();
}

pub(crate) fn display_read_evicted_early(reason: &'static str) {
    let mut health = HEALTH.lock();
    health.display_read_evicted_early = health.display_read_evicted_early.saturating_add(1);
    if health.warned.insert(reason) {
        log::warn!("display pull retention shortened by pressure: {reason}");
    }
}

pub(crate) fn display_reader() -> Result<Option<String>, subc_protocol::ErrorBody> {
    display::reader()
}

pub(crate) struct Worker {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn shutdown(mut self) {
        self.stop.take();
        // Only module teardown joins the isolated bus runtime, never a reply.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.take();
    }
}

/// Start after the provider has attached to the daemon. Neither the frame loop
/// nor a request executor runs credential issuance, database scans or publishes.
pub(crate) fn start(app: Arc<App>, connection: PathBuf, database: PathBuf) -> Option<Worker> {
    let (stop, stopped) = oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("aft-late-result-bus".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                failure("bus_runtime");
                return;
            };
            runtime.block_on(async {
                tokio::select! {
                    _ = run(app, connection, database, stopped) => {},
                    _ = readers::sweep_cards() => {},
                }
            });
        })
        .map_err(|_| failure("bus_worker"))
        .ok()?;
    Some(Worker {
        stop: Some(stop),
        thread: Some(thread),
    })
}

fn backoff(delay: Duration) -> Duration {
    (delay * 2).min(MAX_BACKOFF)
}

async fn run(app: Arc<App>, path: PathBuf, database: PathBuf, mut stopped: oneshot::Receiver<()>) {
    let mut delay = Duration::from_secs(1);
    let consumer = loop {
        let result = tokio::select! {
            _ = &mut stopped => return,
            result = tokio::time::timeout(IO_TIMEOUT, SubcConsumer::connect(&path, ConsumerOptions::default())) => result,
        };
        if let Ok(Ok(consumer)) = result {
            HEALTH.lock().connects += 1;
            log::info!("late-result bus consumer connected");
            break Arc::new(consumer);
        }
        failure("bus_consumer_connect");
        tokio::select! { _ = &mut stopped => return, _ = tokio::time::sleep(delay) => {} }
        delay = backoff(delay);
    };
    // SubcConsumer restores dropped daemon connections itself. Keep one consumer
    // for the process, rather than opening another on each credential retry.
    let daemon_connected = Arc::new(AtomicBool::new(true));
    let state = daemon_connected.clone();
    consumer.on_connection_state(move |event| {
        display::invalidate_reader();
        state.store(
            matches!(event, ConnectionState::Restored { .. }),
            Ordering::Relaxed,
        );
    });
    tokio::select! {
        _ = &mut stopped => {},
        _ = drive(app, database, consumer.clone(), daemon_connected.clone()) => {},
        _ = display::watch_reader(consumer.clone(), daemon_connected) => {},
    }
    // Bound transport cleanup even if the daemon or NATS stopped responding.
    let _ = tokio::time::timeout(IO_TIMEOUT, consumer.close()).await;
    display::invalidate_reader();
    HEALTH.lock().connected = false;
}

async fn drive(
    app: Arc<App>,
    database: PathBuf,
    consumer: Arc<SubcConsumer>,
    daemon_connected: Arc<AtomicBool>,
) {
    let mut state = DeliveryState::default();
    let mut bus = None;
    let mut delay = Duration::from_secs(1);
    let mut scan_pending = true;
    let mut services = None;
    loop {
        if !daemon_connected.load(Ordering::Relaxed) {
            services = None;
            failure("bus_consumer_disconnected");
            tokio::time::sleep(delay).await;
            delay = backoff(delay);
            continue;
        }
        if bus.as_ref().is_some_and(|client: &Arc<BusClient>| {
            client.connection_changes().has_changed().is_err()
        }) {
            services = None;
            bus = None;
        }
        if bus.is_none() {
            let options = BusClientOptions::new("aft").expect("static module id");
            match tokio::time::timeout(IO_TIMEOUT, BusClient::connect(consumer.clone(), options))
                .await
            {
                Ok(Ok(client)) => {
                    bus = Some(Arc::new(client));
                    delay = Duration::from_secs(1);
                }
                Ok(Err(error)) => {
                    failure(error_code(&error));
                    tokio::time::sleep(delay).await;
                    delay = backoff(delay);
                    continue;
                }
                Err(_) => {
                    failure("bus_connect_timeout");
                    tokio::time::sleep(delay).await;
                    delay = backoff(delay);
                    continue;
                }
            }
        }
        let client = bus.as_ref().expect("connected above");
        let connected = client.connection().client().connection_state()
            == async_nats::connection::State::Connected;
        if !connected {
            services = None;
        } else if services.is_none() {
            services = Some(readers::Services::start(Arc::clone(client)));
        }
        HEALTH.lock().connected = connected;
        display::publish_one(client).await;
        let mut publisher = RealPublisher(client);
        let due = state.due();
        if !due.is_empty() {
            let app = app.clone();
            let database = database.clone();
            let retained = tokio::task::spawn_blocking(move || {
                let db = app.open_db(&database).map_err(|_| "bus_ledger_open")?;
                let conn = db.try_lock().map_err(|_| "bus_ledger_busy")?;
                due.into_iter()
                    .map(|notice| {
                        let retained =
                            is_retained(&conn, &notice).map_err(|_| "bus_ledger_read")?;
                        Ok((notice, retained))
                    })
                    .collect::<Result<Vec<_>, &'static str>>()
            })
            .await;
            match retained {
                Ok(Ok(retained)) => state.retry(&mut publisher, retained).await,
                _ => {
                    failure("bus_retry_ledger");
                    tokio::time::sleep(delay).await;
                    delay = backoff(delay);
                    continue;
                }
            }
        }
        let capacity = state.capacity();
        if scan_pending && capacity > 0 {
            let app = app.clone();
            let database = database.clone();
            let position = state.position.clone();
            let batch = tokio::task::spawn_blocking(move || {
                let db = app.open_db(&database).map_err(|_| "bus_ledger_open")?;
                let conn = db.try_lock().map_err(|_| "bus_ledger_busy")?;
                read_batch(&conn, &position, capacity).map_err(|_| "bus_ledger_read")
            })
            .await;
            match batch {
                Ok(Ok(batch)) => {
                    scan_pending = state.deliver(&mut publisher, batch).await;
                    delay = Duration::from_secs(1);
                    if scan_pending {
                        continue;
                    }
                }
                _ => {
                    failure("bus_ledger_read");
                    tokio::time::sleep(delay).await;
                    delay = backoff(delay);
                    continue;
                }
            }
        }
        // Only hints read newly settled rows. Retries read their own bounded
        // set of ledger keys; idle waits never start a ledger scan.
        if wait_for_work(&SETTLED, state.next_retry()).await {
            if !scan_pending {
                state.hint();
            }
            scan_pending = true;
        }
    }
}

async fn wait_for_work(settled: &Notify, retry: Option<tokio::time::Instant>) -> bool {
    tokio::select! {
        _ = settled.notified() => true,
        _ = DISPLAY.notified() => false,
        _ = async {
            if let Some(deadline) = retry { tokio::time::sleep_until(deadline).await; }
            else { std::future::pending::<()>().await; }
        } => false,
        _ = tokio::time::sleep(Duration::from_secs(1)) => false,
    }
}

fn error_code(error: &cortexkit_bus_client::Error) -> &'static str {
    use cortexkit_bus_client::Error;
    match error {
        Error::Refused(_) => "bus_refused",
        Error::Transport(_) => "bus_transport",
        Error::Reply(_) => "bus_reply",
        Error::Naming(_) => "bus_naming",
        Error::Bus(_) | Error::Nats(_) => "bus_publish",
        Error::EventBodyTooLarge { .. } => "bus_body_size",
        _ => "bus_client",
    }
}

#[derive(Clone, Debug)]
struct Notice {
    key: Key,
    settled_at: i64,
    reader: String,
    event_id: String,
    body: LateResultReady,
}

impl Notice {
    fn msg_id(&self) -> String {
        late_result_ready_msg_id(&self.event_id, &self.reader)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Cursor {
    settled_at: i64,
    rowid: i64,
}

#[derive(Clone, Debug, Default)]
enum Position {
    #[default]
    Start,
    Replay {
        boundary: Cursor,
        reader: String,
        seq: u64,
    },
    Live(Cursor),
}

struct Batch {
    position: Position,
    high_water: Cursor,
    more: bool,
    visited: usize,
    notices: Vec<Notice>,
}

fn notice(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Notice>> {
    let custodian: Option<String> = row.get(3)?;
    let Some(reader) = custodian.as_deref().and_then(late_result_ready_reader) else {
        return Ok(None);
    };
    Ok(Some(Notice {
        key: Key {
            carrier: row.get(4)?,
            call_key: row.get(5)?,
        },
        settled_at: row.get(1)?,
        reader: reader.into(),
        event_id: row.get(6)?,
        body: LateResultReady::new(
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(7)?
                .unwrap_or_else(|| "unknown".into()),
        ),
    }))
}

fn read_batch(conn: &Connection, position: &Position, capacity: usize) -> rusqlite::Result<Batch> {
    match position {
        Position::Start => {
            // The retention index yields the startup boundary without walking
            // the ledger. Older entries are replayed through each reader's feed.
            let boundary = conn.query_row("SELECT settled_at,rowid FROM call_ledger WHERE state='Settled' ORDER BY settled_at DESC,rowid DESC LIMIT 1", [], |row| Ok(Cursor { settled_at: row.get(0)?, rowid: row.get(1)? })).optional()?.unwrap_or_default();
            Ok(Batch {
                position: Position::Replay {
                    boundary,
                    reader: String::new(),
                    seq: 0,
                },
                high_water: boundary,
                more: true,
                visited: 0,
                notices: Vec::new(),
            })
        }
        Position::Replay {
            boundary,
            reader,
            seq,
        } => replay(conn, *boundary, reader, *seq, capacity),
        Position::Live(after) => scan(conn, *after, capacity),
    }
}

fn scan(conn: &Connection, after: Cursor, capacity: usize) -> rusqlite::Result<Batch> {
    let limit = SCAN_BATCH.min(capacity);
    // The existing (state, settled_at) index also orders equal timestamps by
    // rowid. LIMIT bounds rows before collecting, not after a whole-ledger walk.
    let mut stmt = conn.prepare(LIVE_SCAN_SQL)?;
    let mut rows = stmt.query(params![after.settled_at, after.rowid, limit])?;
    let mut batch = Batch {
        position: Position::Live(after),
        high_water: after,
        more: false,
        visited: 0,
        notices: Vec::new(),
    };
    while let Some(row) = rows.next()? {
        batch.visited += 1;
        let cursor = Cursor {
            settled_at: row.get(1)?,
            rowid: row.get(0)?,
        };
        batch.high_water = cursor;
        batch.position = Position::Live(cursor);
        if let Some(notice) = notice(row)? {
            batch.notices.push(notice);
        }
    }
    batch.more = batch.visited == limit;
    Ok(batch)
}

fn replay(
    conn: &Connection,
    boundary: Cursor,
    reader: &str,
    seq: u64,
    capacity: usize,
) -> rusqlite::Result<Batch> {
    let mut batch = Batch {
        position: Position::Live(Cursor {
            settled_at: boundary.settled_at.saturating_sub(OVERLAP_MS),
            rowid: 0,
        }),
        high_water: boundary,
        more: true,
        visited: 0,
        notices: Vec::new(),
    };
    // Terminal feed ack deletes call_ledger rows (ack_through -> ack). Only
    // non-terminal call_ledger_updates keep dedup rows with a NULL entry.
    // through_seq and acked are checked too, so retained ack markers never wake.
    let current = if seq == 0 {
        conn.query_row("SELECT custodian,through_seq FROM call_ledger_ack WHERE custodian>?1 AND custodian>='reserved:' AND custodian<'reserved;' ORDER BY custodian LIMIT 1", [reader], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))).optional()?
    } else {
        Some((reader.to_owned(), seq))
    };
    let Some((reader, seq)) = current else {
        return Ok(batch);
    };
    let limit = SCAN_BATCH.min(LATE_RESULTS_PAGE_MAX as usize).min(capacity);
    // This is the terminal half of late_results' custodian/seq seek, without
    // loading result bodies or the separate non-terminal held-update entries.
    let mut stmt = conn.prepare("SELECT rowid,settled_at,seq,custodian,carrier,call_key,event_id,outcome FROM call_ledger WHERE custodian=?1 AND seq>?2 AND late_entry IS NOT NULL AND acked=0 AND (settled_at,rowid)<=(?3,?4) ORDER BY seq LIMIT ?5")?;
    let mut rows = stmt.query(params![
        reader,
        seq,
        boundary.settled_at,
        boundary.rowid,
        limit
    ])?;
    let mut last = seq;
    while let Some(row) = rows.next()? {
        batch.visited += 1;
        last = row.get(2)?;
        if let Some(notice) = notice(row)? {
            batch.notices.push(notice);
        }
    }
    batch.position = Position::Replay {
        boundary,
        reader,
        seq: if batch.visited == limit { last } else { 0 },
    };
    Ok(batch)
}

fn is_retained(conn: &Connection, notice: &Notice) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM call_ledger WHERE carrier=?1 AND call_key=?2 AND event_id=?3 AND state='Settled' AND acked=0 AND late_entry IS NOT NULL AND seq>COALESCE((SELECT through_seq FROM call_ledger_ack WHERE custodian=call_ledger.custodian),0))", params![notice.key.carrier, notice.key.call_key, notice.event_id], |row| row.get(0))
}

trait Publisher {
    async fn publish(
        &mut self,
        reader: &str,
        msg_id: &str,
        body: Vec<u8>,
    ) -> Result<(), &'static str>;
}

struct RealPublisher<'a>(&'a BusClient);
impl Publisher for RealPublisher<'_> {
    async fn publish(
        &mut self,
        reader: &str,
        msg_id: &str,
        body: Vec<u8>,
    ) -> Result<(), &'static str> {
        // publish_event uses AccountNames::event_subject_with_reader and sets
        // Nats-Msg-Id, and succeeds only after the JetStream acknowledgement.
        self.0
            .publish_event(
                LATE_RESULT_READY_EVENT,
                LATE_RESULT_READY_VERSION as u32,
                Some(reader),
                msg_id,
                body,
            )
            .await
            .map(|_| ())
            .map_err(|error| error_code(&error))
    }
}

#[derive(Default)]
struct DeliveryState {
    position: Position,
    high_water: Cursor,
    acknowledged: HashMap<String, i64>,
    pending: BTreeMap<String, Retry>,
}

struct Retry {
    notice: Notice,
    at: tokio::time::Instant,
    delay: Duration,
}

impl DeliveryState {
    fn capacity(&self) -> usize {
        UNACKED_RESULTS_MAX - self.pending.len()
    }

    fn hint(&mut self) {
        // A ten-second overlap catches older rowids committed in the same
        // millisecond and small backward wall-clock steps. Larger clock jumps
        // can delay a wake, but late_results polling still retrieves the result.
        self.position = Position::Live(Cursor {
            settled_at: self.high_water.settled_at.saturating_sub(OVERLAP_MS),
            rowid: 0,
        });
        self.prune();
    }

    fn prune(&mut self) {
        let oldest = self.high_water.settled_at.saturating_sub(OVERLAP_MS);
        self.acknowledged
            .retain(|_, settled_at| *settled_at >= oldest);
    }

    fn next_retry(&self) -> Option<tokio::time::Instant> {
        self.pending.values().map(|retry| retry.at).min()
    }

    fn due(&self) -> Vec<Notice> {
        let now = tokio::time::Instant::now();
        let mut retries: Vec<_> = self
            .pending
            .values()
            .filter(|retry| retry.at <= now)
            .collect();
        retries.sort_unstable_by_key(|retry| retry.at);
        let mut due = Vec::new();
        for retry in retries {
            due.push(retry.notice.clone());
            if due.len() == SCAN_BATCH {
                break;
            }
        }
        due
    }

    async fn deliver(&mut self, publisher: &mut impl Publisher, batch: Batch) -> bool {
        // read_batch never reads more candidates than available retry slots.
        // If the bounded set fills, stop advancing the cursor; the unread
        // settlements stay in the ledger rather than being dropped from memory.
        assert!(batch.notices.len() <= self.capacity());
        for notice in batch.notices {
            let id = notice.msg_id();
            if self.acknowledged.contains_key(&id) || self.pending.contains_key(&id) {
                continue;
            }
            if send(publisher, &notice).await {
                self.acknowledged.insert(id, notice.settled_at);
            } else {
                self.pending.insert(
                    id,
                    Retry {
                        notice,
                        at: tokio::time::Instant::now() + Duration::from_secs(1),
                        delay: Duration::from_secs(1),
                    },
                );
            }
        }
        self.high_water = self.high_water.max(batch.high_water);
        self.position = batch.position;
        self.prune();
        batch.more
    }

    async fn retry(&mut self, publisher: &mut impl Publisher, retained: Vec<(Notice, bool)>) {
        for (notice, retained) in retained {
            let id = notice.msg_id();
            if !retained {
                self.pending.remove(&id);
                continue;
            }
            if send(publisher, &notice).await {
                self.pending.remove(&id);
                self.acknowledged.insert(id, notice.settled_at);
            } else if let Some(retry) = self.pending.get_mut(&id) {
                retry.delay = backoff(retry.delay);
                retry.at = tokio::time::Instant::now() + retry.delay;
            }
        }
        self.prune();
    }
}

async fn send(publisher: &mut impl Publisher, notice: &Notice) -> bool {
    let result = match serde_json::to_vec(&notice.body) {
        Ok(body) => tokio::time::timeout(
            IO_TIMEOUT,
            publisher.publish(&notice.reader, &notice.msg_id(), body),
        )
        .await
        .unwrap_or(Err("bus_publish_timeout")),
        Err(_) => Err("bus_body_encode"),
    };
    match result {
        Ok(()) => {
            let mut health = HEALTH.lock();
            health.published = health.published.saturating_add(1);
            health.last_error = None;
            true
        }
        Err(code) => {
            failure(code);
            false
        }
    }
}

#[cfg(test)]
mod tests;
