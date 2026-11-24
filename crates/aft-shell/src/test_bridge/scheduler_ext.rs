use crate::bash_background::runner_notice_scheduler::{Next, Scheduled, Scheduler, Work};
use std::{collections::HashMap, sync::Condvar, time::Instant};

pub trait SchedulerTestExt {
    fn attachment_changed(&self, attached: bool);
    fn bus_wake(&self, send_id: Option<&str>) -> Vec<tokio::sync::oneshot::Receiver<()>>;
    fn take(entries: &mut HashMap<String, Scheduled>, now: Instant) -> Option<(String, Work)>;
    fn finish(&self, id: &str, work: Work, next: Next, now: Instant);
    fn wake(&self) -> &Condvar;
}

impl SchedulerTestExt for Scheduler {
    fn attachment_changed(&self, attached: bool) {
        Scheduler::attachment_changed(self, attached);
    }
    fn bus_wake(&self, send_id: Option<&str>) -> Vec<tokio::sync::oneshot::Receiver<()>> {
        Scheduler::bus_wake(self, send_id)
    }
    fn take(entries: &mut HashMap<String, Scheduled>, now: Instant) -> Option<(String, Work)> {
        Scheduler::take(entries, now)
    }
    fn finish(&self, id: &str, work: Work, next: Next, now: Instant) {
        Scheduler::finish(self, id, work, next, now);
    }
    fn wake(&self) -> &Condvar {
        &self.wake
    }
}

pub trait ScheduledTestExt {
    fn due(&self) -> Instant;
}

impl ScheduledTestExt for Scheduled {
    fn due(&self) -> Instant {
        self.due
    }
}
