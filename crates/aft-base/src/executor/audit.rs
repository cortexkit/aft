use std::time::{Duration, Instant};

// Thread-local operation counters keep concurrent tests from charging each
// other's filesystem work. They observe the call sites, not elapsed time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuditWork {
    pub cancel_probes: usize,
    pub corpus_opens: usize,
    pub sort_stats: usize,
    pub sort_canonicalizes: usize,
    pub watcher_global: usize,
    pub watcher_head: usize,
    pub filter_keys: usize,
    pub presence_checks: usize,
}

thread_local! {
    static AUDIT_WORK: std::cell::Cell<AuditWork> = std::cell::Cell::new(AuditWork::default());
    pub static AUDIT_IO_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    pub static AUDIT_CANCEL_ELAPSED: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

pub struct HelperStateScope(AuditWork, Option<Box<dyn FnMut()>>, Option<Duration>);
impl Drop for HelperStateScope {
    fn drop(&mut self) {
        AUDIT_WORK.with(|work| work.set(self.0));
        AUDIT_IO_PROBE.with(|probe| *probe.borrow_mut() = self.1.take());
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.set(self.2));
    }
}
pub fn helper_state_scope() -> HelperStateScope {
    HelperStateScope(
        AUDIT_WORK.with(|work| work.replace(AuditWork::default())),
        AUDIT_IO_PROBE.with(|probe| probe.replace(None)),
        AUDIT_CANCEL_ELAPSED.with(|elapsed| elapsed.replace(None)),
    )
}

pub fn audit_cancel_elapsed(started: Instant) -> Duration {
    AUDIT_CANCEL_ELAPSED.with(|slot| slot.get().unwrap_or_else(|| started.elapsed()))
}

pub fn audit_record(record: impl FnOnce(&mut AuditWork)) {
    AUDIT_WORK.with(|slot| {
        let mut work = slot.get();
        record(&mut work);
        slot.set(work);
    });
}

pub fn audit_work_reset() {
    AUDIT_WORK.with(|slot| slot.set(AuditWork::default()));
}

pub fn audit_work() -> AuditWork {
    AUDIT_WORK.with(std::cell::Cell::get)
}
