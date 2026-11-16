//! Optional wakes for terminal entries in the durable `late_results` feed.
//!
//! The ledger remains authoritative. Bus acknowledgements live only in memory:
//! restart replays retained entries with the same message id. A duplicate after
//! JetStream's deduplication window is harmless because the notice is only a wake.
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock,
    },
    time::Duration,
};

use cortexkit_bus_client::{BusClient, BusClientOptions};
use cortexkit_bus_naming::{validate_token, TokenKind};
use rusqlite::{params, Connection};
use serde::Serialize;
use subc_client_rs::{ConnectionState, ConsumerOptions, SubcConsumer};
use tokio::sync::{oneshot, Notify};

use crate::context::App;

const SCAN_BATCH: usize = 64;
const RECHECK: Duration = Duration::from_secs(60);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT: &str = "late_result_ready";
static SETTLED: LazyLock<Notify> = LazyLock::new(Notify::new);
static HEALTH: LazyLock<parking_lot::Mutex<BusHealth>> = LazyLock::new(Default::default);

#[derive(Default, Serialize)]
struct BusHealth {
    connected: bool,
    connects: u64,
    published: u64,
    publish_failures: u64,
    last_error: Option<&'static str>,
}

pub(crate) fn health() -> serde_json::Value {
    serde_json::to_value(&*HEALTH.lock()).expect("bus health is JSON")
}

fn failure(code: &'static str) {
    let mut health = HEALTH.lock();
    health.connected = false;
    health.publish_failures = health.publish_failures.saturating_add(1);
    health.last_error = Some(code);
}

pub(crate) fn settled() {
    // Coalesced hints carry no payload and never wait for the publisher.
    SETTLED.notify_one();
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
            runtime.block_on(run(app, connection, database, stopped));
        })
        .map_err(|_| failure("bus_worker"))
        .ok()?;
    Some(Worker {
        stop: Some(stop),
        thread: Some(thread),
    })
}

fn backoff(delay: Duration) -> Duration {
    (delay * 2).min(RECHECK)
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
        state.store(
            matches!(event, ConnectionState::Restored { .. }),
            Ordering::Relaxed,
        );
    });
    tokio::select! {
        _ = &mut stopped => {},
        _ = drive(app, database, consumer.clone(), daemon_connected) => {},
    }
    // Bound transport cleanup even if the daemon or NATS stopped responding.
    let _ = tokio::time::timeout(IO_TIMEOUT, consumer.close()).await;
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
    let mut next_check = tokio::time::Instant::now();
    let mut cycle_failed = false;
    loop {
        if !daemon_connected.load(Ordering::Relaxed) {
            failure("bus_consumer_disconnected");
            tokio::time::sleep(delay).await;
            delay = backoff(delay);
            continue;
        }
        if bus
            .as_ref()
            .is_some_and(|client: &BusClient| client.connection_changes().has_changed().is_err())
        {
            bus = None;
        }
        if bus.is_none() {
            let options = BusClientOptions::new("aft").expect("static module id");
            match tokio::time::timeout(IO_TIMEOUT, BusClient::connect(consumer.clone(), options))
                .await
            {
                Ok(Ok(client)) => bus = Some(client),
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
        HEALTH.lock().connected = client.connection().client().connection_state()
            == async_nats::connection::State::Connected;
        if tokio::time::Instant::now() >= next_check || state.after != 0 {
            let app = app.clone();
            let database = database.clone();
            let after = state.after;
            let batch = tokio::task::spawn_blocking(move || {
                let db = app.open_db(&database).map_err(|_| "bus_ledger_open")?;
                let conn = db.try_lock().map_err(|_| "bus_ledger_busy")?;
                scan(&conn, after).map_err(|_| "bus_ledger_read")
            })
            .await;
            let batch = match batch {
                Ok(Ok(batch)) => batch,
                other => {
                    failure(
                        other
                            .ok()
                            .and_then(Result::err)
                            .unwrap_or("bus_ledger_worker"),
                    );
                    tokio::time::sleep(delay).await;
                    delay = backoff(delay);
                    continue;
                }
            };
            match state.deliver(&mut RealPublisher(client), batch).await {
                Ok(_) => {}
                Err(code) => {
                    failure(code);
                    cycle_failed = true;
                }
            }
            if state.after != 0 {
                continue;
            }
            if cycle_failed {
                tokio::time::sleep(delay).await;
                delay = backoff(delay);
                cycle_failed = false;
                next_check = tokio::time::Instant::now();
                continue;
            }
            delay = Duration::from_secs(1);
            next_check = tokio::time::Instant::now() + RECHECK;
        }
        // The short tick only samples connection health. Ledger work occurs on
        // a committed-settlement hint or the independent 60-second re-check.
        wait_for_work(&SETTLED, &mut next_check).await;
    }
}

async fn wait_for_work(settled: &Notify, next_check: &mut tokio::time::Instant) {
    tokio::select! {
        _ = settled.notified() => *next_check = tokio::time::Instant::now(),
        _ = tokio::time::sleep_until(*next_check) => {},
        _ = tokio::time::sleep(Duration::from_secs(1)) => {},
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
    reader: String,
    event_id: String,
    body: Body,
}

#[derive(Clone, Debug, Serialize)]
struct Body {
    call_key: String,
    outcome: String,
}

impl Notice {
    fn msg_id(&self) -> String {
        format!("{}:{}", self.event_id, self.reader)
    }
}

struct Batch {
    after: i64,
    full: bool,
    notices: Vec<Notice>,
}

fn scan(conn: &Connection, after: i64) -> rusqlite::Result<Batch> {
    // Limit visited rows, not a filtered/collected result. The rowid seek walks
    // at most one fixed batch, even when almost every call is still running.
    // No result body or scope/path metadata is loaded into the bus worker.
    let mut stmt = conn.prepare("SELECT rowid, CASE WHEN state='Settled' AND late_entry IS NOT NULL THEN custodian END, call_key, event_id, outcome FROM call_ledger WHERE rowid>?1 ORDER BY rowid LIMIT ?2")?;
    let mut rows = stmt.query(params![after, SCAN_BATCH])?;
    let mut batch = Batch {
        after,
        full: false,
        notices: Vec::new(),
    };
    let mut visited = 0;
    while let Some(row) = rows.next()? {
        visited += 1;
        batch.after = row.get(0)?;
        let custodian: Option<String> = row.get(1)?;
        let Some(reader) = custodian
            .as_deref()
            .and_then(|id| id.strip_prefix("reserved:"))
        else {
            continue;
        };
        if validate_token(TokenKind::ModuleId, reader).is_err() {
            continue;
        }
        batch.notices.push(Notice {
            reader: reader.into(),
            event_id: row.get(3)?,
            body: Body {
                call_key: row.get(2)?,
                outcome: row
                    .get::<_, Option<String>>(4)?
                    .unwrap_or_else(|| "unknown".into()),
            },
        });
    }
    batch.full = visited == SCAN_BATCH;
    Ok(batch)
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
            .publish_event(EVENT, 1, Some(reader), msg_id, body)
            .await
            .map(|_| ())
            .map_err(|error| error_code(&error))
    }
}

#[derive(Default)]
struct DeliveryState {
    after: i64,
    acknowledged: HashSet<String>,
    retained: HashSet<String>,
}

impl DeliveryState {
    async fn deliver(
        &mut self,
        publisher: &mut impl Publisher,
        batch: Batch,
    ) -> Result<bool, &'static str> {
        for notice in &batch.notices {
            self.retained.insert(notice.msg_id());
        }
        let result = self.publish_batch(publisher, &batch).await;
        self.after = if batch.full { batch.after } else { 0 };
        if !batch.full {
            // Feed acknowledgements delete ledger entries. Forget their bus
            // acknowledgements after a full scan so process memory follows retention.
            self.acknowledged.retain(|id| self.retained.contains(id));
            self.retained.clear();
        }
        result.map(|()| batch.full)
    }

    async fn publish_batch(
        &mut self,
        publisher: &mut impl Publisher,
        batch: &Batch,
    ) -> Result<(), &'static str> {
        for notice in &batch.notices {
            let id = notice.msg_id();
            if self.acknowledged.contains(&id) {
                continue;
            }
            let body = serde_json::to_vec(&notice.body).map_err(|_| "bus_body_encode")?;
            tokio::time::timeout(IO_TIMEOUT, publisher.publish(&notice.reader, &id, body))
                .await
                .map_err(|_| "bus_publish_timeout")??;
            self.acknowledged.insert(id);
            let mut health = HEALTH.lock();
            health.published = health.published.saturating_add(1);
            health.last_error = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
