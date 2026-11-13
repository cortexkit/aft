//! Reply budgets for tools that deliberately wait, independent of task lifetime.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

// Calls without an explicit client deadline use the catalog's default window.
// Share its constants so enlarging the transport declaration also preserves
// the ordinary worker wait inside the same reply-budget rule.
const LEGACY_TRANSPORT_MS: u64 =
    crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS + crate::subc::WORKER_REPLY_MARGIN_MS;
pub(crate) const MAX_BLOCKING_WAIT_MS: u64 =
    LEGACY_TRANSPORT_MS - safety_margin_ms(LEGACY_TRANSPORT_MS);

const fn safety_margin_ms(window_ms: u64) -> u64 {
    // Thirty seconds proved insufficient for dispatch and reply delivery. Keep
    // at least twice that, or 10% for long calls. Short transport classes retain
    // half their original window instead of losing it all to the 60-second floor.
    let fraction = window_ms / 10;
    let margin = if fraction > 60_000 { fraction } else { 60_000 };
    let half = window_ms / 2;
    if margin < half {
        margin
    } else {
        half
    }
}

fn safety_margin(window: Duration) -> Duration {
    Duration::from_millis(safety_margin_ms(
        window.as_millis().min(u64::MAX as u128) as u64
    ))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplyBudget {
    pub(crate) received_at: Instant,
    pub(crate) deadline: Instant,
    clock: WaitClock,
}

/// Project suspend-inclusive elapsed time onto the receipt's Instant timeline.
/// Task lifetime and hard-kill deadlines keep using their original clock.
#[derive(Clone, Copy, Debug)]
struct WaitClock {
    awake: Instant,
    continuous: Duration,
    #[cfg(test)]
    suspend: Option<&'static std::sync::atomic::AtomicU64>,
}

impl WaitClock {
    fn new() -> Self {
        Self {
            awake: Instant::now(),
            continuous: continuous_now(),
            #[cfg(test)]
            suspend: None,
        }
    }

    fn now(self) -> Instant {
        let elapsed = continuous_now().saturating_sub(self.continuous);
        #[cfg(test)]
        let elapsed = elapsed
            + Duration::from_millis(
                self.suspend
                    .map_or(0, |clock| clock.load(std::sync::atomic::Ordering::SeqCst)),
            );
        self.awake + elapsed
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn continuous_now() -> Duration {
    #[cfg(target_os = "linux")]
    let id = libc::CLOCK_BOOTTIME;
    // Darwin's CLOCK_MONOTONIC includes suspend; Linux's does not.
    #[cfg(target_os = "macos")]
    let id = libc::CLOCK_MONOTONIC;
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes the timespec on success.
    let result = unsafe { libc::clock_gettime(id, time.as_mut_ptr()) };
    assert_eq!(result, 0, "suspend-inclusive clock unavailable");
    // SAFETY: the successful clock_gettime call initialized time.
    let time = unsafe { time.assume_init() };
    Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn continuous_now() -> Duration {
    // Windows Instant uses QueryPerformanceCounter, which includes standby
    // and hibernation.
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed()
}

impl ReplyBudget {
    pub(crate) fn from_envelope(received_at: Instant, body: &Value) -> Self {
        let now = Instant::now();
        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        Self::from_clocks(received_at, body, now, wall_ms)
    }

    fn from_clocks(received_at: Instant, body: &Value, now: Instant, wall_ms: u64) -> Self {
        let deadline = body.get("deadline_ms").and_then(Value::as_u64).map(|ms| {
            // Pair clocks at the same point, not receipt: receipt can predate
            // decoding by a long queue wait. An expired deadline stays expired.
            if ms >= wall_ms {
                now.checked_add(Duration::from_millis(ms - wall_ms))
                    .unwrap_or(now)
            } else {
                now.checked_sub(Duration::from_millis(wall_ms - ms))
                    .unwrap_or(received_at)
            }
        });
        Self::new(received_at, deadline)
    }

    pub(crate) fn new(received_at: Instant, client_deadline: Option<Instant>) -> Self {
        let end = client_deadline
            .unwrap_or_else(|| received_at + Duration::from_millis(LEGACY_TRANSPORT_MS));
        let window = end.saturating_duration_since(received_at);
        Self::with_deadline(received_at, end - safety_margin(window))
    }

    pub(crate) fn with_deadline(received_at: Instant, deadline: Instant) -> Self {
        Self {
            received_at,
            deadline,
            clock: WaitClock::new(),
        }
    }

    pub(crate) fn cap(&self, deadline: Instant) -> Instant {
        deadline.min(self.deadline)
    }

    pub(crate) fn now(&self) -> Instant {
        self.clock.now()
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.now().saturating_duration_since(self.received_at)
    }

    pub(crate) fn io_deadline(&self, deadline: Instant) -> Instant {
        Instant::now() + deadline.saturating_duration_since(self.now())
    }

    /// Short scheduler sleeps ensure an awake-only runtime timer cannot retain
    /// a long pre-suspend remainder. Only the continuous clock decides expiry.
    pub(crate) async fn sleep_until(&self, deadline: Instant) {
        loop {
            let remaining = deadline.saturating_duration_since(self.now());
            if remaining.is_zero() {
                return;
            }
            tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        }
    }

    pub(crate) async fn timeout_at<F: std::future::Future>(
        &self,
        deadline: Instant,
        future: F,
    ) -> Result<F::Output, ()> {
        tokio::select! {
            biased;
            output = future => Ok(output),
            _ = self.sleep_until(deadline) => Err(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_suspend_clock(
        mut self,
        clock: &'static std::sync::atomic::AtomicU64,
    ) -> Self {
        self.clock.suspend = Some(clock);
        self
    }

    /// Refund restoration time only to the internal startup allowance. The
    /// caller's absolute reply deadline, including delivery margin, stays fixed.
    pub(crate) fn after_launch_restore(self, elapsed: Duration) -> Self {
        Self {
            received_at: self.received_at + elapsed,
            deadline: self.deadline,
            ..self
        }
    }
}

impl From<Instant> for ReplyBudget {
    fn from(received_at: Instant) -> Self {
        Self::new(received_at, None)
    }
}

pub(crate) fn clamp_worker_wait_ms(configured: u64) -> u64 {
    if configured > MAX_BLOCKING_WAIT_MS {
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| {
            log::warn!(
                "bash.worker_wait_max_ms={configured} clamped to {MAX_BLOCKING_WAIT_MS} ms so blocking waits reply before the transport deadline"
            );
        });
    }
    configured.min(MAX_BLOCKING_WAIT_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn suspend_makes_wait_and_reply_budget_due_at_resume() {
        use futures_util::FutureExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        let suspend = Box::leak(Box::new(AtomicU64::new(0)));
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(280)))
            .with_suspend_clock(suspend);
        let wait_deadline = budget.cap(receipt + Duration::from_secs(270));
        suspend.store(2_000_000, Ordering::SeqCst);
        assert!(
            Instant::now() < wait_deadline,
            "awake clock did not reach the wait limit"
        );
        assert!(budget.now() >= budget.deadline);
        assert!(budget.elapsed() >= Duration::from_secs(2_000));
        // Poll once at resume: no further awake-time scheduler tick is needed.
        assert!(budget.sleep_until(wait_deadline).now_or_never().is_some());
    }

    #[test]
    fn launch_restore_wait_refunds_startup_but_not_caller_deadline() {
        let receipt = Instant::now() - Duration::from_secs(20);
        let budget = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(2)));
        assert!(budget.deadline < Instant::now());
        let shifted = budget.after_launch_restore(Duration::from_secs(20));
        assert!(shifted.deadline < Instant::now());
        assert_eq!(
            shifted.received_at - budget.received_at,
            Duration::from_secs(20)
        );
        assert_eq!(shifted.deadline, budget.deadline);
        assert_eq!(
            budget.after_launch_restore(Duration::ZERO).deadline,
            budget.deadline
        );
    }

    #[test]
    fn default_worker_wait_keeps_full_thirty_minutes() {
        let configured = crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS;
        assert_eq!(
            crate::commands::bash_orchestrate::blocking_wait_cap_ms(true, configured),
            Some(1_800_000),
        );
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, None);
        let ordinary_wait = receipt + Duration::from_millis(configured);
        assert_eq!(budget.cap(ordinary_wait), ordinary_wait);
        assert_eq!(budget.deadline - receipt, Duration::from_millis(1_836_000));
    }

    #[test]
    fn older_admitted_client_window_still_has_headroom() {
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(1_830)));
        let ordinary_wait = receipt + Duration::from_secs(1_800);
        assert_eq!(
            budget.cap(ordinary_wait) - receipt,
            Duration::from_secs(1_647)
        );
    }

    #[test]
    fn no_client_deadline_clamps_even_oversized_worker_limits() {
        let receipt = Instant::now();
        let budget = ReplyBudget::new(receipt, None);
        assert_eq!(budget.deadline - receipt, Duration::from_millis(1_836_000));
        assert_eq!(clamp_worker_wait_ms(u64::MAX), 1_836_000);
        assert_eq!(clamp_worker_wait_ms(60_000), 60_000);
    }

    #[test]
    fn queue_time_consumes_the_original_reply_window() {
        let receipt = Instant::now();
        let client = receipt + Duration::from_secs(2_040);
        let budget = ReplyBudget::new(receipt, Some(client));
        let queued_for = Duration::from_secs(120);
        assert_eq!(
            budget.deadline - (receipt + queued_for),
            Duration::from_secs(1_716)
        );
        assert_eq!(client - budget.deadline, Duration::from_secs(204));
        let short = ReplyBudget::new(receipt, Some(receipt + Duration::from_secs(10)));
        assert_eq!(short.deadline - receipt, Duration::from_secs(5));
    }

    #[test]
    fn client_deadline_keeps_reader_queue_time() {
        let receipt = Instant::now();
        let body = serde_json::json!({"deadline_ms": 20_000});
        let unqueued = ReplyBudget::from_clocks(receipt, &body, receipt, 10_000);
        let queue_time = Duration::from_secs(4);
        let dispatched = receipt + queue_time;
        let queued = ReplyBudget::from_clocks(receipt, &body, dispatched, 14_000);
        assert_eq!(unqueued.deadline, queued.deadline);
        assert_eq!(unqueued.deadline - receipt, Duration::from_secs(5));
        assert_eq!(queued.deadline - dispatched, Duration::from_secs(1));
        assert_eq!(
            (unqueued.deadline - receipt) - (queued.deadline - dispatched),
            queue_time
        );
    }
}
