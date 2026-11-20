//! The per-root `aft.db` open that a route bind defers past its reply.
//!
//! A bind acknowledges before the root's database is open, so read-only tools
//! work at once while persistence-dependent tools (bash, edits, undo, state)
//! wait a bounded time for the open. The open used to run as a stage of the
//! root's configure tail, and configure tails are admitted a few at a time
//! across every root and share the executor's maintenance workers with index
//! work. After a daemon restart dozens of roots bind together, every tail
//! queues, and a root's database waited for other roots' view loads, storage
//! sweeps and callgraph warms: about two minutes in one captured restart,
//! while mutating tools were refused as "still initializing".
//!
//! The open now runs on a dedicated thread, dispatched right after the bind
//! acknowledgement is sent. It never enters the executor, so no index build,
//! tier-2 refresh or other root's maintenance can hold it back. The configure
//! tail still reaches its database stage, and there it runs the open itself if
//! nothing dispatched it (standalone mode, tests) or waits for the dedicated
//! thread to finish, so session replay always sees the selected database.
//!
//! Each finished open logs one line per root with the time from the bind's
//! configure commit to database-ready and where that time went.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::context::AppContext;

/// A root whose database takes at least this long to become ready after its
/// bind gets its delay reason named in the readiness log line.
const SLOW_DATABASE_READY: Duration = Duration::from_secs(1);
/// A persistence-dependent tool waits at most this long for the open before it
/// is refused (the daemon-mode readiness wait in `subc::persistence`). A root
/// slower than this refused tools, so its readiness line is a warning.
const TOOL_WAIT_BUDGET: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_configure_registration_refuses_staged_open() {
        const TEST: &str =
            "database_open::tests::missing_configure_registration_refuses_staged_open";
        const CHILD: &str = "AFT_TEST_UNREGISTERED_DATABASE_OPEN";
        if std::env::var(CHILD).as_deref() != Ok(TEST) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, TEST)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed;"));
            return;
        }
        assert!(crate::runtime_drain::configure::command_hooks().is_none());
        let dir = tempfile::tempdir().unwrap();
        let ctx = AppContext::from_app(
            crate::context::App::default_shared(),
            crate::config::Config::default(),
        );
        {
            let mut slot = ctx.database_open_slot().0.lock();
            slot.epoch = 1;
            slot.pending = Some(DatabaseOpenRequest {
                epoch: 1,
                canonical_cache_root: dir.path().into(),
                storage_root: dir.path().into(),
                session_id: "session".into(),
                committed_at: Instant::now(),
                dispatched_at: None,
            });
        }
        run_staged_open(&ctx, DatabaseOpenRunner::ConfigureTail, true);
        let response = ctx
            .database_runtime_refusal("test", "db_set_host_state")
            .unwrap();
        assert!(!response.success);
        assert!(serde_json::to_string(&response)
            .unwrap()
            .contains("register_runtime_hooks"));
        assert!(!dir.path().join("aft.db").exists());
        let slot = ctx.database_open_slot().0.lock();
        assert!(!slot.running);
        assert!(slot.ready_for.is_none());
    }
}

/// The open a committed configure asked for, waiting for a runner.
#[derive(Debug, Clone)]
pub(crate) struct DatabaseOpenRequest {
    /// The configure commit this open belongs to. Every configure commit bumps
    /// the root's epoch; an open that finishes under an older epoch publishes
    /// nothing, so it cannot mark the root ready (or failed) after a newer
    /// configure asked for a different open.
    pub(crate) epoch: u64,
    pub(crate) canonical_cache_root: PathBuf,
    pub(crate) storage_root: PathBuf,
    pub(crate) session_id: String,
    /// When the configure committed and marked persistence as initializing.
    pub(crate) committed_at: Instant,
    /// When the bind acknowledgement was sent and the open was handed to the
    /// dedicated thread; `None` when only the configure tail runs it.
    pub(crate) dispatched_at: Option<Instant>,
}

/// Per-root open state, guarded by one mutex on the root's context.
#[derive(Debug)]
pub(crate) struct DatabaseOpenSlot {
    /// Bumped by every configure commit. Publication compares against it
    /// under this mutex, so an older open can never mark a newer configure's
    /// database ready.
    pub(crate) epoch: u64,
    pub(crate) committed_at: Instant,
    pub(crate) pending: Option<DatabaseOpenRequest>,
    /// True while some thread is running an open for this root. Only one open
    /// per root runs at a time; others wait on the context's condvar.
    pub(crate) running: bool,
    /// Root and storage directory of the last open that published ready.
    /// A rebind with the same pair keeps the root ready.
    pub(crate) ready_for: Option<(PathBuf, PathBuf)>,
}

impl Default for DatabaseOpenSlot {
    fn default() -> Self {
        Self {
            epoch: 0,
            committed_at: Instant::now(),
            pending: None,
            running: false,
            ready_for: None,
        }
    }
}

/// Where a staged open ran; named in the readiness log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatabaseOpenRunner {
    /// The dedicated database-open thread, dispatched after the bind ack.
    Lane,
    /// The root's configure tail, at its database stage.
    ConfigureTail,
}

impl DatabaseOpenRunner {
    fn label(self) -> &'static str {
        match self {
            Self::Lane => "database-open thread",
            Self::ConfigureTail => "configure tail",
        }
    }
}

/// How one open attempt ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DatabaseOpenOutcome {
    Ready,
    /// The storage root's reader floor, or a schema version newer than this
    /// build reads, refused the database before any read-write connection.
    Refused(String),
    Failed(String),
    /// A newer configure committed while this open ran; nothing was published.
    Superseded,
}

/// Timings of one open attempt, measured inside the open itself.
#[derive(Clone, Debug)]
pub(crate) struct DatabaseOpenReport {
    /// Reader-floor check plus the read-only schema peek.
    pub(crate) floor_check: Duration,
    /// Taking the process-shared database slot and opening SQLite (PRAGMAs,
    /// lock-contention retries and migrations included).
    pub(crate) open: Duration,
    /// The process already held a handle for this database file.
    pub(crate) reused_handle: bool,
    pub(crate) outcome: DatabaseOpenOutcome,
}

/// Hand the root's staged open to the dedicated thread. Called once the bind
/// acknowledgement has been sent, so the open never delays the bind reply, and
/// again by any persistence-dependent tool that finds the root initializing,
/// so a staged open can never sit unscheduled. Does nothing when no open is
/// staged (an unchanged rebind, or the open is already running).
pub(crate) fn dispatch(ctx: &Arc<AppContext>) {
    if !ctx.mark_database_open_dispatched() {
        return;
    }
    match lane_sender() {
        Some(sender) => {
            if sender.send(Arc::downgrade(ctx)).is_err() {
                log::warn!(
                    "database-open thread is gone; the configure tail will open aft.db instead"
                );
            }
        }
        None => log::warn!(
            "database-open thread could not start; the configure tail will open aft.db instead"
        ),
    }
}

/// Run the root's staged open if nobody is running one. With
/// `wait_for_in_flight`, also wait until no open is running or staged, so the
/// caller can rely on the outcome being published (the configure tail needs
/// that before session replay). Without it, return as soon as another thread
/// is running this root's open: that runner picks up anything staged after it
/// started before it stops.
pub(crate) fn run_staged_open(
    ctx: &AppContext,
    runner: DatabaseOpenRunner,
    wait_for_in_flight: bool,
) {
    let (slot_mutex, idle) = ctx.database_open_slot();
    let mut slot = slot_mutex.lock();
    loop {
        if slot.running {
            if !wait_for_in_flight {
                return;
            }
            idle.wait(&mut slot);
            continue;
        }
        let Some(request) = slot.pending.take() else {
            return;
        };
        slot.running = true;
        drop(slot);

        // Clears `running` and wakes waiters even if the open panics, so a
        // configure tail waiting for this root's database is never stranded.
        let running = RunningGuard { ctx };
        let started_at = Instant::now();
        #[cfg(test)]
        let recorded = record_open_started_for_test(&request, runner, started_at);
        crate::log_ctx::with_session(Some(request.session_id.clone()), || {
            let open = crate::runtime_drain::configure::command_hooks()
                .and_then(|hooks| hooks.database_open);
            let report = match open {
                Some(open) => open(
                    ctx,
                    &request.canonical_cache_root,
                    &request.storage_root,
                    crate::db::OpenMode::Deferred,
                    request.epoch,
                ),
                None => {
                    let error = crate::runtime_drain::configure::MISSING_CONFIGURE_REGISTRATION;
                    let published = ctx.publish_database_open(request.epoch, |slot| {
                        slot.ready_for = None;
                        ctx.backup().lock().clear_db_pool();
                        ctx.bash_background().clear_db_pool();
                        ctx.finish_database_runtime_error(error.into(), false);
                    });
                    DatabaseOpenReport {
                        floor_check: Duration::ZERO,
                        open: Duration::ZERO,
                        reused_handle: false,
                        outcome: if published {
                            DatabaseOpenOutcome::Failed(error.into())
                        } else {
                            DatabaseOpenOutcome::Superseded
                        },
                    }
                }
            };
            let finished_at = Instant::now();
            log_database_ready(&request, runner, started_at, finished_at, &report);
            #[cfg(test)]
            record_open_finished_for_test(recorded, finished_at, &report);
        });
        drop(running);
        slot = slot_mutex.lock();
    }
}

struct RunningGuard<'a> {
    ctx: &'a AppContext,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        let (slot_mutex, idle) = self.ctx.database_open_slot();
        slot_mutex.lock().running = false;
        idle.notify_all();
    }
}

/// One process-wide thread runs every root's dispatched open, in dispatch
/// order. Opens of the same database file serialize on the process-shared
/// database slot anyway, and after the first one they reuse its handle, so one
/// thread keeps up with a restart's worth of binds.
fn lane_sender() -> Option<&'static crossbeam_channel::Sender<Weak<AppContext>>> {
    static LANE: OnceLock<Option<crossbeam_channel::Sender<Weak<AppContext>>>> = OnceLock::new();
    LANE.get_or_init(|| {
        let (sender, receiver) = crossbeam_channel::unbounded::<Weak<AppContext>>();
        std::thread::Builder::new()
            .name("aft-db-open".to_string())
            .spawn(move || {
                for ctx in receiver {
                    let Some(ctx) = ctx.upgrade() else {
                        continue;
                    };
                    // A root unbound after its bind is left alone: its
                    // configure tail drops unbound work, and the next bind
                    // (or tool call) dispatches the open again.
                    if ctx.subc_unbound_quiesced() {
                        continue;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_staged_open(&ctx, DatabaseOpenRunner::Lane, false);
                    }));
                    if result.is_err() {
                        log::error!(
                            "aft.db open panicked on the database-open thread; tools for this root \
                             stay refused until the next bind"
                        );
                    }
                }
            })
            .ok()
            .map(|_| sender)
    })
    .as_ref()
}

/// One open attempt with the timings its readiness log line reports, kept so
/// a test can see where a root's wait went (queued behind other opens, or the
/// open itself) instead of only how long the wait was. `finished_at` is `None`
/// while the open is still running.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct RecordedOpen {
    pub(crate) root: PathBuf,
    pub(crate) runner: DatabaseOpenRunner,
    pub(crate) dispatched_at: Option<Instant>,
    pub(crate) started_at: Instant,
    pub(crate) finished_at: Option<Instant>,
    pub(crate) floor_check: Duration,
    pub(crate) open: Duration,
    pub(crate) outcome: Option<DatabaseOpenOutcome>,
}

/// Every open this process has started. Under libtest all tests share one
/// process and one database-open thread, so a test reading this sees other
/// tests' opens too; that is intended, because they occupy the same thread.
#[cfg(test)]
static RECORDED_OPENS: std::sync::Mutex<Vec<RecordedOpen>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn record_open_started_for_test(
    request: &DatabaseOpenRequest,
    runner: DatabaseOpenRunner,
    started_at: Instant,
) -> usize {
    let mut opens = RECORDED_OPENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    opens.push(RecordedOpen {
        root: request.canonical_cache_root.clone(),
        runner,
        dispatched_at: request.dispatched_at,
        started_at,
        finished_at: None,
        floor_check: Duration::ZERO,
        open: Duration::ZERO,
        outcome: None,
    });
    opens.len() - 1
}

#[cfg(test)]
fn record_open_finished_for_test(index: usize, finished_at: Instant, report: &DatabaseOpenReport) {
    let mut opens = RECORDED_OPENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let open = &mut opens[index];
    open.finished_at = Some(finished_at);
    open.floor_check = report.floor_check;
    open.open = report.open;
    open.outcome = Some(report.outcome.clone());
}

/// Every open this process has started so far (finished or still running),
/// oldest first.
#[cfg(test)]
pub(crate) fn recorded_opens_for_test() -> Vec<RecordedOpen> {
    RECORDED_OPENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn millis(duration: Duration) -> u128 {
    duration.as_millis()
}

/// One line per open: total time from the configure commit to the outcome,
/// each phase's duration, and for a slow open the longest phase named.
fn log_database_ready(
    request: &DatabaseOpenRequest,
    runner: DatabaseOpenRunner,
    started_at: Instant,
    finished_at: Instant,
    report: &DatabaseOpenReport,
) {
    let total = finished_at.saturating_duration_since(request.committed_at);
    let phases = DelayPhases::measure(request, started_at);
    let handle = if report.reused_handle {
        "reused"
    } else {
        "opened"
    };
    let detail = format!(
        "{}ms after route bind (bind reply {}ms, queued {}ms, floor check {}ms, open {}ms; \
         handle {handle}; via {})",
        millis(total),
        millis(phases.bind_reply),
        millis(phases.queued),
        millis(report.floor_check),
        millis(report.open),
        runner.label(),
    );
    let reason = if total >= SLOW_DATABASE_READY {
        phases.slowest_reason(request, runner, report)
    } else {
        "none"
    };
    let root = request.canonical_cache_root.display();
    match &report.outcome {
        DatabaseOpenOutcome::Ready if total >= TOOL_WAIT_BUDGET => {
            crate::slog_warn!("aft.db ready for {root}: {detail}; delay: {reason}")
        }
        DatabaseOpenOutcome::Ready => {
            crate::slog_info!("aft.db ready for {root}: {detail}; delay: {reason}")
        }
        DatabaseOpenOutcome::Refused(error) | DatabaseOpenOutcome::Failed(error) => {
            crate::slog_warn!("aft.db not ready for {root}: {error}; {detail}; delay: {reason}")
        }
        DatabaseOpenOutcome::Superseded => {
            crate::slog_info!("aft.db open for {root} superseded by a newer configure; {detail}")
        }
    }
}

struct DelayPhases {
    /// Configure commit to bind acknowledgement (dispatch). Zero when only the
    /// configure tail ran the open.
    bind_reply: Duration,
    /// Dispatch (or commit, without a dispatch) to the start of the open.
    queued: Duration,
}

impl DelayPhases {
    fn measure(request: &DatabaseOpenRequest, started_at: Instant) -> Self {
        match request.dispatched_at {
            Some(dispatched_at) => Self {
                bind_reply: dispatched_at.saturating_duration_since(request.committed_at),
                queued: started_at.saturating_duration_since(dispatched_at),
            },
            None => Self {
                bind_reply: Duration::ZERO,
                queued: started_at.saturating_duration_since(request.committed_at),
            },
        }
    }

    fn slowest_reason(
        &self,
        request: &DatabaseOpenRequest,
        runner: DatabaseOpenRunner,
        report: &DatabaseOpenReport,
    ) -> &'static str {
        let candidates = [
            (
                self.bind_reply,
                "the route bind reply was still pending (configure finishing its prefix)",
            ),
            (
                self.queued,
                match (request.dispatched_at.is_some(), runner) {
                    (false, _) => {
                        "waited for this root's configure tail to reach its database stage"
                    }
                    (true, DatabaseOpenRunner::Lane) => {
                        "queued on the database-open thread behind other roots' opens"
                    }
                    (true, DatabaseOpenRunner::ConfigureTail) => {
                        "the database-open thread had not reached this root before its \
                         configure tail did"
                    }
                },
            ),
            (report.floor_check, "reader-floor check and schema peek"),
            (
                report.open,
                "SQLite open (shared database slot held by another open, lock-contention \
                 retries, or migrations)",
            ),
        ];
        candidates
            .into_iter()
            .max_by_key(|(duration, _)| *duration)
            .map_or("none", |(_, reason)| reason)
    }
}
