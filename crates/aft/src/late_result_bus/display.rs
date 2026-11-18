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

#[derive(Default)]
struct ReaderCache {
    epoch: u64,
    module: Option<Option<String>>,
}

impl ReaderCache {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.module = None;
    }

    fn update(&mut self, epoch: u64, modules: &[subc_client_rs::CatalogEntry]) {
        if self.epoch == epoch {
            self.module = Some(crate::subc::rich_ui::display::declared_display(modules));
        }
    }
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
                        crate::access_policy::prefrontal::observe_catalog(
                            catalog.modules.iter().map(|entry| entry.module_id.as_str()),
                        );
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

fn body(id: &str) -> Result<Vec<u8>, &'static str> {
    let notice = DisplayRecordReady::new(id);
    notice.validate().map_err(|_| "display_bus_id")?;
    serde_json::to_vec(&notice).map_err(|_| "display_bus_body")
}

pub(super) async fn publish_one(client: &BusClient) {
    let Some((id, pull_bytes)) = crate::subc::rich_ui::display::with_outbox(|outbox| {
        let now = std::time::Instant::now();
        let id = outbox.reads.due(now)?;
        let bytes = outbox.reads.read(&id, now)?;
        Some((id, bytes))
    })
    .flatten() else {
        return;
    };
    let result = crate::subc::rich_ui::display::announce(id.clone(), pull_bytes, async {
        match body(&id) {
            Ok(bytes) => tokio::time::timeout(
                super::IO_TIMEOUT,
                client.publish_event(
                    DISPLAY_RECORD_READY_EVENT,
                    DISPLAY_RECORD_READY_VERSION as u32,
                    None,
                    &id,
                    bytes,
                ),
            )
            .await
            .map_err(|_| "display_bus_timeout")
            .and_then(|result| {
                result
                    .map(|_| ())
                    .map_err(|error| super::error_code(&error))
            }),
            Err(code) => Err(code),
        }
    })
    .await;
    completed(&id, result);
}

fn completed(id: &str, result: Result<(), &'static str>) {
    crate::subc::rich_ui::display::with_outbox(|outbox| {
        outbox
            .reads
            .published(id, result.is_ok(), std::time::Instant::now());
    });
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
