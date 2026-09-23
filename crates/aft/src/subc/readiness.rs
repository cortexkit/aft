//! Module readiness: register not-ready, warm, then flip ready.
//!
//! AFT sends `ready: false` on its HELLO manifest. While the module is not
//! ready the daemon answers new route opens with `module_warming`, so this
//! module owns one promise: the module flips to ready within a fixed budget,
//! no matter what the daemon or the warmer does. A module stuck not-ready
//! refuses every session, which is worse than having no readiness at all.
//!
//! The sequence (see `docs/design/subc-readiness-warmup.md`):
//! 1. Ask the daemon which roots have live or pending routes
//!    (`supervisor.live_roots`, a module-originated channel-0 request).
//! 2. Classify the reply. "No bindings at all" is a positive statement from
//!    the daemon; "bindings whose root is unknown" is not "no roots".
//! 3. Hand the known roots to a [`RootWarmer`] under the budget's deadline.
//! 4. Flip ready with `catalog.update { ready: true }`, retrying failures with
//!    backoff until one succeeds.
//!
//! Any failure of the query (not advertised, refused, timed out, closed,
//! undecodable) flips ready at once. The warm step can only make the flip
//! later, never prevent it: the budget cuts it off.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use std::collections::HashMap;

use subc_protocol::manifest::ProviderRole;
use subc_protocol::session::{
    LiveRoot, ModuleControlRequestFromModule, ModuleControlResponseToModule,
    MODULE_TO_SUBC_OP_CATALOG_UPDATE,
};
use subc_protocol::{ErrorBody, Frame, FrameType};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::health::DispatchPathMetrics;
use super::manifest::control_flags;
use super::wire::{send_reliable_writer_frame, WriterSender};

/// Warm-up budget on a plain start (first launch or restart). Callers see
/// `module_warming` for this long at most, and the subc SDKs retry that code
/// only until their 30 s route-open deadline, so the budget must stay well
/// inside it.
pub(super) const PLAIN_START_WARM_BUDGET: Duration = Duration::from_secs(10);

/// Warm-up budget for a blue/green swap candidate. The incumbent keeps
/// serving until cutover, so nobody waits on this budget; it is a ceiling
/// against a warm-up that hangs (about twice the measured full warm burst).
pub(super) const SWAP_CANDIDATE_WARM_BUDGET: Duration = Duration::from_secs(90);

/// Environment variable the subc supervisor sets on a swap candidate only.
pub(super) const SPAWN_ROLE_ENV: &str = "SUBC_SPAWN_ROLE";
const SWAP_CANDIDATE_ROLE: &str = "swap_candidate";

/// The daemon op name for the live-root query, as advertised in HelloAck.
pub(super) const LIVE_ROOTS_OP: &str = "supervisor.live_roots";

/// One live-root query may not take longer than this even when the budget is
/// larger, so a daemon that never answers flips a swap candidate quickly too.
const LIVE_ROOTS_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for one `catalog.update` attempt before it counts as failed.
const READY_FLIP_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
const READY_FLIP_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const READY_FLIP_MAX_BACKOFF: Duration = Duration::from_secs(10);

pub(super) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How the supervisor started this process. It selects the warm-up budget
/// and nothing else: the variable is set by whoever launches the process, so
/// nothing security-relevant may ever be keyed on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpawnRole {
    PlainStart,
    SwapCandidate,
}

impl SpawnRole {
    /// Only the exact value `swap_candidate` selects the swap budget; absence
    /// or any other value is a plain start.
    pub(super) fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some(SWAP_CANDIDATE_ROLE) => Self::SwapCandidate,
            _ => Self::PlainStart,
        }
    }

    /// Read the process environment. Called once, before HELLO.
    pub(super) fn from_process_env() -> Self {
        Self::from_env_value(std::env::var(SPAWN_ROLE_ENV).ok().as_deref())
    }

    pub(super) fn warm_budget(self) -> Duration {
        match self {
            Self::PlainStart => PLAIN_START_WARM_BUDGET,
            Self::SwapCandidate => SWAP_CANDIDATE_WARM_BUDGET,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::PlainStart => "plain start",
            Self::SwapCandidate => "swap candidate",
        }
    }
}

/// Why a daemon control request did not produce a usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ControlError {
    /// The daemon's HelloAck does not list the op, so it cannot serve it.
    NotAdvertised(&'static str),
    /// The daemon answered with a channel-0 Error frame.
    Refused {
        code: String,
        message: String,
    },
    Timeout,
    ConnectionClosed,
    /// The reply could not be decoded, or was the wrong reply kind.
    Decode(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAdvertised(op) => write!(f, "daemon does not advertise {op}"),
            Self::Refused { code, message } => write!(f, "{code}: {message}"),
            Self::Timeout => f.write_str("timed out"),
            Self::ConnectionClosed => f.write_str("daemon connection closed"),
            Self::Decode(detail) => write!(f, "undecodable reply: {detail}"),
        }
    }
}

/// The daemon's live-root snapshot, as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LiveRootsReply {
    pub(super) roots: Vec<LiveRoot>,
    pub(super) unknown_root_bindings: u64,
    pub(super) total_bindings: u64,
}

/// The roots worth warming, plus what the daemon could not name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WarmSet {
    /// Canonical project roots with at least one bound or pending route.
    pub(super) known_roots: Vec<PathBuf>,
    /// Bindings whose root the daemon did not record. Those roots exist but
    /// cannot be named, so they warm lazily on their first bind.
    pub(super) unknown_root_bindings: u64,
    pub(super) total_bindings: u64,
}

/// The three answers the design distinguishes. They stay separate variants
/// so "the daemon could not name the roots" can never be read as "there are
/// no roots".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LiveSet {
    /// The daemon positively reports no bindings: nothing to warm.
    NoBindings,
    /// Every binding has a known root.
    KnownRoots(WarmSet),
    /// Some bindings have no recorded root. The known ones are warmed; the
    /// rest warm lazily.
    WithUnknownRoots(WarmSet),
}

pub(super) fn classify_live_roots(reply: &LiveRootsReply) -> LiveSet {
    let known_roots: Vec<PathBuf> = reply
        .roots
        .iter()
        .filter(|root| root.bound.saturating_add(root.pending) > 0)
        .map(|root| root.project_root.clone())
        .collect();
    if reply.total_bindings == 0 && reply.unknown_root_bindings == 0 && known_roots.is_empty() {
        return LiveSet::NoBindings;
    }
    if reply.total_bindings == 0 {
        // The daemon's invariant is total == sum(bound + pending) + unknown,
        // read under one lock. A zero total with roots listed breaks it; warm
        // what is listed rather than trusting the zero.
        log::warn!(
            "readiness: live_roots reply reports 0 total bindings but lists {} root(s) and {} unknown-root binding(s)",
            known_roots.len(),
            reply.unknown_root_bindings
        );
    }
    let set = WarmSet {
        known_roots,
        unknown_root_bindings: reply.unknown_root_bindings,
        total_bindings: reply.total_bindings,
    };
    if set.unknown_root_bindings > 0 {
        LiveSet::WithUnknownRoots(set)
    } else {
        LiveSet::KnownRoots(set)
    }
}

/// What happened to one root during warm-up.
// `NoWarmer` only ever skips; `Warmed` and `TimedOut` are the outcomes a
// load-only warmer reports, and the tests exercise their counting.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RootWarmOutcome {
    /// Persisted artifacts were loaded and are resident.
    Warmed,
    /// Nothing was loaded; the root warms lazily on first use.
    Skipped { reason: String },
    /// The root was still loading when the budget ran out.
    TimedOut,
}

/// Loads a root's already-persisted artifacts ahead of its first bind.
///
/// Contract for implementations: load only what is already on disk, never
/// start a cold build, and stop by `deadline`. The orchestrator enforces the
/// deadline regardless by dropping the future, so an implementation that
/// overruns only loses its unfinished roots. A root that is never bound must
/// not stay resident past the existing idle eviction rules.
pub(super) trait RootWarmer: Send + Sync {
    /// Short name for the readiness log line.
    fn name(&self) -> &'static str;

    fn warm<'a>(
        &'a self,
        set: &'a WarmSet,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<Vec<(PathBuf, RootWarmOutcome)>, String>>;
}

/// The production warmer today. AFT's only artifact load path runs inside
/// configure after a bind, and reaching it for a root with no route would
/// bypass the rule that unbound roots run no indexing or maintenance. Until a
/// load-only path exists, every root is skipped and warms lazily as before.
pub(super) struct NoWarmer;

impl RootWarmer for NoWarmer {
    fn name(&self) -> &'static str {
        "none"
    }

    fn warm<'a>(
        &'a self,
        set: &'a WarmSet,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<Vec<(PathBuf, RootWarmOutcome)>, String>> {
        Box::pin(async move {
            Ok(set
                .known_roots
                .iter()
                .map(|root| {
                    (
                        root.clone(),
                        RootWarmOutcome::Skipped {
                            reason: "no load-only warmer installed".to_string(),
                        },
                    )
                })
                .collect())
        })
    }
}

/// The two daemon requests readiness needs.
pub(super) trait ReadinessControl: Send + Sync {
    fn live_roots(&self) -> BoxFuture<'_, Result<LiveRootsReply, ControlError>>;
    fn flip_ready(&self) -> BoxFuture<'_, Result<(), ControlError>>;
}

/// Why and how the module flipped ready. Rendered as the readiness log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FlipCause {
    /// The live-root query failed in any way; nothing was warmed.
    QueryFailed(ControlError),
    /// The daemon reported no bindings at all.
    NoBindings,
    /// The warmer ran to completion (or had no known root to run on).
    WarmFinished {
        set: WarmSet,
        warmed: usize,
        skipped: usize,
        timed_out: usize,
    },
    /// The warmer returned an error; the module flips without it.
    WarmFailed { set: WarmSet, error: String },
    /// The budget ran out before the warmer finished.
    BudgetExhausted { set: WarmSet },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReadinessReport {
    pub(super) cause: FlipCause,
    pub(super) warmer: &'static str,
    pub(super) role: SpawnRole,
    /// From HELLO to the successful flip.
    pub(super) elapsed: Duration,
    pub(super) flip_attempts: u32,
}

impl fmt::Display for ReadinessReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "readiness: flipped ready after {} ms (",
            self.elapsed.as_millis()
        )?;
        let live = |f: &mut fmt::Formatter<'_>, set: &WarmSet| {
            write!(
                f,
                "live_roots: {} known, {} unknown-root, {} total",
                set.known_roots.len(),
                set.unknown_root_bindings,
                set.total_bindings
            )
        };
        match &self.cause {
            FlipCause::QueryFailed(error) => write!(f, "live_roots refused: {error}")?,
            FlipCause::NoBindings => f.write_str("live_roots: no bindings")?,
            FlipCause::WarmFinished {
                set,
                warmed,
                skipped,
                timed_out,
            } => {
                live(f, set)?;
                if !set.known_roots.is_empty() {
                    write!(
                        f,
                        "; warmed {warmed}, skipped {skipped}, timed out {timed_out}"
                    )?;
                }
            }
            FlipCause::WarmFailed { set, error } => {
                live(f, set)?;
                write!(f, "; warm-up failed: {error}")?;
            }
            FlipCause::BudgetExhausted { set } => {
                live(f, set)?;
                write!(f, "; warm-up budget exhausted")?;
            }
        }
        write!(
            f,
            "; warmer: {}; budget {} ms ({}); flip attempts {})",
            self.warmer,
            self.role.warm_budget().as_millis(),
            self.role.label(),
            self.flip_attempts
        )
    }
}

/// Run the whole readiness sequence and return once the flip has succeeded.
/// `hello_at` is when HELLO was sent; the budget runs from there.
pub(super) async fn run_readiness(
    control: &dyn ReadinessControl,
    warmer: &dyn RootWarmer,
    role: SpawnRole,
    hello_at: Instant,
) -> ReadinessReport {
    let deadline = hello_at + role.warm_budget();
    let cause = decide_flip_cause(control, warmer, deadline).await;
    let flip_attempts = flip_ready_until_accepted(control).await;
    ReadinessReport {
        cause,
        warmer: warmer.name(),
        role,
        elapsed: hello_at.elapsed(),
        flip_attempts,
    }
}

async fn decide_flip_cause(
    control: &dyn ReadinessControl,
    warmer: &dyn RootWarmer,
    deadline: Instant,
) -> FlipCause {
    let query_deadline = deadline.min(Instant::now() + LIVE_ROOTS_QUERY_TIMEOUT);
    let reply = match tokio::time::timeout_at(query_deadline, control.live_roots()).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => return FlipCause::QueryFailed(error),
        Err(_) => return FlipCause::QueryFailed(ControlError::Timeout),
    };
    let set = match classify_live_roots(&reply) {
        LiveSet::NoBindings => return FlipCause::NoBindings,
        LiveSet::KnownRoots(set) | LiveSet::WithUnknownRoots(set) => set,
    };
    if set.known_roots.is_empty() {
        // Bindings exist but none has a known root: nothing can be named, so
        // everything warms lazily. This is not the no-bindings case.
        return FlipCause::WarmFinished {
            set,
            warmed: 0,
            skipped: 0,
            timed_out: 0,
        };
    }
    match tokio::time::timeout_at(deadline, warmer.warm(&set, deadline)).await {
        Ok(Ok(outcomes)) => {
            let count = |wanted: fn(&RootWarmOutcome) -> bool| {
                outcomes
                    .iter()
                    .filter(|(_, outcome)| wanted(outcome))
                    .count()
            };
            FlipCause::WarmFinished {
                warmed: count(|o| matches!(o, RootWarmOutcome::Warmed)),
                skipped: count(|o| matches!(o, RootWarmOutcome::Skipped { .. })),
                timed_out: count(|o| matches!(o, RootWarmOutcome::TimedOut)),
                set,
            }
        }
        Ok(Err(error)) => FlipCause::WarmFailed { set, error },
        Err(_) => FlipCause::BudgetExhausted { set },
    }
}

/// Send `catalog.update { ready: true }` until the daemon accepts it. There is
/// no attempt limit: giving up would leave the module refusing every session.
/// The loop ends with the connection, when the module loop aborts this task.
async fn flip_ready_until_accepted(control: &dyn ReadinessControl) -> u32 {
    let mut attempts = 0_u32;
    let mut backoff = READY_FLIP_INITIAL_BACKOFF;
    loop {
        attempts = attempts.saturating_add(1);
        let error =
            match tokio::time::timeout(READY_FLIP_ATTEMPT_TIMEOUT, control.flip_ready()).await {
                Ok(Ok(())) => return attempts,
                Ok(Err(error)) => error,
                Err(_) => ControlError::Timeout,
            };
        log::warn!(
            "readiness: catalog.update ready=true failed (attempt {attempts}): {error}; retrying in {} ms",
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(READY_FLIP_MAX_BACKOFF);
    }
}

/// Module-originated channel-0 requests awaiting the daemon's reply, keyed by
/// correlation id. The module loop feeds every channel-0 Response and Error
/// frame through [`PendingControlReplies::resolve`].
#[derive(Clone, Default)]
pub(super) struct PendingControlReplies {
    inner: Arc<StdMutex<HashMap<u64, oneshot::Sender<Frame>>>>,
}

impl PendingControlReplies {
    fn register(&self, corr: u64) -> oneshot::Receiver<Frame> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(corr, tx);
        rx
    }

    fn remove(&self, corr: u64) {
        self.lock().remove(&corr);
    }

    /// Deliver a channel-0 reply. Returns false when no request is waiting
    /// for its correlation id (a late reply after a timeout, for example).
    pub(super) fn resolve(&self, frame: Frame) -> bool {
        let waiter = self.lock().remove(&frame.header.corr);
        match waiter {
            Some(tx) => tx.send(frame).is_ok(),
            None => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Frame>>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Drops the pending entry when a request is abandoned (timed out or
/// cancelled), so a late reply is recognised as unmatched.
struct PendingGuard<'a> {
    pending: &'a PendingControlReplies,
    corr: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending.remove(self.corr);
    }
}

/// [`ReadinessControl`] over the module's own daemon connection.
pub(super) struct DaemonControl {
    writer: WriterSender,
    metrics: Arc<DispatchPathMetrics>,
    negotiated_ver: u8,
    advertised_ops: Vec<String>,
    /// The provider roles from the HELLO manifest. `catalog.update` replaces
    /// the module's roles, so the flip must resend exactly these.
    provides: Vec<ProviderRole>,
    pending: PendingControlReplies,
    next_corr: AtomicU64,
}

impl DaemonControl {
    pub(super) fn new(
        writer: WriterSender,
        metrics: Arc<DispatchPathMetrics>,
        negotiated_ver: u8,
        advertised_ops: Vec<String>,
        provides: Vec<ProviderRole>,
        pending: PendingControlReplies,
        first_corr: u64,
    ) -> Self {
        Self {
            writer,
            metrics,
            negotiated_ver,
            advertised_ops,
            provides,
            pending,
            next_corr: AtomicU64::new(first_corr),
        }
    }

    fn advertises(&self, op: &str) -> bool {
        self.advertised_ops
            .iter()
            .any(|advertised| advertised == op)
    }

    async fn request(
        &self,
        body: &ModuleControlRequestFromModule,
    ) -> Result<ModuleControlResponseToModule, ControlError> {
        let body =
            serde_json::to_vec(body).map_err(|error| ControlError::Decode(error.to_string()))?;
        let corr = self.next_corr.fetch_add(1, Ordering::Relaxed);
        let rx = self.pending.register(corr);
        let _guard = PendingGuard {
            pending: &self.pending,
            corr,
        };
        let frame = Frame::build_with_version(
            self.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|error| ControlError::Decode(error.to_string()))?;
        send_reliable_writer_frame(&self.writer, &self.metrics, frame, "readiness request")
            .await
            .map_err(|_| ControlError::ConnectionClosed)?;
        let reply = rx.await.map_err(|_| ControlError::ConnectionClosed)?;
        decode_control_reply(&reply)
    }
}

fn decode_control_reply(frame: &Frame) -> Result<ModuleControlResponseToModule, ControlError> {
    match frame.header.ty {
        FrameType::Response => serde_json::from_slice(&frame.body)
            .map_err(|error| ControlError::Decode(error.to_string())),
        FrameType::Error => match serde_json::from_slice::<ErrorBody>(&frame.body) {
            Ok(body) => Err(ControlError::Refused {
                code: body.code,
                message: body.message,
            }),
            Err(error) => Err(ControlError::Decode(error.to_string())),
        },
        other => Err(ControlError::Decode(format!("unexpected {other:?} frame"))),
    }
}

impl ReadinessControl for DaemonControl {
    fn live_roots(&self) -> BoxFuture<'_, Result<LiveRootsReply, ControlError>> {
        Box::pin(async move {
            if !self.advertises(LIVE_ROOTS_OP) {
                return Err(ControlError::NotAdvertised(LIVE_ROOTS_OP));
            }
            match self
                .request(&ModuleControlRequestFromModule::LiveRoots {})
                .await?
            {
                ModuleControlResponseToModule::LiveRoots {
                    roots,
                    unknown_root_bindings,
                    total_bindings,
                } => Ok(LiveRootsReply {
                    roots,
                    unknown_root_bindings,
                    total_bindings,
                }),
                other => Err(ControlError::Decode(format!(
                    "expected a live_roots reply, got {other:?}"
                ))),
            }
        })
    }

    fn flip_ready(&self) -> BoxFuture<'_, Result<(), ControlError>> {
        Box::pin(async move {
            match self
                .request(&ModuleControlRequestFromModule::CatalogUpdate {
                    provides: self.provides.clone(),
                    capabilities: None,
                    ready: Some(true),
                })
                .await?
            {
                ModuleControlResponseToModule::CatalogUpdate {} => Ok(()),
                other => Err(ControlError::Decode(format!(
                    "expected a catalog.update reply, got {other:?}"
                ))),
            }
        })
    }
}

/// Whether the daemon can hold this module not-ready at all. A daemon that
/// does not advertise `catalog.update` predates readiness: it ignores the
/// manifest's `ready: false`, so there is nothing to flip and nothing to send.
pub(super) fn daemon_supports_ready_flip(advertised_ops: &[String]) -> bool {
    advertised_ops
        .iter()
        .any(|op| op == MODULE_TO_SUBC_OP_CATALOG_UPDATE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::mpsc;

    use super::super::wire::WriterFrame;

    enum QueryBehavior {
        Reply(LiveRootsReply),
        Fail(ControlError),
        Hang,
    }

    enum FlipBehavior {
        Fail(ControlError),
        Hang,
    }

    /// Daemon double. It fails the way the real connection can: typed
    /// refusals, transport errors and requests that never get an answer.
    struct FakeDaemon {
        query: QueryBehavior,
        /// Consumed in order; once empty every flip is accepted.
        flips: StdMutex<Vec<FlipBehavior>>,
        queries: AtomicUsize,
        flip_calls: AtomicUsize,
        accepted_flips: AtomicUsize,
    }

    impl FakeDaemon {
        fn new(query: QueryBehavior) -> Self {
            Self::with_flips(query, Vec::new())
        }

        fn with_flips(query: QueryBehavior, flips: Vec<FlipBehavior>) -> Self {
            let mut flips = flips;
            flips.reverse();
            Self {
                query,
                flips: StdMutex::new(flips),
                queries: AtomicUsize::new(0),
                flip_calls: AtomicUsize::new(0),
                accepted_flips: AtomicUsize::new(0),
            }
        }
    }

    impl ReadinessControl for FakeDaemon {
        fn live_roots(&self) -> BoxFuture<'_, Result<LiveRootsReply, ControlError>> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match &self.query {
                    QueryBehavior::Reply(reply) => Ok(reply.clone()),
                    QueryBehavior::Fail(error) => Err(error.clone()),
                    QueryBehavior::Hang => std::future::pending().await,
                }
            })
        }

        fn flip_ready(&self) -> BoxFuture<'_, Result<(), ControlError>> {
            self.flip_calls.fetch_add(1, Ordering::SeqCst);
            let next = self.flips.lock().unwrap().pop();
            Box::pin(async move {
                match next {
                    Some(FlipBehavior::Fail(error)) => Err(error),
                    Some(FlipBehavior::Hang) => std::future::pending().await,
                    None => {
                        self.accepted_flips.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                }
            })
        }
    }

    enum WarmBehavior {
        Skip,
        /// Reports these outcomes, one per known root in order.
        Report(Vec<RootWarmOutcome>),
        Hang,
        Fail(String),
    }

    struct FakeWarmer {
        behavior: WarmBehavior,
        calls: StdMutex<Vec<WarmSet>>,
    }

    impl FakeWarmer {
        fn new(behavior: WarmBehavior) -> Self {
            Self {
                behavior,
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<WarmSet> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RootWarmer for FakeWarmer {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn warm<'a>(
            &'a self,
            set: &'a WarmSet,
            _deadline: Instant,
        ) -> BoxFuture<'a, Result<Vec<(PathBuf, RootWarmOutcome)>, String>> {
            self.calls.lock().unwrap().push(set.clone());
            Box::pin(async move {
                match &self.behavior {
                    WarmBehavior::Skip => Ok(set
                        .known_roots
                        .iter()
                        .map(|root| {
                            (
                                root.clone(),
                                RootWarmOutcome::Skipped {
                                    reason: "test".to_string(),
                                },
                            )
                        })
                        .collect()),
                    WarmBehavior::Report(outcomes) => Ok(set
                        .known_roots
                        .iter()
                        .cloned()
                        .zip(outcomes.iter().cloned())
                        .collect()),
                    WarmBehavior::Hang => std::future::pending().await,
                    WarmBehavior::Fail(error) => Err(error.clone()),
                }
            })
        }
    }

    fn live_root(path: &str, bound: u64, pending: u64) -> LiveRoot {
        LiveRoot {
            project_root: PathBuf::from(path),
            bound,
            pending,
        }
    }

    fn reply(roots: Vec<LiveRoot>, unknown: u64, total: u64) -> LiveRootsReply {
        LiveRootsReply {
            roots,
            unknown_root_bindings: unknown,
            total_bindings: total,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn any_live_roots_query_error_flips_ready_immediately() {
        let errors = [
            ControlError::NotAdvertised(LIVE_ROOTS_OP),
            ControlError::Refused {
                code: "unknown_op".to_string(),
                message: "unknown module op supervisor.live_roots".to_string(),
            },
            ControlError::ConnectionClosed,
            ControlError::Decode("bad json".to_string()),
            ControlError::Timeout,
        ];
        for error in errors {
            let daemon = FakeDaemon::new(QueryBehavior::Fail(error.clone()));
            let warmer = FakeWarmer::new(WarmBehavior::Hang);
            let report =
                run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
            assert_eq!(report.cause, FlipCause::QueryFailed(error.clone()));
            assert_eq!(report.elapsed, Duration::ZERO, "{error} must flip at once");
            assert_eq!(daemon.accepted_flips.load(Ordering::SeqCst), 1);
            assert!(warmer.calls().is_empty(), "{error} must not warm");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_live_roots_query_flips_after_the_query_timeout_not_the_budget() {
        let daemon = FakeDaemon::new(QueryBehavior::Hang);
        let warmer = FakeWarmer::new(WarmBehavior::Skip);
        let report =
            run_readiness(&daemon, &warmer, SpawnRole::SwapCandidate, Instant::now()).await;
        assert_eq!(report.cause, FlipCause::QueryFailed(ControlError::Timeout));
        assert_eq!(report.elapsed, LIVE_ROOTS_QUERY_TIMEOUT);
        assert!(warmer.calls().is_empty());
        assert_eq!(daemon.accepted_flips.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_total_bindings_flips_without_warming() {
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(Vec::new(), 0, 0)));
        let warmer = FakeWarmer::new(WarmBehavior::Hang);
        let report = run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
        assert_eq!(report.cause, FlipCause::NoBindings);
        assert_eq!(report.elapsed, Duration::ZERO);
        assert!(warmer.calls().is_empty());
        assert_eq!(daemon.accepted_flips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn classification_keeps_the_three_cases_distinct() {
        assert_eq!(
            classify_live_roots(&reply(Vec::new(), 0, 0)),
            LiveSet::NoBindings
        );
        let known = reply(vec![live_root("/a", 2, 0), live_root("/b", 0, 1)], 0, 3);
        assert_eq!(
            classify_live_roots(&known),
            LiveSet::KnownRoots(WarmSet {
                known_roots: vec![PathBuf::from("/a"), PathBuf::from("/b")],
                unknown_root_bindings: 0,
                total_bindings: 3,
            })
        );
        // Only unknown-root bindings: roots exist that cannot be named. This
        // is not the no-bindings case.
        assert_eq!(
            classify_live_roots(&reply(Vec::new(), 4, 4)),
            LiveSet::WithUnknownRoots(WarmSet {
                known_roots: Vec::new(),
                unknown_root_bindings: 4,
                total_bindings: 4,
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_root_bindings_warm_the_known_roots_and_are_not_read_as_empty() {
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(
            vec![live_root("/known", 1, 0)],
            3,
            4,
        )));
        let warmer = FakeWarmer::new(WarmBehavior::Skip);
        let report = run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
        let expected_set = WarmSet {
            known_roots: vec![PathBuf::from("/known")],
            unknown_root_bindings: 3,
            total_bindings: 4,
        };
        assert_eq!(warmer.calls(), vec![expected_set.clone()]);
        assert_eq!(
            report.cause,
            FlipCause::WarmFinished {
                set: expected_set,
                warmed: 0,
                skipped: 1,
                timed_out: 0,
            }
        );

        // With no known root at all the warmer has nothing to load, but the
        // cause still records the unknown bindings rather than "no bindings".
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(Vec::new(), 2, 2)));
        let warmer = FakeWarmer::new(WarmBehavior::Hang);
        let report = run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
        assert_ne!(report.cause, FlipCause::NoBindings);
        assert!(matches!(
            report.cause,
            FlipCause::WarmFinished { ref set, .. } if set.unknown_root_bindings == 2
        ));
        assert!(report
            .to_string()
            .contains("0 known, 2 unknown-root, 2 total"));
    }

    #[tokio::test(start_paused = true)]
    async fn finished_warm_up_counts_each_root_outcome() {
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(
            vec![
                live_root("/a", 1, 0),
                live_root("/b", 1, 0),
                live_root("/c", 0, 1),
            ],
            0,
            3,
        )));
        let warmer = FakeWarmer::new(WarmBehavior::Report(vec![
            RootWarmOutcome::Warmed,
            RootWarmOutcome::Skipped {
                reason: "no persisted artifacts".to_string(),
            },
            RootWarmOutcome::TimedOut,
        ]));
        let report = run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
        assert!(matches!(
            report.cause,
            FlipCause::WarmFinished {
                warmed: 1,
                skipped: 1,
                timed_out: 1,
                ..
            }
        ));
        assert!(report.to_string().contains(
            "3 known, 0 unknown-root, 3 total; warmed 1, skipped 1, timed out 1; warmer: fake"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn budget_flips_ready_while_the_warmer_hangs() {
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(
            vec![live_root("/slow", 1, 0)],
            0,
            1,
        )));
        let warmer = FakeWarmer::new(WarmBehavior::Hang);
        let report = run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
        assert!(matches!(report.cause, FlipCause::BudgetExhausted { .. }));
        assert_eq!(report.elapsed, PLAIN_START_WARM_BUDGET);
        assert_eq!(daemon.accepted_flips.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn warmer_error_flips_ready_without_waiting_for_the_budget() {
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(
            vec![live_root("/a", 1, 0)],
            0,
            1,
        )));
        let warmer = FakeWarmer::new(WarmBehavior::Fail("disk gone".to_string()));
        let report = run_readiness(&daemon, &warmer, SpawnRole::PlainStart, Instant::now()).await;
        assert!(
            matches!(report.cause, FlipCause::WarmFailed { ref error, .. } if error == "disk gone")
        );
        assert_eq!(report.elapsed, Duration::ZERO);
    }

    #[test]
    fn swap_budget_applies_only_to_the_exact_swap_candidate_role() {
        assert_eq!(
            SpawnRole::from_env_value(Some("swap_candidate")).warm_budget(),
            SWAP_CANDIDATE_WARM_BUDGET
        );
        for other in [
            None,
            Some(""),
            Some("SWAP_CANDIDATE"),
            Some("swap"),
            Some("incumbent"),
        ] {
            assert_eq!(
                SpawnRole::from_env_value(other).warm_budget(),
                PLAIN_START_WARM_BUDGET,
                "{other:?} must select the plain-start budget"
            );
        }
        assert_eq!(PLAIN_START_WARM_BUDGET, Duration::from_secs(10));
        assert_eq!(SWAP_CANDIDATE_WARM_BUDGET, Duration::from_secs(90));
    }

    #[test]
    fn spawn_role_is_read_from_the_supervisor_variable() {
        let _env = crate::test_env::process_env_lock();
        let previous = std::env::var_os(SPAWN_ROLE_ENV);
        std::env::remove_var(SPAWN_ROLE_ENV);
        assert_eq!(SpawnRole::from_process_env(), SpawnRole::PlainStart);
        std::env::set_var(SPAWN_ROLE_ENV, "swap_candidate");
        assert_eq!(SpawnRole::from_process_env(), SpawnRole::SwapCandidate);
        std::env::set_var(SPAWN_ROLE_ENV, "plain");
        assert_eq!(SpawnRole::from_process_env(), SpawnRole::PlainStart);
        match previous {
            Some(value) => std::env::set_var(SPAWN_ROLE_ENV, value),
            None => std::env::remove_var(SPAWN_ROLE_ENV),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn swap_candidate_waits_the_swap_budget_for_a_hanging_warmer() {
        let daemon = FakeDaemon::new(QueryBehavior::Reply(reply(
            vec![live_root("/slow", 1, 0)],
            0,
            1,
        )));
        let warmer = FakeWarmer::new(WarmBehavior::Hang);
        let report =
            run_readiness(&daemon, &warmer, SpawnRole::SwapCandidate, Instant::now()).await;
        assert!(matches!(report.cause, FlipCause::BudgetExhausted { .. }));
        assert_eq!(report.elapsed, SWAP_CANDIDATE_WARM_BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn production_warmer_skips_roots_without_loading_or_building() {
        let root = tempfile::tempdir().expect("tempdir");
        let set = WarmSet {
            known_roots: vec![root.path().to_path_buf()],
            unknown_root_bindings: 0,
            total_bindings: 1,
        };
        let outcomes = NoWarmer
            .warm(&set, Instant::now() + PLAIN_START_WARM_BUDGET)
            .await
            .expect("warm");
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, RootWarmOutcome::Skipped { .. }));
        // A root with no persisted artifacts stays exactly as it was: no
        // cache directory, no index, nothing written by a build.
        assert_eq!(
            std::fs::read_dir(root.path()).expect("read root").count(),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_ready_flip_is_retried_until_accepted() {
        let daemon = FakeDaemon::with_flips(
            QueryBehavior::Fail(ControlError::NotAdvertised(LIVE_ROOTS_OP)),
            vec![
                FlipBehavior::Fail(ControlError::Refused {
                    code: "busy".to_string(),
                    message: "try again".to_string(),
                }),
                FlipBehavior::Hang,
                FlipBehavior::Fail(ControlError::ConnectionClosed),
                FlipBehavior::Fail(ControlError::Timeout),
                FlipBehavior::Fail(ControlError::Decode("x".to_string())),
                FlipBehavior::Fail(ControlError::Timeout),
                FlipBehavior::Fail(ControlError::Timeout),
                FlipBehavior::Fail(ControlError::Timeout),
            ],
        );
        let report = run_readiness(
            &daemon,
            &FakeWarmer::new(WarmBehavior::Skip),
            SpawnRole::PlainStart,
            Instant::now(),
        )
        .await;
        assert_eq!(report.flip_attempts, 9);
        assert_eq!(daemon.flip_calls.load(Ordering::SeqCst), 9);
        assert_eq!(daemon.accepted_flips.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn report_line_says_what_happened_without_claiming_warm_up() {
        let report = ReadinessReport {
            cause: FlipCause::QueryFailed(ControlError::NotAdvertised(LIVE_ROOTS_OP)),
            warmer: NoWarmer.name(),
            role: SpawnRole::PlainStart,
            elapsed: Duration::from_millis(12),
            flip_attempts: 1,
        };
        assert_eq!(
            report.to_string(),
            "readiness: flipped ready after 12 ms (live_roots refused: daemon does not advertise supervisor.live_roots; warmer: none; budget 10000 ms (plain start); flip attempts 1)"
        );
    }

    fn wire_control(
        advertised: &[&str],
    ) -> (
        DaemonControl,
        mpsc::Receiver<WriterFrame>,
        PendingControlReplies,
    ) {
        let (tx, rx) = mpsc::channel(8);
        let pending = PendingControlReplies::default();
        let control = DaemonControl::new(
            tx,
            Arc::new(DispatchPathMetrics::new()),
            subc_protocol::PROTOCOL_VERSION,
            advertised.iter().map(|op| (*op).to_string()).collect(),
            super::super::manifest::build_manifest().provides,
            pending.clone(),
            2,
        );
        (control, rx, pending)
    }

    fn reply_frame(ty: FrameType, corr: u64, body: Vec<u8>) -> Frame {
        Frame::build(ty, control_flags(), 0, 0, corr, body).expect("reply frame")
    }

    #[tokio::test]
    async fn live_roots_query_is_not_sent_when_the_daemon_does_not_advertise_it() {
        let (control, mut rx, _pending) = wire_control(&[MODULE_TO_SUBC_OP_CATALOG_UPDATE]);
        assert_eq!(
            control.live_roots().await,
            Err(ControlError::NotAdvertised(LIVE_ROOTS_OP))
        );
        assert!(rx.try_recv().is_err(), "no request frame may be sent");
    }

    #[tokio::test]
    async fn live_roots_round_trips_over_channel_zero() {
        let (control, mut rx, pending) = wire_control(&[LIVE_ROOTS_OP]);
        let daemon = async {
            let request = rx.recv().await.expect("request frame");
            assert_eq!(request.header.ty, FrameType::Request);
            assert_eq!(request.header.channel, 0);
            let body: ModuleControlRequestFromModule =
                serde_json::from_slice(&request.body).expect("request body");
            assert_eq!(body, ModuleControlRequestFromModule::LiveRoots {});
            let answer = ModuleControlResponseToModule::LiveRoots {
                roots: vec![live_root("/r", 1, 1)],
                unknown_root_bindings: 1,
                total_bindings: 3,
            };
            assert!(pending.resolve(reply_frame(
                FrameType::Response,
                request.header.corr,
                serde_json::to_vec(&answer).unwrap(),
            )));
        };
        let (result, ()) = tokio::join!(control.live_roots(), daemon);
        assert_eq!(result, Ok(reply(vec![live_root("/r", 1, 1)], 1, 3)));
    }

    #[tokio::test]
    async fn refused_live_roots_query_surfaces_the_daemon_error() {
        let (control, mut rx, pending) = wire_control(&[LIVE_ROOTS_OP]);
        let daemon = async {
            let request = rx.recv().await.expect("request frame");
            let error = ErrorBody::new("unknown_op", "unknown module op");
            assert!(pending.resolve(reply_frame(
                FrameType::Error,
                request.header.corr,
                serde_json::to_vec(&error).unwrap(),
            )));
        };
        let (result, ()) = tokio::join!(control.live_roots(), daemon);
        assert_eq!(
            result,
            Err(ControlError::Refused {
                code: "unknown_op".to_string(),
                message: "unknown module op".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn ready_flip_resends_the_manifest_roles_with_ready_true() {
        let (control, mut rx, pending) = wire_control(&[MODULE_TO_SUBC_OP_CATALOG_UPDATE]);
        let daemon = async {
            let request = rx.recv().await.expect("request frame");
            assert_eq!(request.header.channel, 0);
            let body: ModuleControlRequestFromModule =
                serde_json::from_slice(&request.body).expect("request body");
            assert_eq!(
                body,
                ModuleControlRequestFromModule::CatalogUpdate {
                    provides: super::super::manifest::build_manifest().provides,
                    capabilities: None,
                    ready: Some(true),
                }
            );
            assert!(pending.resolve(reply_frame(
                FrameType::Response,
                request.header.corr,
                serde_json::to_vec(&ModuleControlResponseToModule::CatalogUpdate {}).unwrap(),
            )));
        };
        let (result, ()) = tokio::join!(control.flip_ready(), daemon);
        assert_eq!(result, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_request_leaves_no_pending_entry_for_a_late_reply() {
        let (control, mut rx, pending) = wire_control(&[LIVE_ROOTS_OP]);
        let timed_out = tokio::time::timeout(Duration::from_secs(1), control.live_roots()).await;
        assert!(timed_out.is_err());
        let request = rx.recv().await.expect("request frame");
        assert!(!pending.resolve(reply_frame(
            FrameType::Response,
            request.header.corr,
            b"{}".to_vec(),
        )));
    }

    #[test]
    fn ready_flip_is_sent_only_to_daemons_that_advertise_catalog_update() {
        assert!(daemon_supports_ready_flip(&[
            MODULE_TO_SUBC_OP_CATALOG_UPDATE.to_string()
        ]));
        assert!(!daemon_supports_ready_flip(&[]));
        assert!(!daemon_supports_ready_flip(&[LIVE_ROOTS_OP.to_string()]));
    }
}
