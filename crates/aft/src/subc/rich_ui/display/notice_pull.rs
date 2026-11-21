//! Notices use the same bounded pull cache and durable store as tool records.

use std::{
    sync::TryLockError,
    time::{Duration, Instant},
};

use super::{Writer, SENDER};
use crate::late_result_bus::{NoticeViewsReady, NOTICE_VIEWS_READY};
use crate::subc::rich_ui::notices::PullRecord;

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

async fn stage_after_views(
    writer: &Writer,
    record: PullRecord,
    views: &NoticeViewsReady,
    budget: Duration,
) {
    if views.wait(budget).await {
        stage(writer, record, Duration::from_millis(50)).await;
    }
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
