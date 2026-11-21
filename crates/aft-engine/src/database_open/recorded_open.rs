use super::*;

/// One open attempt with the timings its readiness log line reports, kept so
/// a test can see where a root's wait went (queued behind other opens, or the
/// open itself) instead of only how long the wait was. `finished_at` is `None`
/// while the open is still running.
#[derive(Clone, Debug)]
pub struct RecordedOpen {
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
static RECORDED_OPENS: std::sync::Mutex<Vec<RecordedOpen>> = std::sync::Mutex::new(Vec::new());

pub(super) fn record_open_started_for_test(
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

pub(super) fn record_open_finished_for_test(
    index: usize,
    finished_at: Instant,
    report: &DatabaseOpenReport,
) {
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
pub fn recorded_opens_for_test() -> Vec<RecordedOpen> {
    RECORDED_OPENS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
