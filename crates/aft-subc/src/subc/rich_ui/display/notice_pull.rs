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

pub(in crate::subc) async fn stage_awaited_notice(record: PullRecord) -> Option<String> {
    let id = stage_for_display(SENDER.get()?, record, WRITE_BUDGET).await?;
    crate::late_result_bus::display_committed();
    Some(id)
}

async fn stage_for_display(
    writer: &Writer,
    record: PullRecord,
    budget: Duration,
) -> Option<String> {
    // The durable queue only announces that the card's data is ready to pull.
    // The orchestrator owns delivery of the notice that links to it.
    let PullRecord { id, bytes } = record;
    let stored = confirm_linked_write(
        writer.store.persist_awaited_notice(id.clone(), bytes),
        budget,
    )
    .await
    .ok()?;
    stored.then_some(id)
}

async fn confirm_linked_write<T>(
    write: impl std::future::Future<Output = T>,
    budget: Duration,
) -> T {
    tokio::pin!(write);
    match tokio::time::timeout(budget, &mut write).await {
        Ok(result) => result,
        // Dropping the future cannot cancel a blocking database write. Wait
        // off the shell reply path so the request gets the eventual write result.
        Err(_) => write.await,
    }
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
