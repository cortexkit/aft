//! Notices use the same bounded pull cache and durable store as tool records.

use std::{
    sync::TryLockError,
    time::{Duration, Instant},
};

use super::{Writer, SENDER};
use crate::late_result_bus::{NoticeViewsReady, NOTICE_VIEWS_READY};
use crate::subc::rich_ui::notices::PullRecord;

const WRITE_BUDGET: Duration = Duration::from_millis(50);

pub(in crate::subc) async fn stage_notice(record: PullRecord) {
    if let Some(writer) = SENDER.get() {
        stage_after_views(
            writer,
            record,
            &NOTICE_VIEWS_READY,
            Duration::from_millis(250),
        )
        .await;
    }
}

#[derive(Clone, Default)]
pub(in crate::subc) enum AwaitedCards {
    #[default]
    Module,
    #[cfg(all(test, unix))]
    Test(std::sync::Arc<TestAwaitedCards>),
}

impl AwaitedCards {
    fn writer(&self) -> Option<&Writer> {
        match self {
            Self::Module => SENDER.get(),
            #[cfg(all(test, unix))]
            Self::Test(test) => Some(&test.writer),
        }
    }

    pub(in crate::subc) async fn commit(&self, record: PullRecord) -> Option<String> {
        let writer = self.writer()?;
        let id = record.id.clone();
        if prepare_for_display(writer, record).await.is_some() {
            #[cfg(all(test, unix))]
            if let Self::Test(test) = self {
                if test.pause_rename.load(std::sync::atomic::Ordering::Acquire) {
                    *test.staged_id.lock() = Some(id.clone());
                    test.rename_pending.notify_one();
                    test.allow_rename.notified().await;
                }
            }
            if writer.store.commit_awaited_notice(id.clone()).await == Ok(true) {
                return Some(id);
            }
        }
        let cards = self.clone();
        tokio::spawn(async move { cards.discard(id).await });
        None
    }

    pub(in crate::subc) async fn discard(&self, id: String) {
        let Some(writer) = self.writer() else { return };
        let _ = writer.store.discard_unlinked_notice(id).await;
        #[cfg(all(test, unix))]
        if let Self::Test(test) = self {
            test.discarded.notify_one();
        }
    }

    pub(in crate::subc) fn announce(self, id: String) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let Some(writer) = self.writer() else { return };
            match writer.store.activate_awaited_notice(id.clone()).await {
                Ok(()) => crate::late_result_bus::display_committed(),
                Err(error) => log::warn!("could not activate awaited record {id}: {error}"),
            }
        })
    }
}

#[cfg(all(test, unix))]
pub(in crate::subc) struct TestAwaitedCards {
    writer: Writer,
    pub(in crate::subc) pause_rename: std::sync::atomic::AtomicBool,
    pub(in crate::subc) rename_pending: tokio::sync::Notify,
    pub(in crate::subc) allow_rename: tokio::sync::Notify,
    pub(in crate::subc) discarded: tokio::sync::Notify,
    pub(in crate::subc) staged_id: parking_lot::Mutex<Option<String>>,
}

#[cfg(all(test, unix))]
impl TestAwaitedCards {
    pub(in crate::subc) fn new(path: std::path::PathBuf) -> Self {
        Self {
            writer: super::tests::test_writer(path),
            pause_rename: Default::default(),
            rename_pending: Default::default(),
            allow_rename: Default::default(),
            discarded: Default::default(),
            staged_id: Default::default(),
        }
    }

    pub(in crate::subc) async fn read(&self, id: String) -> Option<Vec<u8>> {
        self.writer
            .store
            .read(id, crate::db::display_records::now_ms())
            .await
            .ok()
    }
}

async fn prepare_for_display(writer: &Writer, record: PullRecord) -> Option<String> {
    let PullRecord { id, bytes } = record;
    let stored = writer
        .store
        .prepare_awaited_notice(id.clone(), bytes)
        .await
        .ok()?;
    stored.then_some(id)
}

#[cfg(test)]
async fn stage_for_display(
    writer: &Writer,
    record: PullRecord,
    budget: Duration,
) -> Option<String> {
    let id = tokio::time::timeout(budget, prepare_for_display(writer, record))
        .await
        .ok()??;
    writer
        .store
        .commit_awaited_notice(id.clone())
        .await
        .ok()?
        .then_some(())?;
    writer
        .store
        .activate_awaited_notice(id.clone())
        .await
        .ok()?;
    Some(id)
}

pub(super) async fn restore_awaited() {
    let writer = SENDER.get().expect("display writer started");
    match writer.store.restore_awaited_count().await {
        Ok(count) if count > 0 => crate::late_result_bus::display_committed(),
        Ok(_) => {}
        Err(error) => log::warn!("could not restore pending awaited display records: {error}"),
    }
}

pub(crate) async fn publish_awaited_notice<F>(
    publish: impl FnOnce(String) -> F,
) -> Result<Option<String>, &'static str>
where
    F: std::future::Future<Output = Result<(), &'static str>>,
{
    let Some(writer) = SENDER.get() else {
        return Ok(None);
    };
    publish_awaited_with(writer, publish).await
}

async fn publish_awaited_with<F>(
    writer: &Writer,
    publish: impl FnOnce(String) -> F,
) -> Result<Option<String>, &'static str>
where
    F: std::future::Future<Output = Result<(), &'static str>>,
{
    let Some(id) = writer.store.pending_awaited_notice().await? else {
        return Ok(None);
    };
    if let Err(error) = publish(id.clone()).await {
        writer.store.defer_awaited_notice(id);
        return Err(error);
    }
    if let Err(error) = writer.store.acknowledged_awaited_notice(id.clone()).await {
        writer.store.defer_awaited_notice(id);
        return Err(error);
    }
    Ok(Some(id))
}

async fn stage_after_views(
    writer: &Writer,
    record: PullRecord,
    views: &NoticeViewsReady,
    budget: Duration,
) {
    if views.wait(budget).await {
        stage(writer, record, WRITE_BUDGET).await;
    } else {
        views.defer(record);
    }
}

pub(super) async fn flush_pending() {
    let writer = SENDER.get().expect("display writer started");
    while stage_pending(writer, &NOTICE_VIEWS_READY, WRITE_BUDGET).await {}
}

async fn stage_pending(writer: &Writer, views: &NoticeViewsReady, budget: Duration) -> bool {
    let Some(record) = views.next_pending().await else {
        return false;
    };
    stage(writer, record, budget).await;
    true
}

async fn stage(writer: &Writer, record: PullRecord, deadline: Duration) {
    stage_with_contention(writer, record, deadline, || {}).await;
}

async fn stage_with_contention(
    writer: &Writer,
    PullRecord { id, bytes }: PullRecord,
    deadline: Duration,
    on_contended: impl FnOnce(),
) {
    if writer
        .outbox
        .try_lock()
        .ok()
        .is_some_and(|outbox| outbox.reads.read(&id, Instant::now()).is_some())
    {
        return;
    }
    // Give the local write a short head start over the runner send. If it is
    // slow or full, the ordinary wake queue retries without delaying delivery.
    let bytes = tokio::time::timeout(deadline, async {
        if writer.store.persist(id.clone(), bytes.clone()).await == Ok(true) {
            return None;
        }
        // A replay may build different display facts (for example, without
        // live credentials). An existing send id retains its first bytes.
        writer
            .store
            .read(id.clone(), crate::db::display_records::now_ms())
            .await
            .ok()
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(bytes);
    let outbox = match writer.outbox.try_lock() {
        Ok(outbox) => Some(outbox),
        Err(TryLockError::WouldBlock) => {
            // The bus publisher only sees in-memory wakes. A stored notice
            // cannot be announced unless it enters the bounded pull cache.
            on_contended();
            writer.outbox.lock().ok()
        }
        Err(TryLockError::Poisoned(_)) => None,
    };
    if let Some(mut outbox) = outbox {
        let now = Instant::now();
        let weight = bytes.len();
        outbox.reads.insert(id.clone(), bytes, weight, now);
        outbox.reads.push_finished(&id, now);
        drop(outbox);
        crate::late_result_bus::display_committed();
    }
}

#[cfg(test)]
mod tests;
