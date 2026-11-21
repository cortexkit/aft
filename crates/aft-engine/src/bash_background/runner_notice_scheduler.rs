//! Shared queue for daemon notices and durable held-task retries.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[doc(hidden)]
pub static SCHEDULER: OnceLock<Result<Arc<Scheduler>, String>> = OnceLock::new();

pub(crate) const QUEUE_CAPACITY: usize = 1024;
pub(crate) type Work = Box<dyn FnMut() -> Next + Send>;

#[doc(hidden)]
pub enum Next {
    Done,
    Retry(Duration),
}

pub(crate) struct Scheduled {
    // Taking work leaves its send_id reserved until the attempt returns, so a
    // reconnect cannot start a second send for the same completion.
    pub(crate) work: Option<Work>,
    pub(crate) due: std::time::Instant,
    pub(crate) completion: bool,
    pub(crate) retrying: bool,
    expedite: bool,
    follow: Arc<Follow>,
}

#[derive(Default)]
#[doc(hidden)]
pub struct Follow {
    pub(crate) send_id: std::sync::Mutex<Option<String>>,
    pub(crate) force: AtomicBool,
    pub(crate) attached: AtomicBool,
    pub(crate) receipts: std::sync::Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
}

#[doc(hidden)]
pub struct FollowReceipt(pub(crate) Vec<tokio::sync::oneshot::Sender<()>>);
impl FollowReceipt {
    pub(crate) fn applied(self) {
        for done in self.0 {
            let _ = done.send(());
        }
    }
}

#[derive(Default)]
#[doc(hidden)]
pub struct Scheduler {
    // Delayed and in-flight notices count toward the same queue capacity limit.
    // Idle workers wait on the earliest deadline, not on one notice's backoff.
    pub(crate) entries: std::sync::Mutex<HashMap<String, Scheduled>>,
    pub(crate) wake: std::sync::Condvar,
}

impl Scheduler {
    pub(crate) fn offer(&self, id: String, completion: bool, work: Work, expedite: bool) -> bool {
        self.offer_follow(id, completion, work, expedite, Arc::new(Follow::default()))
    }

    pub(crate) fn offer_follow(
        &self,
        id: String,
        completion: bool,
        work: Work,
        expedite: bool,
        follow: Arc<Follow>,
    ) -> bool {
        let mut entries = self.entries.lock().unwrap();
        follow.attached.store(
            crate::late_result_bus::readers::sendstate_attached(),
            Ordering::Relaxed,
        );
        if let Some(entry) = entries.get_mut(&id) {
            if expedite {
                if entry.work.is_some() {
                    entry.due = std::time::Instant::now();
                } else {
                    entry.expedite = true;
                }
                self.wake.notify_all();
            }
            return true;
        }
        if entries.len() >= QUEUE_CAPACITY {
            return false;
        }
        entries.insert(
            id,
            Scheduled {
                work: Some(work),
                due: std::time::Instant::now(),
                completion,
                retrying: false,
                expedite: false,
                follow,
            },
        );
        self.wake.notify_all();
        true
    }

    pub(crate) fn bus_wake(
        &self,
        send_id: Option<&str>,
    ) -> Vec<tokio::sync::oneshot::Receiver<()>> {
        let mut entries = self.entries.lock().unwrap();
        let targeted = send_id.is_some_and(|id| {
            entries
                .values()
                .any(|entry| entry.follow.send_id.lock().unwrap().as_deref() == Some(id))
        });
        let mut waits = Vec::new();
        for entry in entries.values_mut() {
            let followed = entry.follow.send_id.lock().unwrap();
            if followed.is_none() || targeted && followed.as_deref() != send_id {
                continue;
            }
            let (done, wait) = tokio::sync::oneshot::channel();
            entry.follow.receipts.lock().unwrap().push(done);
            entry.follow.force.store(true, Ordering::Relaxed);
            if entry.work.is_some() {
                entry.due = std::time::Instant::now();
            } else {
                entry.expedite = true;
            }
            waits.push(wait);
        }
        self.wake.notify_all();
        waits
    }

    pub(crate) fn attachment_changed(&self, attached: bool) {
        for entry in self.entries.lock().unwrap().values_mut() {
            entry.follow.attached.store(attached, Ordering::Relaxed);
            if entry.follow.send_id.lock().unwrap().is_none() {
                continue;
            }
            if attached {
                entry.due = std::time::Instant::now() + crate::late_result_bus::readers::BACKSTOP;
            } else {
                entry.follow.force.store(true, Ordering::Relaxed);
                if entry.work.is_some() {
                    entry.due = std::time::Instant::now();
                } else {
                    entry.expedite = true;
                }
            }
        }
        self.wake.notify_all();
    }

    pub(crate) fn take(
        entries: &mut HashMap<String, Scheduled>,
        now: std::time::Instant,
    ) -> Option<(String, Work)> {
        let (id, entry) = entries
            .iter_mut()
            .filter(|(_, entry)| entry.work.is_some() && entry.due <= now)
            .min_by_key(|(_, entry)| entry.due)?;
        Some((id.clone(), entry.work.take().unwrap()))
    }

    pub(crate) fn finish(&self, id: &str, work: Work, next: Next, now: std::time::Instant) {
        let mut entries = self.entries.lock().unwrap();
        match next {
            Next::Done => {
                entries.remove(id);
            }
            Next::Retry(delay) => {
                let entry = entries.get_mut(id).unwrap();
                entry.work = Some(work);
                entry.due = if entry.expedite { now } else { now + delay };
                entry.expedite = false;
                entry.retrying = true;
            }
        }
        self.wake.notify_all();
    }

    fn run(&self) {
        loop {
            let (id, mut work) = {
                let mut entries = self.entries.lock().unwrap();
                loop {
                    let now = std::time::Instant::now();
                    if let Some(work) = Self::take(&mut entries, now) {
                        break work;
                    }
                    let due = entries
                        .values()
                        .filter(|entry| entry.work.is_some())
                        .map(|entry| entry.due)
                        .min();
                    entries = match due {
                        Some(due) => {
                            self.wake
                                .wait_timeout(entries, due.saturating_duration_since(now))
                                .unwrap()
                                .0
                        }
                        None => self.wake.wait(entries).unwrap(),
                    };
                }
            };
            let next = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut work))
                .unwrap_or_else(|_| {
                    log::error!(
                        "runner notice worker panicked; persisted completion remains pending"
                    );
                    Next::Done
                });
            self.finish(&id, work, next, std::time::Instant::now());
        }
    }
}

pub(crate) async fn bus_wake(send_id: Option<&str>) -> Result<(), &'static str> {
    let Some(Ok(scheduler)) = SCHEDULER.get() else {
        return Ok(());
    };
    for wait in scheduler.bus_wake(send_id) {
        wait.await.map_err(|_| "send_state_apply")?;
    }
    Ok(())
}

pub(crate) fn bus_attachment_changed(attached: bool) {
    let Some(Ok(scheduler)) = SCHEDULER.get() else {
        return;
    };
    scheduler.attachment_changed(attached);
}

#[doc(hidden)]
pub fn scheduler() -> Option<&'static Arc<Scheduler>> {
    match SCHEDULER.get_or_init(|| {
        let scheduler = Arc::new(Scheduler::default());
        for _ in 0..16 {
            let scheduler = scheduler.clone();
            std::thread::Builder::new()
                .name("aft-runner-notice".into())
                .spawn(move || scheduler.run())
                .map_err(|error| error.to_string())?;
        }
        Ok(scheduler)
    }) {
        Ok(scheduler) => Some(scheduler),
        Err(error) => {
            log::error!("runner notice workers unavailable: {error}");
            None
        }
    }
}

#[doc(hidden)]
pub fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis((250u64 << attempt.min(8)).min(60_000))
}

pub(crate) fn retry_held_notice_fallback(
    registry: crate::bash_background::BgTaskRegistry,
    task: String,
    key: crate::db::call_ledger::Key,
) {
    let Some(scheduler) = scheduler() else {
        log::warn!(
            "held notice fallback retry scheduler unavailable task={task} call_key={}",
            key.call_key
        );
        return;
    };
    let mut attempt = 0;
    let id = format!("aft:bash-recovery:{task}");
    let work: Work = Box::new(move || {
        if registry.retry_held_notice_fallback(&task, &key) {
            Next::Done
        } else {
            let delay = retry_delay(attempt);
            attempt = attempt.saturating_add(1);
            Next::Retry(delay)
        }
    });
    // A durable held call must not lose its only terminal retry when the
    // best-effort notice queue is full. Keep one retry per persisted task.
    scheduler
        .entries
        .lock()
        .unwrap()
        .entry(id)
        .or_insert_with(|| Scheduled {
            work: Some(work),
            due: std::time::Instant::now(),
            completion: false,
            retrying: false,
            expedite: false,
            follow: Arc::new(Follow::default()),
        });
    scheduler.wake.notify_all();
}
