use super::{ReplayKey, UnackedCompletions};
use crate::bash_background::{task_identity_scope, BgTaskRegistry, TaskIdentity};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;

pub(super) struct Settlement {
    pub(super) registry: BgTaskRegistry,
    pub(super) key: ReplayKey,
    pub(super) task: String,
    pub(super) unacked: UnackedCompletions,
    pub(super) offers: Option<usize>,
    #[cfg(test)]
    pub(super) gate: Option<TestGate>,
}

impl Settlement {
    fn run(&self) {
        #[cfg(test)]
        if let Some(gate) = &self.gate {
            let _ = gate.entered.send(());
            let _ = gate.release.recv();
        }
        let identity = TaskIdentity {
            principal: self.key.principal.clone(),
            harness: self.key.harness.clone(),
            session: self.key.session.clone(),
        };
        let _scope = task_identity_scope(Some(identity));
        if let Some(offers) = self.offers {
            if let Err(error) =
                self.registry
                    .persist_runner_completion_offer(&self.key.session, &self.task, offers)
            {
                log::warn!("subc completion push: task={} could not persist runner offer count={offers} key={:?}: {error}", self.task, self.key);
            }
            return;
        }
        if self
            .registry
            .ack_completions_for_session(Some(&self.key.session), std::slice::from_ref(&self.task))
            .contains(&self.task)
        {
            if let Some(sessions) = self.unacked.lock().unwrap().get_mut(&self.key.root) {
                if let Some(tasks) = sessions.get_mut(&self.key.session) {
                    tasks.remove(&self.task);
                }
            }
        } else {
            log::warn!("subc completion push: task={} could not durably mark runner completion delivered key={:?}; suppressing further offers in this process", self.task, self.key);
        }
    }
}

enum Work {
    Settle(Settlement),
    #[cfg(test)]
    Barrier(std::sync::mpsc::Sender<()>),
}

fn sender() -> Result<&'static SyncSender<Work>, &'static String> {
    static WORKER: OnceLock<Result<SyncSender<Work>, String>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            // A single process-wide writer serializes all roots, including jobs
            // queued by different connections. Admission never waits for disk or
            // queue capacity; a full queue leaves the task pending until restart.
            let (tx, rx) = sync_channel(1024);
            std::thread::Builder::new()
                .name("aft-completion-settlement".into())
                .spawn(move || {
                    while let Ok(work) = rx.recv() {
                        match work {
                            Work::Settle(job) => {
                                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run()))
                                    .is_err()
                                {
                                    log::warn!("subc completion push: task={} durable runner completion worker panicked key={:?}; suppressing further offers in this process", job.task, job.key);
                                }
                            }
                            #[cfg(test)]
                            Work::Barrier(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                })
                .map(|_| tx)
                .map_err(|error| error.to_string())
        })
        .as_ref()
}

pub(super) fn enqueue(job: Settlement) -> Result<(), String> {
    sender()
        .map_err(Clone::clone)?
        .try_send(Work::Settle(job))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
pub(super) struct TestGate {
    pub(super) entered: std::sync::mpsc::Sender<()>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(super) fn wait_for_settlements() {
    let (done, finished) = std::sync::mpsc::channel();
    assert!(sender().unwrap().send(Work::Barrier(done)).is_ok());
    finished
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
}
