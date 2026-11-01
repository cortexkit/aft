//! Translate work milestones into the opt-in, invisible call keepalive PUSH.
use super::*;
use crate::run_tool_call::progress::Progress;

pub(super) fn channel(
    tx: &WriterSender,
    metrics: &Arc<DispatchPathMetrics>,
    active: &ActiveToolCalls,
    frame: &Frame,
    enabled: bool,
) -> Option<Progress> {
    if !enabled {
        return None;
    }
    let tx = tx.clone();
    let metrics = metrics.clone();
    let active = Arc::downgrade(active);
    let route = route_key(frame.header.channel, frame.header.epoch);
    let corr = frame.header.corr;
    let flags = frame.header.flags;
    let send = move || {
        let Some(active) = active.upgrade() else {
            return false;
        };
        let Ok(calls) = active.try_lock() else {
            return false;
        };
        if !calls
            .get(&(route, corr))
            .is_some_and(|call| !call.answering)
        {
            return false;
        }
        // Queue under the same lock as terminal claiming, so a milestone can
        // never enqueue a PUSH after the call's terminal frame.
        let Ok(frame) = Frame::build(
            FrameType::Push,
            flags,
            route.channel,
            route.epoch,
            corr,
            cortexkit_role_tool_provider::keepalive_body().to_vec(),
        ) else {
            return false;
        };
        matches!(
            wire::try_enqueue_writer_frame(&tx, &metrics, frame),
            wire::WriterEnqueueOutcome::Enqueued
        )
    };
    #[cfg(test)]
    if let Some(clock) = tests::clock(corr) {
        let origin = Instant::now();
        return Some(Progress::with_clock(
            move || origin + Duration::from_millis(clock.load(Ordering::SeqCst)),
            send,
        ));
    }
    Some(Progress::new(send))
}

#[cfg(test)]
#[path = "keepalive_tests.rs"]
mod tests;

pub(super) fn job(
    progress: Option<Progress>,
    job: crate::executor::ExecutorJob,
) -> crate::executor::ExecutorJob {
    Box::new(move |ctx| {
        if let (Some(progress), Some(token)) =
            (progress, crate::executor::current_job_cancellation())
        {
            token.set_progress(progress);
        }
        job(ctx)
    })
}
