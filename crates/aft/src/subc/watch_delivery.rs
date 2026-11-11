//! Coalesced retry-eviction work. SQLite never runs on the SubC frame loop.
use super::*;
use crate::protocol::BashPatternMatchFrame;
use std::sync::{Mutex, Weak};

static PENDING_CHECKS: AtomicUsize = AtomicUsize::new(0);
pub(crate) use crate::task_watch_lines::Candidate;

type Key = (push::ReplayKey, String, String);

#[derive(Default)]
pub(super) struct WatchDelivery {
    pending: Mutex<HashMap<Key, Candidate>>,
    executor: Mutex<Weak<Executor>>,
    running: AtomicBool,
    in_flight: AtomicUsize,
}

impl WatchDelivery {
    pub(super) fn install(&self, executor: &Arc<Executor>) {
        *self.executor.lock().unwrap() = Arc::downgrade(executor);
    }

    pub(super) fn evicted(self: &Arc<Self>, key: push::ReplayKey, frame: PushFrame) {
        let PushFrame::BashPatternMatch(frame) = frame else {
            return;
        };
        if frame.reason == "task_exit" {
            return;
        }
        let key = (key, frame.task_id.clone(), frame.watch_id.clone());
        let candidate = Candidate::new(frame);
        self.pending
            .lock()
            .unwrap()
            .entry(key)
            .and_modify(|old| old.merge(candidate.clone()))
            .or_insert_with(|| {
                PENDING_CHECKS.fetch_add(1, Ordering::Relaxed);
                candidate
            });
        self.kick();
    }

    pub(super) fn health(&self) -> serde_json::Value {
        let attached = self.executor.lock().unwrap().upgrade().is_some();
        let (gaps, missing, uncertain) = if attached {
            crate::db::bash_watches::watch_gap_health()
        } else {
            (0, 0, false)
        };
        let pending = if attached {
            PENDING_CHECKS.load(Ordering::Relaxed)
        } else {
            0
        };
        health_from_census(gaps, missing, uncertain, pending)
    }

    pub(super) fn kick(self: &Arc<Self>) {
        let Some(executor) = self.executor.lock().unwrap().upgrade() else {
            return;
        };
        if self.pending.lock().unwrap().is_empty() || self.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let worker = self.clone();
        tokio::task::spawn_blocking(move || {
            let candidates = {
                let mut pending = worker.pending.lock().unwrap();
                worker.in_flight.store(pending.len(), Ordering::Relaxed);
                std::mem::take(&mut *pending)
            };
            for (key, candidate) in candidates {
                let result = executor
                    .actor_context(&key.0.root)
                    .ok_or_else(|| "watch registry unavailable".to_string())
                    .and_then(|ctx| {
                        ctx.bash_background()
                            .retain_retry_gap(&key.0.harness, &candidate)
                    });
                if let Err(error) = result {
                    log::warn!("retaining watch retry gap failed: {error}");
                    worker
                        .pending
                        .lock()
                        .unwrap()
                        .entry(key)
                        .and_modify(|new| {
                            new.merge(candidate.clone());
                            PENDING_CHECKS.fetch_sub(1, Ordering::Relaxed);
                        })
                        .or_insert(candidate);
                } else {
                    PENDING_CHECKS.fetch_sub(1, Ordering::Relaxed);
                }
                worker.in_flight.fetch_sub(1, Ordering::Relaxed);
            }
            worker.running.store(false, Ordering::Release);
            if !worker.pending.lock().unwrap().is_empty() {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    worker.kick();
                });
            }
        });
    }
}

pub(super) fn health_from_census(
    gaps: u64,
    missing: u64,
    uncertain: bool,
    pending: usize,
) -> serde_json::Value {
    let reason = if gaps > 0 {
        Some("retry_overflow")
    } else if pending > 0 || uncertain {
        Some("watch_gap_persistence_unconfirmed")
    } else {
        None
    };
    serde_json::json!({
        "evicted_transport_copies_total": push::reliable_push_evictions(),
        "outstanding_watch_gaps": gaps,
        "known_missing_occurrences": missing,
        "pending_gap_checks": pending,
        "status": if reason.is_some() { "degraded" } else { "ok" },
        "reason": reason,
    })
}

#[cfg(test)]
pub(crate) fn stalled_route_eviction(frame: BashPatternMatchFrame) -> Option<Candidate> {
    let metrics = DispatchPathMetrics::new();
    let key = push::ReplayKey {
        principal: Some("direct".to_string()),
        root: ProjectRootId::from_path(std::env::current_dir().unwrap()).unwrap(),
        harness: "opencode".to_string(),
        session: frame.session_id.clone(),
    };
    let mut retry = RetryBuffer::new();
    let channel = RouteChannel {
        channel: 1,
        epoch: 1,
    };
    push::buffer_retry_frame(
        &metrics,
        &mut retry,
        channel,
        key.clone(),
        PushFrame::BashPatternMatch(frame),
    );
    for _ in 0..PUSH_BUFFER_MAX_PER_KEY {
        let mut filler = crate::protocol::BashPatternMatchFrame::task_exit(
            "filler",
            &key.session,
            "exit",
            "exit",
        );
        filler.once = false;
        push::buffer_retry_frame(
            &metrics,
            &mut retry,
            channel,
            key.clone(),
            PushFrame::BashPatternMatch(filler),
        );
    }
    let candidate = metrics
        .watch_delivery
        .pending
        .lock()
        .unwrap()
        .drain()
        .next()
        .map(|(_, c)| c);
    if candidate.is_some() {
        PENDING_CHECKS.fetch_sub(1, Ordering::Relaxed);
    }
    candidate
}
