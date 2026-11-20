//! Per-runner status segments in bus KV. The existing status-holder route still
//! carries AFT's segment when KV is unavailable. The KV writer only attaches
//! to the bus-owned bucket and never provisions it.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use cortexkit_bus_client::BusClient;
use cortexkit_bus_naming::{AccountNames, ModuleBucket, MODULE_BUCKETS};
use serde::Serialize;
use tokio::{sync::Notify, time::Instant};

use super::{backoff, HEALTH, IO_TIMEOUT};

const REFRESH: Duration = Duration::from_secs(5);

#[derive(Default, Serialize)]
pub(super) struct StatusHealth {
    puts: u64,
    refreshes: u64,
    skips: u64,
    last_failure: Option<&'static str>,
}

fn failure(reason: &'static str) {
    HEALTH.lock().status.last_failure = Some(reason);
}

fn declaration() -> &'static ModuleBucket {
    MODULE_BUCKETS
        .iter()
        .find(|row| row.owner == "aft" && row.name == "status")
        .expect("naming declares AFT status")
}

fn key(session: &str) -> String {
    use std::fmt::Write;
    let mut key = String::with_capacity(session.len() * 2);
    for byte in session.bytes() {
        write!(&mut key, "{byte:02x}").expect("write to string");
    }
    key
}

fn encode(segment: &str, generation: u64) -> Vec<u8> {
    #[derive(Serialize)]
    struct Value<'a> {
        v: u8,
        segment: &'a str,
        generation: u64,
    }
    serde_json::to_vec(&Value {
        v: 1,
        segment,
        generation,
    })
    .expect("status is JSON")
}

pub(crate) use aft_base::runner_status_state::{initialize, route_bound, route_unbound};
use aft_base::runner_status_state::{notifier, snapshot_segments, Segment};
#[cfg(test)]
use aft_base::runner_status_state::{segment_for_test, Sessions};

trait Writer {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str>;
}
struct RealWriter(async_nats::jetstream::kv::Store);
impl Writer for RealWriter {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str> {
        self.0
            .put(key, value.into())
            .await
            .map(|_| ())
            .map_err(|_| "status_put")
    }
}

#[derive(Default)]
struct Publication {
    written: HashMap<String, (u64, Instant)>,
    skipped: HashMap<String, u64>,
}
impl Publication {
    async fn sync(
        &mut self,
        writer: &mut impl Writer,
        values: Vec<(String, Segment)>,
        health: &mut StatusHealth,
    ) -> Result<(), &'static str> {
        let live: HashSet<_> = values.iter().map(|(session, _)| session.as_str()).collect();
        self.written
            .retain(|session, _| live.contains(session.as_str()));
        self.skipped
            .retain(|session, _| live.contains(session.as_str()));
        for (session, segment) in values {
            // A previous put may have awaited I/O while this session ended.
            // A copied snapshot must not keep its route lifetime alive.
            if segment.live.strong_count() == 0 {
                self.written.remove(&session);
                self.skipped.remove(&session);
                continue;
            }
            if self.skipped.get(&session) == Some(&segment.generation) {
                continue;
            }
            let value = encode(&segment.text, segment.generation);
            if value.len() > declaration().max_value_bytes as usize {
                self.written.remove(&session);
                self.skipped.insert(session, segment.generation);
                health.skips += 1;
                health.last_failure = Some("status_value_oversize");
                continue;
            }
            let refresh = self
                .written
                .get(&session)
                .is_some_and(|(generation, _)| *generation == segment.generation);
            if refresh && self.written[&session].1.elapsed() < REFRESH {
                continue;
            }
            tokio::time::timeout(IO_TIMEOUT, writer.put(&key(&session), value))
                .await
                .map_err(|_| "status_put_timeout")??;
            self.written
                .insert(session, (segment.generation, Instant::now()));
            if refresh {
                health.refreshes += 1;
            } else {
                health.puts += 1;
            }
        }
        Ok(())
    }
    fn next_refresh(&self) -> Instant {
        self.written
            .values()
            .map(|(_, at)| *at + REFRESH)
            .min()
            .unwrap_or_else(|| Instant::now() + REFRESH)
    }
}

trait Connector {
    type Writer: Writer;
    async fn attach(&mut self) -> Result<Self::Writer, &'static str>;
}
struct RealConnector {
    client: Arc<BusClient>,
    bucket: String,
}
impl Connector for RealConnector {
    type Writer = RealWriter;
    async fn attach(&mut self) -> Result<RealWriter, &'static str> {
        async_nats::jetstream::new(self.client.connection().client())
            .get_key_value(&self.bucket)
            .await
            .map(RealWriter)
            .map_err(|_| "status_attach")
    }
}

async fn drive(
    connector: &mut impl Connector,
    snapshot: impl Fn() -> Vec<(String, Segment)>,
    changed: &Notify,
) {
    let mut publication = Publication::default();
    let mut delay = Duration::from_secs(1);
    loop {
        match tokio::time::timeout(IO_TIMEOUT, connector.attach()).await {
            Ok(Ok(mut writer)) => loop {
                let mut health = StatusHealth::default();
                let result = publication.sync(&mut writer, snapshot(), &mut health).await;
                {
                    let mut all = HEALTH.lock();
                    all.status.puts += health.puts;
                    all.status.refreshes += health.refreshes;
                    all.status.skips += health.skips;
                    if health.last_failure.is_some() {
                        all.status.last_failure = health.last_failure;
                    }
                }
                if let Err(reason) = result {
                    failure(reason);
                    break;
                }
                delay = Duration::from_secs(1);
                tokio::select! {
                    _ = changed.notified() => {},
                    _ = tokio::time::sleep_until(publication.next_refresh()) => {},
                }
            },
            Ok(Err(reason)) => failure(reason),
            Err(_) => failure("status_attach_timeout"),
        }
        tokio::time::sleep(delay).await;
        delay = backoff(delay);
    }
}

pub(super) async fn run(client: Arc<BusClient>) {
    let names = AccountNames::derive(client.acct()).expect("issued account");
    let row = declaration();
    let bucket = names
        .module_bucket_name(row.owner, row.name)
        .expect("reviewed bucket");
    let mut changes = client.connection_changes();
    let mut connector = RealConnector { client, bucket };
    loop {
        changes.borrow_and_update();
        tokio::select! {
            _ = drive(&mut connector, snapshot_segments, notifier()) => {},
            result = changes.changed() => {
                failure("status_connection_changed");
                if result.is_err() { return; }
            },
        }
    }
}

pub(super) fn unavailable(reason: &'static str) {
    failure(reason);
}

#[cfg(test)]
pub(crate) fn test_value(session: &str) -> Option<serde_json::Value> {
    segment_for_test(session)
        .map(|segment| serde_json::from_slice(&encode(&segment.text, segment.generation)).unwrap())
}

#[cfg(test)]
pub(crate) mod tests;
