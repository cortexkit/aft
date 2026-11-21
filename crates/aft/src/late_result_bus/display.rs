//! Display wakes share the process-lived bus connection, never the reply path.
use cortexkit_bus_client::BusClient;
use cortexkit_role_tool_display::{
    DisplayRecordReady, DISPLAY_RECORD_READY_EVENT, DISPLAY_RECORD_READY_VERSION,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, LazyLock,
};
use std::time::Duration;

use super::{DisplayPublish, Hooks};

pub(crate) fn declared_display(modules: &[subc_client_rs::CatalogEntry]) -> Option<String> {
    let mut claimants: Vec<_> = modules
        .iter()
        .filter(|module| {
            module.capabilities.as_ref().is_some_and(|capabilities| {
                capabilities.provides.iter().any(|capability| {
                    capability == cortexkit_role_tool_display::TOOL_DISPLAY_CAPABILITY
                })
            })
        })
        .map(|module| module.module_id.clone())
        .collect();
    claimants.sort();
    claimants.dedup();
    (claimants.len() == 1).then(|| claimants.remove(0))
}

#[derive(Default)]
struct ReaderCache {
    epoch: u64,
    module: Option<Option<String>>,
    notice_capable: bool,
}

impl ReaderCache {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.module = None;
        self.notice_capable = false;
    }

    fn update(&mut self, epoch: u64, modules: &[subc_client_rs::CatalogEntry]) {
        if self.epoch == epoch {
            self.module = Some(declared_display(modules));
            self.notice_capable = modules.iter().any(|entry| {
                self.module.as_ref().and_then(Option::as_deref) == Some(entry.module_id.as_str())
                    && entry.capabilities.as_ref().is_some_and(|caps| {
                        caps.provides.iter().any(|capability| {
                            capability
                                == cortexkit_role_tool_display::NOTICE_RECORD_DISPLAY_CAPABILITY
                        })
                    })
            });
        }
    }
}

pub(super) fn notice_reader() -> Result<Option<String>, subc_protocol::ErrorBody> {
    let reader = READER.lock();
    let module = reader
        .module
        .as_ref()
        .ok_or_else(cortexkit_role_tool_display::not_ready)?;
    Ok(module.clone().filter(|_| reader.notice_capable))
}

static READER: LazyLock<parking_lot::Mutex<ReaderCache>> = LazyLock::new(Default::default);

pub(super) fn invalidate_reader() {
    READER.lock().invalidate();
}

pub(super) fn reader() -> Result<Option<String>, subc_protocol::ErrorBody> {
    #[cfg(test)]
    if let Some(reader) = TEST_READER.with(|reader| reader.borrow().clone()) {
        return Ok(reader);
    }
    READER
        .lock()
        .module
        .clone()
        .ok_or_else(cortexkit_role_tool_display::not_ready)
}

#[cfg(test)]
thread_local! {
    static TEST_READER: std::cell::RefCell<Option<Option<String>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_reader<T>(reader: Option<&str>, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Option<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_READER.with(|reader| *reader.borrow_mut() = self.0.take());
        }
    }
    let _restore =
        Restore(TEST_READER.with(|value| value.replace(Some(reader.map(str::to_owned)))));
    run()
}

pub(super) async fn watch_reader(
    consumer: Arc<subc_client_rs::SubcConsumer>,
    connected: Arc<AtomicBool>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        invalidate_reader();
        if connected.load(Ordering::Relaxed) {
            if let Ok(Ok(mut watch)) =
                tokio::time::timeout(super::IO_TIMEOUT, consumer.catalog_watch()).await
            {
                if let Ok(Ok(Some(_))) = tokio::time::timeout(super::IO_TIMEOUT, watch.next()).await
                {
                    loop {
                        let epoch = READER.lock().epoch;
                        let Ok(Ok(catalog)) =
                            tokio::time::timeout(super::IO_TIMEOUT, consumer.catalog_list()).await
                        else {
                            break;
                        };
                        if !connected.load(Ordering::Relaxed) {
                            break;
                        }
                        READER.lock().update(epoch, &catalog.modules);
                        delay = Duration::from_secs(1);
                        let changed = loop {
                            tokio::select! {
                                event = watch.next() => match event {
                                    Ok(Some(event)) if event.generation <= catalog.generation => continue,
                                    Ok(Some(_)) => break true,
                                    _ => break false,
                                },
                                _ = tokio::time::sleep(Duration::from_secs(5 * 60)) => break true,
                            }
                        };
                        invalidate_reader();
                        if !changed {
                            break;
                        }
                    }
                }
            }
        }
        invalidate_reader();
        tokio::time::sleep(delay).await;
        delay = super::backoff(delay);
    }
}

pub(crate) fn body(id: &str) -> Result<Vec<u8>, &'static str> {
    let notice = DisplayRecordReady::new(id);
    notice.validate().map_err(|_| "display_bus_id")?;
    serde_json::to_vec(&notice).map_err(|_| "display_bus_body")
}

pub(crate) async fn announce(
    hooks: Option<&Hooks>,
    id: String,
    pull_bytes: Vec<u8>,
    publish: DisplayPublish<'_>,
) -> Result<(), &'static str> {
    let hooks = hooks.ok_or("display_store_not_started")?;
    (hooks.announce)(id, pull_bytes, publish).await
}

pub(crate) trait ReadyPublisher {
    fn ready(
        &self,
        id: &str,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), &'static str>> + Send;
}

impl ReadyPublisher for BusClient {
    async fn ready(&self, id: &str, bytes: Vec<u8>) -> Result<(), &'static str> {
        tokio::time::timeout(
            super::IO_TIMEOUT,
            self.publish_event(
                DISPLAY_RECORD_READY_EVENT,
                DISPLAY_RECORD_READY_VERSION as u32,
                None,
                id,
                bytes,
            ),
        )
        .await
        .map_err(|_| "display_bus_timeout")?
        .map(|_| ())
        .map_err(|error| super::error_code(&error))
    }
}

pub(crate) async fn publish_one(publisher: &(impl ReadyPublisher + Sync), hooks: Option<&Hooks>) {
    let Some(hooks) = hooks else {
        return;
    };
    let Some((id, pull_bytes)) = (hooks.take_due_read)() else {
        return;
    };
    let result = announce(
        Some(hooks),
        id.clone(),
        pull_bytes,
        Box::pin(async { publisher.ready(&id, body(&id)?).await }),
    )
    .await;
    completed(hooks, &id, result);
}

fn completed(hooks: &Hooks, id: &str, result: Result<(), &'static str>) {
    // Wake backoff is separate from Outbox::retry's push attempt budget.
    // A full store may defer a wake indefinitely without exhausting its pushes.
    (hooks.mark_read_published)(id, result.is_ok());
    let mut health = super::HEALTH.lock();
    match result {
        Ok(()) => {
            health.display_published = health.display_published.saturating_add(1);
            health.display_last_error = None;
        }
        Err(code) => {
            health.display_publish_failures = health.display_publish_failures.saturating_add(1);
            health.display_last_error = Some(code);
            if health.warned.insert(code) {
                log::warn!("display bus failure: {code}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_reader_cache_invalidates_on_catalog_change_and_rejects_stale_refresh() {
        let module = |id: &str| {
            serde_json::from_value(serde_json::json!({
            "module_id":id,"ready":false,"roles":[],"control_ops":[],
            "capabilities":{"provides":[cortexkit_role_tool_display::TOOL_DISPLAY_CAPABILITY],"requires":[],"must_never_reach":[]}
        })).unwrap()
        };
        let mut cache = ReaderCache::default();
        cache.update(0, &[module("arbitrary-display")]);
        for _ in 0..100 {
            assert_eq!(cache.module, Some(Some("arbitrary-display".into())));
        }
        cache.invalidate();
        assert!(cache.module.is_none());
        cache.update(0, &[module("stale")]);
        assert!(
            cache.module.is_none(),
            "disconnected or superseded snapshots cannot restore authority"
        );
        cache.update(cache.epoch, &[module("one"), module("two")]);
        assert_eq!(cache.module, Some(None));
        cache.update(cache.epoch, &[]);
        assert_eq!(cache.module, Some(None));
    }

    #[test]
    fn notice_records_require_the_discovered_display_server_to_declare_its_token() {
        use cortexkit_role_tool_display::{
            NOTICE_RECORD_DISPLAY_CAPABILITY, TOOL_DISPLAY_CAPABILITY,
        };
        let module = |id: &str, provides: &[&str]| {
            serde_json::from_value(serde_json::json!({
                "module_id":id,"ready":true,"roles":[],"control_ops":[],
                "capabilities":{"provides":provides,"requires":[],"must_never_reach":[]}
            }))
            .unwrap()
        };
        let mut cache = ReaderCache::default();
        cache.update(
            0,
            &[
                module("display", &[TOOL_DISPLAY_CAPABILITY]),
                module("other", &[NOTICE_RECORD_DISPLAY_CAPABILITY]),
            ],
        );
        assert_eq!(cache.module, Some(Some("display".into())));
        assert!(!cache.notice_capable);
        cache.update(
            0,
            &[module(
                "display",
                &[TOOL_DISPLAY_CAPABILITY, NOTICE_RECORD_DISPLAY_CAPABILITY],
            )],
        );
        assert!(cache.notice_capable);
        cache.invalidate();
        assert!(!cache.notice_capable);
    }
    #[test]
    fn display_event_matches_role_wire_vector_without_reader_suffix() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/display-reads.json")).unwrap();
        let names = cortexkit_bus_naming::AccountNames::derive("box_test").unwrap();
        assert_eq!(DISPLAY_RECORD_READY_EVENT, vector["event"]["name"]);
        assert_eq!(DISPLAY_RECORD_READY_VERSION, vector["event"]["version"]);
        let subject = names
            .event_subject(
                "provider",
                DISPLAY_RECORD_READY_EVENT,
                DISPLAY_RECORD_READY_VERSION as u32,
            )
            .unwrap();
        assert_eq!(
            subject,
            format!(
                "ck.box_test.{}",
                vector["event"]["subject"].as_str().unwrap()
            )
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body("record:1").unwrap()).unwrap(),
            vector["event"]["body"]
        );
        assert_eq!(vector["event"]["headers"]["Nats-Msg-Id"], "record:1");
        assert!(body("bad id").is_err());
        assert!(serde_json::to_vec(&super::super::health()).unwrap().len() < 12 * 1024);
    }
}
