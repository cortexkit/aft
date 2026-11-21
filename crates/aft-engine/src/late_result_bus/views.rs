//! AFT is the only writer of its per-pin tool.views replies. ck-bus owns bucket
//! provisioning, retention and permissions; AFT only attaches and puts.
use std::{collections::HashSet, sync::Arc, time::Duration};

use cortexkit_bus_client::BusClient;
use cortexkit_bus_naming::{AccountNames, ModuleBucket, MODULE_BUCKETS};
use serde::Serialize;

use super::{backoff, Hooks, HEALTH, IO_TIMEOUT};

mod notice_ready;
pub use notice_ready::{NoticeViewsReady, PullRecord, NOTICE_VIEWS_READY};

#[derive(Default, Serialize)]
pub(super) struct ViewsHealth {
    attached: bool,
    reason: Option<&'static str>,
    writes: u64,
    skips: u64,
    #[serde(skip)]
    warned: HashSet<&'static str>,
}

pub(crate) fn declaration() -> &'static ModuleBucket {
    MODULE_BUCKETS
        .iter()
        .find(|row| row.owner == "aft" && row.name == "views")
        .expect("naming declares AFT views")
}

/// Pins include tool identity and semantics as well as the schema digest.
/// Encoding the full UTF-8 pin preserves those distinctions without KV's ':'.
pub(crate) fn key(pin: &str) -> String {
    use std::fmt::Write;
    let mut key = String::with_capacity(pin.len() * 2);
    for byte in pin.bytes() {
        write!(&mut key, "{byte:02x}").expect("write to string");
    }
    key
}

fn status(attached: bool, reason: Option<&'static str>) {
    let mut health = HEALTH.lock();
    let health = &mut health.views;
    if health.attached != attached {
        log::info!(
            "bus views KV {}",
            if attached { "attached" } else { "detached" }
        );
    }
    if let Some(reason) = reason {
        if health.warned.insert(reason) {
            log::warn!("bus views KV failure: {reason}");
        }
    }
    health.attached = attached;
    health.reason = reason;
}

pub trait Writer {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str>;
}

struct RealWriter(async_nats::jetstream::kv::Store);
impl Writer for RealWriter {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str> {
        self.0
            .put(key, value.into())
            .await
            .map(|_| ())
            .map_err(|_| "views_put")
    }
}

pub struct Publication<'a> {
    notice_views: &'a NoticeViewsReady,
    written: HashSet<(String, blake3::Hash)>,
    skipped: HashSet<(String, blake3::Hash)>,
}

impl<'a> Publication<'a> {
    pub fn new(notice_views: &'a NoticeViewsReady) -> Self {
        Self {
            notice_views,
            written: HashSet::new(),
            skipped: HashSet::new(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn sync_for_test(
        &mut self,
        writer: &mut impl Writer,
        values: &[(String, Vec<u8>)],
    ) -> Result<(), &'static str> {
        self.sync(writer, values).await
    }

    pub async fn sync_with_hooks(
        &mut self,
        writer: &mut impl Writer,
        hooks: Option<&Hooks>,
    ) -> Result<(), &'static str> {
        match hooks {
            Some(hooks) => self.sync(writer, (hooks.bus_snapshot)()).await,
            None => Ok(()),
        }
    }

    async fn sync(
        &mut self,
        writer: &mut impl Writer,
        values: &[(String, Vec<u8>)],
    ) -> Result<(), &'static str> {
        let notice_pins: Vec<_> = values
            .iter()
            .filter(|(pin, _)| pin.starts_with("nv1:"))
            .map(|(pin, _)| pin.clone())
            .collect();
        for (pin, bytes) in values {
            let identity = (pin.clone(), blake3::hash(bytes));
            if bytes.len() > declaration().max_value_bytes as usize {
                if pin.starts_with("nv1:") {
                    self.notice_views.fail("notice_view_oversized");
                }
                if self.skipped.insert(identity) {
                    HEALTH.lock().views.skips += 1;
                    log::warn!(
                        "bus views KV skipping oversized pin={} bytes={} cap={}",
                        key(pin),
                        bytes.len(),
                        declaration().max_value_bytes
                    );
                }
                continue;
            }
            if self.written.contains(&identity) {
                continue;
            }
            tokio::time::timeout(IO_TIMEOUT, writer.put(&key(pin), bytes.clone()))
                .await
                .map_err(|_| "views_put_timeout")??;
            self.written.insert(identity);
            if pin.starts_with("nv1:")
                && notice_pins
                    .iter()
                    .all(|pin| self.written.iter().any(|(written, _)| written == pin))
            {
                self.notice_views.confirm();
            }
            HEALTH.lock().views.writes += 1;
        }
        Ok(())
    }
}

pub(super) async fn run(client: Arc<BusClient>) {
    let names = AccountNames::derive(client.acct()).expect("issued account");
    let row = declaration();
    let bucket = names
        .module_bucket_name(row.owner, row.name)
        .expect("reviewed bucket");
    let mut changes = client.connection_changes();
    let mut publication = Publication::new(&NOTICE_VIEWS_READY);
    let mut delay = Duration::from_secs(1);
    loop {
        changes.borrow_and_update();
        let context = async_nats::jetstream::new(client.connection().client());
        match tokio::time::timeout(IO_TIMEOUT, context.get_key_value(&bucket)).await {
            Ok(Ok(store)) => {
                status(true, None);
                let mut writer = RealWriter(store);
                loop {
                    match publication
                        .sync_with_hooks(&mut writer, super::hooks())
                        .await
                    {
                        Ok(()) => {
                            delay = Duration::from_secs(1);
                        }
                        Err(reason) => {
                            status(false, Some(reason));
                            break;
                        }
                    }
                    tokio::select! {
                        _ = changes.changed() => { status(false, Some("views_connection_changed")); break; },
                        _ = tokio::time::sleep(super::readers::BACKSTOP) => {},
                    }
                }
            }
            _ => status(false, Some("views_attach")),
        }
        tokio::time::sleep(delay).await;
        delay = backoff(delay);
    }
}

pub(super) fn detached() {
    status(false, Some("bus_disconnected"));
}
pub(super) fn unavailable(reason: &'static str) {
    status(false, Some(reason));
}

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct RecordingWriter(pub(crate) Vec<(String, Vec<u8>)>);

#[cfg(feature = "test-support")]
impl RecordingWriter {
    pub fn writes(&self) -> &[(String, Vec<u8>)] {
        &self.0
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Writer for RecordingWriter {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str> {
        self.0.push((key.into(), value));
        Ok(())
    }
}
