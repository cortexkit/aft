fn completed_scoped_waiter(
    manager: &InspectManager,
    snapshot: &InspectSnapshot,
    cache: &InspectCache,
    category: InspectCategory,
    scope: &JobScope,
    payload: Value,
) -> Value {
    // Model completion fanout after a shared project scan has finished. Keeping
    // the channel ready makes the work assertion independent of thread timing.
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(JobOutcome::Fresh { payload }).unwrap();
    match manager.wait_for_tier2_reuse(
        &JobKey::for_project_category(category),
        scope,
        cache,
        rx,
        snapshot,
    ) {
        JobOutcome::Fresh { payload } => payload,
        other => panic!("expected fresh scoped completion, got {other:?}"),
    }
}

thread_local! {
    static CALLGRAPH_READ_RETRY_OBSERVER: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static CALLGRAPH_READ_RETRY_CLOCK: std::cell::Cell<Option<Instant>> = const {
        std::cell::Cell::new(None)
    };
}

pub(super) fn callgraph_read_retry_now_for_test() -> Option<Instant> {
    CALLGRAPH_READ_RETRY_CLOCK.with(|clock| {
        let now = clock.get()?;
        // Advance on every deadline check so an unreleased lock still exhausts
        // the retry budget, without charging filesystem or scheduling delays.
        clock.set(Some(now + Duration::from_millis(5)));
        Some(now)
    })
}

pub(super) fn notify_callgraph_read_retry_for_test() {
    let observer = CALLGRAPH_READ_RETRY_OBSERVER.with(|slot| slot.borrow_mut().take());
    if let Some(observer) = observer {
        observer();
    }
}

pub(super) fn observe_callgraph_read_error_for_test(
    error: &CallGraphStoreError,
    attempt_elapsed: Duration,
    retry_remaining: Duration,
) {
    CALLGRAPH_READ_RETRY_OBSERVER.with(|slot| {
        if slot.borrow().is_some() {
            eprintln!(
                "locked callgraph read: attempt={attempt_elapsed:?}, wall_retry_remaining={retry_remaining:?}, error={error:?}"
            );
        }
    });
}

struct CallgraphReadRetryTestGuard;

impl CallgraphReadRetryTestGuard {
    fn install() -> Self {
        CALLGRAPH_READ_RETRY_CLOCK.with(|clock| clock.set(Some(Instant::now())));
        Self
    }
}

impl Drop for CallgraphReadRetryTestGuard {
    fn drop(&mut self) {
        CALLGRAPH_READ_RETRY_OBSERVER.with(|slot| slot.borrow_mut().take());
        CALLGRAPH_READ_RETRY_CLOCK.with(|clock| clock.set(None));
    }
}

#[test]
fn dead_code_store_read_does_not_retry_an_expired_lock() {
    let _retry = CallgraphReadRetryTestGuard::install();
    let start = CALLGRAPH_READ_RETRY_CLOCK.with(|clock| clock.get().unwrap());
    let mut attempts = 0;
    let result = retry_dead_code_store_read(|| {
        attempts += 1;
        // A lock that consumes the whole budget stays pending, even with the
        // controlled clock used by the transient-lock fixtures.
        CALLGRAPH_READ_RETRY_CLOCK
            .with(|clock| clock.set(Some(start + Duration::from_millis(100))));
        Err::<(), _>(CallGraphStoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )))
    });
    assert!(result.unwrap_err().is_transient_lock_contention());
    assert_eq!(
        attempts, 1,
        "an expired read must not start another attempt"
    );
}

#[test]
fn dead_code_store_read_exhausts_its_retry_budget() {
    let _retry = CallgraphReadRetryTestGuard::install();
    let mut attempts = 0;
    let result = retry_dead_code_store_read(|| {
        attempts += 1;
        Err::<(), _>(CallGraphStoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        )))
    });
    assert!(result.unwrap_err().is_transient_lock_contention());
    assert_eq!(attempts, 20, "a persistent lock must stop at the deadline");
}
