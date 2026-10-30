//! Background bind retries for transient database or cache coordination contention.
//!
//! A bind reads older standing-root pins from aft.db without waiting, so a
//! database held by another writer leaves the root's identity provisional:
//! configure turns the root's indexes off rather than guess an identity a
//! later bind might contradict. Without a retry that state lasts until the
//! folder binds again, which may never happen: a daemon restart binds dozens
//! of roots at once, so several can meet a busy database, and a long-lived
//! session keeps its bind for hours.
//!
//! So a successful bind that leaves the identity provisional only because
//! aft.db was busy arms a retry here. The configure-tail maintenance drain
//! (the executor's maintenance lane under the attached daemon, the idle
//! drain in the standalone loop) polls it with bounded backoff. Each due
//! attempt first checks, without waiting, whether the pins are readable; once
//! they are, the same configure request is replayed. That replay is the bind
//! the folder would otherwise need: it settles and saves the identity and
//! enables the indexes exactly as a successful first bind does. Under the
//! attached daemon it also, like a route bind, takes over from standing-root
//! maintenance every entry that shares the now settled key before the
//! session's indexes start loading.
//!
//! A bind whose cache coordination file stayed busy after its bounded open
//! attempts uses the same admission and root-lifetime checks for repeated
//! recovery. Cache retries back off from one second to a sixty-second ceiling,
//! without giving up while the root stays bound and its cache stays busy.
//!
//! The replay runs with the admission a bind of the root gets, never inside
//! the maintenance job that noticed the free database. Configure publishes
//! its new state in steps (configuration, then cache root and artifact
//! access, then degraded reasons), so a reader admitted beside it could see
//! indexes on while the identity still reads as provisional. A route bind
//! that changes the root runs as the actor's exclusive writer, which no read
//! overlaps; a maintenance job holds only the read gate. The attached daemon
//! therefore submits the replay as a route bind, and the standalone loop runs
//! it on its request thread, where it runs every configure request.
//!
//! The retry never outlives the bind it belongs to. A newer configure replaces
//! it (re-arming only if that bind is provisional for the same reason), an
//! unbound or quiesced root cancels it, and a root whose identity is already
//! settled drops it without replaying anything, so a retry can never re-key a
//! settled root. A provisional identity caused by unreadable Git (for example
//! a dubious-ownership refusal) never arms one: retrying cannot change Git's
//! answer, so that root still waits for its next bind.

use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::protocol::RawRequest;
use crate::search_index::{PROVISIONAL_IDENTITY_EXHAUSTED_REASON, PROVISIONAL_IDENTITY_REASON};

/// Delay before the first retry after the bind.
const FIRST_DELAY: Duration = Duration::from_millis(250);
/// Each further delay doubles up to this ceiling.
const MAX_DELAY: Duration = Duration::from_secs(10);
/// Stop retrying once this long has passed since the bind. The exhausted
/// degraded reason names this window, so keep both in step.
const GIVE_UP_AFTER: Duration = Duration::from_secs(5 * 60);
const CACHE_FIRST_DELAY: Duration = Duration::from_secs(1);
const CACHE_MAX_DELAY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct Timing {
    first_delay: Duration,
    max_delay: Duration,
    give_up_after: Duration,
}

const PRODUCTION_TIMING: Timing = Timing {
    first_delay: FIRST_DELAY,
    max_delay: MAX_DELAY,
    give_up_after: GIVE_UP_AFTER,
};

#[cfg(test)]
thread_local! {
    static TIMING_FOR_TEST: std::cell::Cell<Option<Timing>> = const { std::cell::Cell::new(None) };
    static NOW_FOR_TEST: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = NOW_FOR_TEST.with(std::cell::Cell::get) {
        return now;
    }
    Instant::now()
}

#[cfg(test)]
pub(crate) fn with_clock_for_test<T>(body: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            NOW_FOR_TEST.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(NOW_FOR_TEST.with(|slot| slot.replace(Some(Instant::now()))));
    body()
}

#[cfg(test)]
pub(crate) fn advance_clock_for_test(elapsed: Duration) {
    NOW_FOR_TEST
        .with(|slot| slot.set(Some(slot.get().expect("test clock is installed") + elapsed)));
}

fn timing() -> Timing {
    #[cfg(test)]
    if let Some(timing) = TIMING_FOR_TEST.with(std::cell::Cell::get) {
        return timing;
    }
    PRODUCTION_TIMING
}

fn cache_timing() -> Timing {
    #[cfg(test)]
    if let Some(timing) = TIMING_FOR_TEST.with(std::cell::Cell::get) {
        return timing;
    }
    Timing {
        first_delay: CACHE_FIRST_DELAY,
        max_delay: CACHE_MAX_DELAY,
        // Cache retries have no give-up window; only identity retries use it.
        give_up_after: Duration::MAX,
    }
}

/// Run `body` with retry delays and the give-up window replaced on this
/// thread, so a unit test can make an attempt due at once or exhaust it.
#[cfg(test)]
pub(crate) fn with_timing_for_test<T>(
    first_delay: Duration,
    give_up_after: Duration,
    body: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<Timing>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TIMING_FOR_TEST.with(|slot| slot.set(self.0));
        }
    }
    let previous = TIMING_FOR_TEST.with(|slot| {
        slot.replace(Some(Timing {
            first_delay,
            max_delay: first_delay,
            give_up_after,
        }))
    });
    let _restore = Restore(previous);
    body()
}

/// Delay before attempt number `attempts` (zero-based): the first delay,
/// doubling, capped at the maximum.
fn delay_before(attempts: u32, timing: Timing) -> Duration {
    timing
        .first_delay
        .saturating_mul(1u32 << attempts.min(16))
        .min(timing.max_delay)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetryKind {
    Identity,
    CacheCoordination,
}

#[derive(Clone)]
struct PendingRetry {
    /// The configure request whose bind left a retryable degradation.
    request: RawRequest,
    prepared_bind: Option<std::sync::Arc<crate::subc_config::PreparedBindConfig>>,
    kind: RetryKind,
    root: std::path::PathBuf,
    connect_generation: u64,
    /// The configure generation that bind published. Any later configure
    /// moves it, which makes this retry stale.
    generation: u64,
    armed_at: Instant,
    attempts: u32,
    next_attempt_at: Instant,
    exhausted: bool,
    // A prepared replay reserves this slot until it completes or is dropped.
    // The deadline remains armed if admission drops the prepared replay.
    cache_claim: std::sync::Weak<()>,
}

/// Per-root retry state, held by the root's [`AppContext`].
#[derive(Default)]
pub(crate) struct RetrySlot(parking_lot::Mutex<Option<PendingRetry>>);

impl RetrySlot {
    /// True when an armed retry's next attempt is due. Never waits: a held
    /// slot reads as not due, and the next maintenance tick probes again.
    pub(crate) fn is_due(&self) -> bool {
        self.0.try_lock().is_some_and(|slot| {
            slot.as_ref().is_some_and(|retry| {
                !retry.exhausted
                    && retry.cache_claim.strong_count() == 0
                    && now() >= retry.next_attempt_at
            })
        })
    }

    /// True while a retry is armed and has not given up.
    #[cfg(test)]
    pub(crate) fn is_armed(&self) -> bool {
        self.0.lock().as_ref().is_some_and(|retry| !retry.exhausted)
    }

    #[cfg(test)]
    pub(crate) fn attempts_for_test(&self) -> Option<u32> {
        self.0.lock().as_ref().map(|retry| retry.attempts)
    }
}

/// Record what a finished configure means for the retry. Called for every
/// configure, including a replay prepared by [`poll`].
pub(crate) fn note_configure_outcome(ctx: &AppContext, request: &RawRequest, success: bool) {
    if !success {
        // A failed configure publishes nothing, so the bind the retry belongs
        // to still stands. If the failed attempt advanced the generation, the
        // next drain finds the retry stale and drops it.
        return;
    }
    let reasons = ctx.degraded_reasons();
    let kind = if reasons
        .iter()
        .any(|reason| reason == crate::root_cache::CACHE_DIR_BUSY)
    {
        Some(RetryKind::CacheCoordination)
    } else if reasons
        .iter()
        .any(|reason| reason == PROVISIONAL_IDENTITY_REASON)
    {
        Some(RetryKind::Identity)
    } else {
        None
    };
    let mut slot = ctx.provisional_identity_retry().0.lock();
    let Some(kind) = kind else {
        // Settled, provisional for a reason a retry cannot fix, or not
        // needing an identity at all: nothing to retry.
        *slot = None;
        return;
    };
    let Some(root) = ctx.canonical_cache_root_opt() else {
        *slot = None;
        return;
    };
    let now = now();
    if slot.is_none() {
        crate::slog_info!(
            "bind met transient coordination contention for root={}; retrying in the background",
            ctx.canonical_cache_root_opt()
                .map(|root| root.display().to_string())
                .unwrap_or_default()
        );
    }
    *slot = Some(PendingRetry {
        request: request.clone(),
        prepared_bind: crate::subc_config::current_prepared_bind_config(),
        kind,
        root,
        connect_generation: ctx.connect_generation(),
        generation: ctx.configure_generation(),
        armed_at: now,
        attempts: 0,
        next_attempt_at: now
            + delay_before(
                0,
                match kind {
                    RetryKind::Identity => timing(),
                    RetryKind::CacheCoordination => cache_timing(),
                },
            ),
        exhausted: false,
        cache_claim: std::sync::Weak::new(),
    });
}

/// Stop retrying, for a root that unbound or quiesced. Returns whether a
/// retry was armed.
pub(crate) fn cancel(ctx: &AppContext) -> bool {
    ctx.provisional_identity_retry().0.lock().take().is_some()
}

/// Whether the current bound root has cache recovery pending, due or admitted.
/// A busy root without recovery must not promise an automatic retry to agents.
pub(crate) fn cache_retry_pending(ctx: &AppContext) -> bool {
    let Some(retry) = ctx
        .provisional_identity_retry()
        .0
        .try_lock()
        .and_then(|slot| slot.clone())
    else {
        return false;
    };
    retry.kind == RetryKind::CacheCoordination
        && !retry.exhausted
        && stale_reason(ctx, &retry).is_none()
}

/// Why `retry` is no longer safe to replay, if it is not. It may be replayed
/// only for the configure generation that armed it, while the root is bound
/// and its retryable degradation is still present. Otherwise
/// the root is unbound, a newer configure superseded the retry, the root is
/// missing, the identity is provisional for a reason a retry cannot fix, or
/// it has already settled and must not be keyed again.
///
/// Settled or not is read only from what the root's last committed configure
/// published: its degraded reasons, which keep the provisional reason until a
/// bind that settles the identity commits. The context's general artifact-key
/// cache is no evidence either way. Readers outside configure (standing-root
/// maintenance, status, runtime drains) fill it through
/// `AppContext::memoized_artifact_cache_key` with a key derived from Git
/// alone, and a replay refused after resolving its key but before its commit
/// leaves the settled key there while the root is still provisional. Dropping
/// the retry on either would leave the indexes off until the folder binds
/// again.
fn stale_reason(ctx: &AppContext, retry: &PendingRetry) -> Option<&'static str> {
    // A root with no bound route does no indexing.
    if ctx.subc_unbound_quiesced() {
        return Some("unbound");
    }
    if retry.generation != ctx.configure_generation() {
        return Some("superseded");
    }
    match retry.kind {
        RetryKind::CacheCoordination => {
            if retry.connect_generation != ctx.connect_generation() {
                return Some("newer_connect");
            }
            if !ctx
                .degraded_reasons()
                .iter()
                .any(|reason| reason == crate::root_cache::CACHE_DIR_BUSY)
            {
                return Some("settled");
            }
        }
        RetryKind::Identity => match ctx.artifact_identity_provisional_reason() {
            None => return Some("settled"),
            Some(reason) if reason != PROVISIONAL_IDENTITY_REASON => {
                return Some("not_provisional");
            }
            Some(_) => {}
        },
    }
    if ctx.canonical_cache_root_opt().as_ref() != Some(&retry.root) {
        return Some("root_changed");
    }
    None
}

/// Run one due retry check. A retry that is no longer current is dropped.
/// While aft.db is still busy the retry is rescheduled and this returns
/// `None`. Once the pins are readable it returns the bind to replay, which
/// the caller must run with the admission a bind of this root gets: the
/// attached daemon submits it as a route bind
/// ([`crate::executor::Executor::submit_bind_cancellable_async`]); the
/// standalone loop runs it on its request thread, where it runs configure
/// requests.
///
/// Called only from configure-tail maintenance, never from an interactive
/// request.
pub(crate) fn poll(ctx: &AppContext) -> Option<PreparedReplay> {
    let slot = ctx.provisional_identity_retry();
    if !slot.is_due() {
        return None;
    }
    let (retry, cache_claim) = {
        let mut pending = slot.0.lock();
        let retry = pending.as_mut()?;
        if retry.exhausted || retry.cache_claim.strong_count() > 0 || now() < retry.next_attempt_at
        {
            return None;
        }
        if retry.kind == RetryKind::CacheCoordination {
            let claim = std::sync::Arc::new(());
            retry.cache_claim = std::sync::Arc::downgrade(&claim);
            // Keep a bounded fallback deadline if admission drops the replay.
            retry.next_attempt_at = now() + delay_before(retry.attempts, cache_timing());
            (retry.clone(), Some(claim))
        } else {
            retry.attempts = retry.attempts.saturating_add(1);
            (pending.take()?, None)
        }
    };
    if stale_reason(ctx, &retry).is_some() {
        let mut pending = slot.0.lock();
        if pending.as_ref().is_some_and(|current| {
            current.generation == retry.generation
                && current.root == retry.root
                && current.armed_at == retry.armed_at
        }) {
            pending.take();
        }
        return None;
    }
    let root = retry.root.clone();
    if retry.kind == RetryKind::Identity {
        let storage = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
        if !crate::root_identity::legacy_pins_readable(&root, &storage) {
            reschedule(ctx, &root, retry, timing());
            return None;
        }
    }
    Some(PreparedReplay {
        retry: std::sync::Arc::new(parking_lot::Mutex::new(Some(retry))),
        root,
        _cache_claim: cache_claim,
    })
}

/// Poll and, when the database is free, replay the bind on this thread.
/// Only for callers that run configure requests on the calling thread
/// themselves (the standalone loop, in-process embedders and tests); the
/// attached daemon uses [`poll`] and submits the replay as a bind.
///
/// The replay hands nothing over from standing-root maintenance: standing
/// roots are served only by the attached daemon's subc module, which replays
/// through [`PreparedReplay::run`] with its standing handoff, so these
/// callers have no standing entry a settled key could share.
pub(crate) fn drain_inline(ctx: &AppContext) -> bool {
    match poll(ctx) {
        Some(replay) => {
            replay.run(ctx, |_| {});
            true
        }
        None => false,
    }
}

/// A bind ready to be replayed because aft.db became readable. Cloning
/// shares the one pending replay, so the executor may run it more than once
/// (a shared-hold run that must rerun as an exclusive writer), and only the
/// run that completes the configure settles the retry.
#[derive(Clone)]
pub(crate) struct PreparedReplay {
    retry: std::sync::Arc<parking_lot::Mutex<Option<PendingRetry>>>,
    root: std::path::PathBuf,
    _cache_claim: Option<std::sync::Arc<()>>,
}

impl std::fmt::Debug for PreparedReplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedReplay")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl PreparedReplay {
    /// Executor request id. The `subc-bind-` prefix is what makes the
    /// executor admit the job as a route bind.
    pub(crate) fn request_id(&self) -> String {
        format!("subc-bind-identity-retry-{}", self.root.display())
    }

    /// The session whose bind is replayed.
    pub(crate) fn session(&self) -> String {
        self.retry
            .lock()
            .as_ref()
            .map(|retry| retry.request.session().to_string())
            .unwrap_or_default()
    }

    /// Replay the bind. The checks in [`stale_reason`] run again here, at
    /// admission, because a real bind, an unbind or a settle may have
    /// happened since [`poll`] prepared it.
    ///
    /// `bind_handoff` runs once the replayed configure has committed, still
    /// inside this bind's admission: the same step a route bind runs after
    /// its configure. Under the attached daemon it pauses standing-root
    /// maintenance of every entry whose artifact key now equals the root's
    /// settled key. The provisional bind could not pause them, because it had
    /// no key yet and claimed only the standing entry for its own folder, so
    /// a sibling checkout of the same repository may still be publishing
    /// into the family the session is about to build. The session's index
    /// loads start only from its configure tail, which runs after this bind
    /// returns, so the handoff always precedes the session's first build.
    pub(crate) fn run(
        &self,
        ctx: &AppContext,
        bind_handoff: impl FnOnce(&AppContext),
    ) -> crate::protocol::Response {
        let id = self.request_id();
        let Some(mut retry) = self.retry.lock().clone() else {
            return skipped(id, "already_replayed");
        };
        if let Some(reason) = stale_reason(ctx, &retry) {
            self.retry.lock().take();
            return skipped(id, reason);
        }
        if retry.kind == RetryKind::CacheCoordination {
            retry.attempts = retry.attempts.saturating_add(1);
        }
        let run = || match retry.kind {
            RetryKind::Identity => {
                crate::commands::configure::handle_configure(&retry.request, ctx)
            }
            RetryKind::CacheCoordination => {
                // Cache recovery must preserve the running configuration,
                // including edits deferred until the next real connect.
                let mut prepared = (*ctx.config()).clone();
                crate::root_cache::prepare_cache_root(&mut prepared);
                crate::commands::configure::handle_cache_recovery(
                    &retry.request,
                    retry.connect_generation,
                    ctx,
                    &prepared,
                )
            }
        };
        let response = match retry.prepared_bind.as_ref() {
            Some(prepared) => crate::subc_config::with_prepared_bind_config(prepared.clone(), run),
            None => run(),
        };
        if response.data["code"] == "configure_needs_exclusive" {
            // A shared-hold run handed its worker back before changing the
            // root; the executor runs this job again as an exclusive writer.
            return response;
        }
        if response.success {
            bind_handoff(ctx);
        }
        if self.retry.lock().take().is_some() {
            finish_replay(ctx, &self.root, retry, &response);
        }
        response
    }
}

fn skipped(id: String, reason: &str) -> crate::protocol::Response {
    crate::protocol::Response::success(id, serde_json::json!({ "skipped": reason }))
}

/// Record the outcome of a replayed bind.
fn finish_replay(
    ctx: &AppContext,
    root: &std::path::Path,
    retry: PendingRetry,
    response: &crate::protocol::Response,
) {
    let timing = timing();
    let slot = ctx.provisional_identity_retry();
    if ctx.subc_unbound_quiesced() {
        // The root unbound while the replay ran (a cancelled replay ends
        // here). An unbound root does no indexing, so it keeps no retry; its
        // next bind arms a fresh one if aft.db is still busy.
        slot.0.lock().take();
        return;
    }
    if retry.kind == RetryKind::CacheCoordination {
        if let Some(delay) = reschedule_cache_retry(ctx, &retry) {
            crate::slog_debug!(
                "cache coordination retry failed for root={} attempt={}; retrying in {}s",
                root.display(),
                retry.attempts,
                delay.as_secs_f64()
            );
            if cache_failure_reports_info(retry.attempts) {
                crate::slog_info!("cache coordination still busy for root={} after {} retry attempt(s); retrying in {}s", root.display(), retry.attempts, delay.as_secs_f64());
            }
        }
        return;
    }
    let mut rearmed = slot.0.lock();
    match rearmed.take() {
        Some(next) if next.kind != retry.kind => {
            *rearmed = Some(next);
        }
        // The database was taken again between the check and the replay, so
        // the replay re-armed the retry: keep the original window and backoff
        // instead of starting over.
        Some(mut next) => {
            next.armed_at = retry.armed_at;
            next.attempts = retry.attempts;
            drop(rearmed);
            reschedule(ctx, root, next, timing);
        }
        None if response.success => {
            crate::slog_info!(
                "identity settled for root={} after aft.db became free ({} attempt(s), {} ms after bind); indexes enabled",
                root.display(),
                retry.attempts,
                retry.armed_at.elapsed().as_millis()
            );
        }
        None => {
            // The replay failed outright, so the provisional bind still
            // stands. Retry it on the same schedule.
            drop(rearmed);
            crate::slog_warn!(
                "identity retry for root={} could not replay the bind: {}",
                root.display(),
                response.data
            );
            reschedule(ctx, root, retry, timing);
        }
    }
}

fn cache_failure_reports_info(attempts: u32) -> bool {
    attempts == 1 || (attempts > 0 && attempts % 10 == 0)
}

/// Continue the same cache recovery chain, without reviving cancelled work.
/// Holding the slot through the bound check and publication pairs with unbind's
/// cancellation: an unbind either prevents this schedule or removes it afterward.
fn reschedule_cache_retry(ctx: &AppContext, retry: &PendingRetry) -> Option<Duration> {
    let mut slot = ctx.provisional_identity_retry().0.lock();
    let pending = slot.as_mut()?;
    if pending.kind != RetryKind::CacheCoordination
        || pending.root != retry.root
        || pending.connect_generation != retry.connect_generation
        || ctx.subc_unbound_quiesced()
        || ctx.canonical_cache_root_opt().as_ref() != Some(&retry.root)
        || ctx.connect_generation() != retry.connect_generation
        || !ctx
            .degraded_reasons()
            .iter()
            .any(|reason| reason == crate::root_cache::CACHE_DIR_BUSY)
    {
        return None;
    }
    pending.armed_at = retry.armed_at;
    pending.attempts = retry.attempts;
    pending.generation = ctx.configure_generation();
    pending.exhausted = false;
    pending.cache_claim = std::sync::Weak::new();
    let delay = delay_before(retry.attempts, cache_timing());
    pending.next_attempt_at = now() + delay;
    Some(delay)
}

/// Store `retry` with its next attempt time, or give up once its window has
/// passed and say so in the root's degraded reasons.
fn reschedule(ctx: &AppContext, root: &std::path::Path, mut retry: PendingRetry, timing: Timing) {
    let now = now();
    if now.duration_since(retry.armed_at) >= timing.give_up_after {
        retry.exhausted = true;
        ctx.replace_degraded_reason(
            PROVISIONAL_IDENTITY_REASON,
            PROVISIONAL_IDENTITY_EXHAUSTED_REASON,
        );
        crate::slog_warn!(
            "identity retry gave up for root={} after {} attempt(s): aft.db stayed busy; indexes stay off until the folder binds again",
            root.display(),
            retry.attempts
        );
    } else {
        retry.next_attempt_at = now + delay_before(retry.attempts, timing);
    }
    *ctx.provisional_identity_retry().0.lock() = Some(retry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_advances_without_sleeping() {
        with_clock_for_test(|| {
            let before = now();
            advance_clock_for_test(Duration::from_secs(2));
            assert_eq!(now().duration_since(before), Duration::from_secs(2));
        });
    }

    #[test]
    fn cache_backoff_doubles_to_sixty_seconds_without_giving_up() {
        let timing = cache_timing();
        let actual = (0..9)
            .map(|attempt| delay_before(attempt, timing).as_secs())
            .collect::<Vec<_>>();
        assert_eq!(actual, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(delay_before(u32::MAX, timing), Duration::from_secs(60));
    }

    #[test]
    fn cache_failed_retry_info_is_first_and_every_tenth() {
        let attempts = (0..=31)
            .filter(|attempt| cache_failure_reports_info(*attempt))
            .collect::<Vec<_>>();
        assert_eq!(attempts, vec![1, 10, 20, 30]);
    }

    #[test]
    fn backoff_doubles_from_the_first_delay_to_the_ceiling() {
        let delays: Vec<_> = (0..8)
            .map(|attempt| delay_before(attempt, PRODUCTION_TIMING))
            .collect();
        assert_eq!(
            delays,
            [250, 500, 1_000, 2_000, 4_000, 8_000, 10_000, 10_000].map(Duration::from_millis)
        );
        assert_eq!(delay_before(u32::MAX, PRODUCTION_TIMING), MAX_DELAY);
    }

    #[test]
    fn exhausted_reason_names_the_give_up_window() {
        let minutes = GIVE_UP_AFTER.as_secs() / 60;
        assert!(
            PROVISIONAL_IDENTITY_EXHAUSTED_REASON.contains(&format!("for {minutes} minutes")),
            "{PROVISIONAL_IDENTITY_EXHAUSTED_REASON}"
        );
    }
}
