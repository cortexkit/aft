//! Coalesced retry-eviction work. SQLite never runs on the SubC frame loop.
use super::*;
use crate::protocol::BashPatternMatchFrame;
use std::sync::{Mutex, Weak};

static PENDING_CHECKS: AtomicUsize = AtomicUsize::new(0);
static NEXT_EVICTION: AtomicU64 = AtomicU64::new(1);
static EVICTION_EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub(crate) fn eviction_epoch() -> &'static str {
    EVICTION_EPOCH.get_or_init(|| {
        format!(
            "{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    })
}

#[derive(Clone)]
pub(crate) struct Candidate {
    pub eviction_epoch: String,
    pub last_eviction: u64,
    pub fingerprint: Vec<u8>,
    pub frame: BashPatternMatchFrame,
    pub notifications: u64,
    pub first_version: u64,
    pub last_version: u64,
    pub first_offset: Option<u64>,
    pub last_offset: Option<u64>,
    pub stream: Option<String>,
    pub same_generation: bool,
    pub same_occurrence: bool,
}

impl Candidate {
    fn new(mut frame: BashPatternMatchFrame) -> Self {
        use sha2::Digest;
        let fingerprint = sha2::Sha256::digest(frame.match_text.as_bytes()).to_vec();
        let (first_offset, last_offset, stream) = if let Some(batch) = &frame.batch {
            let stream = batch.lines.first().map(|line| line.stream.clone());
            if batch
                .lines
                .iter()
                .all(|line| Some(&line.stream) == stream.as_ref())
            {
                (
                    batch
                        .lines
                        .first()
                        .map(|line| line.offset)
                        .or(batch.first_dropped_offset),
                    batch.lines.last().map(|line| {
                        line.offset.saturating_add(
                            line.count
                                .saturating_mul(line.end_offset.saturating_sub(line.offset)),
                        )
                    }),
                    stream.or(batch.first_dropped_stream.clone()),
                )
            } else {
                (None, None, None)
            }
        } else {
            (Some(frame.match_offset), Some(frame.match_offset), None)
        };
        let version = frame.match_offset;
        frame.match_text = String::new();
        frame.context = String::new();
        if let Some(batch) = &mut frame.batch {
            batch.lines = Vec::new();
        }
        Self {
            eviction_epoch: eviction_epoch().to_string(),
            last_eviction: NEXT_EVICTION.fetch_add(1, Ordering::Relaxed),
            fingerprint,
            frame,
            notifications: 1,
            first_version: version,
            last_version: version,
            first_offset,
            last_offset,
            stream,
            same_generation: true,
            same_occurrence: true,
        }
    }

    fn merge(&mut self, next: Self) {
        self.same_occurrence &= next.same_occurrence
            && self.fingerprint == next.fingerprint
            && self.first_version == next.first_version;
        self.last_eviction = self.last_eviction.max(next.last_eviction);
        self.notifications = self.notifications.saturating_add(next.notifications);
        self.first_version = self.first_version.min(next.first_version);
        self.last_version = self.last_version.max(next.last_version);
        self.same_generation &= next.same_generation
            && self
                .frame
                .batch
                .as_ref()
                .and_then(|b| b.pending_id.as_ref())
                == next
                    .frame
                    .batch
                    .as_ref()
                    .and_then(|b| b.pending_id.as_ref());
        if self.stream == next.stream {
            self.first_offset = self
                .first_offset
                .zip(next.first_offset)
                .map(|(a, b)| a.min(b));
            self.last_offset = self
                .last_offset
                .zip(next.last_offset)
                .map(|(a, b)| a.max(b));
        } else {
            self.first_offset = None;
            self.last_offset = None;
            self.stream = None;
        }
    }
}

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
