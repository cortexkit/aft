//! Ownership of a checkout's graph acquisition, independent of any one read.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use super::callgraph::{graph_building, CallgraphViewError, ReaderAcquisition};
use crate::executor::{JobCancellation, RootMaintenanceExecutor};

pub(super) struct AcquisitionOwner {
    root: PathBuf,
    executor: Option<RootMaintenanceExecutor>,
    stopped: Arc<AtomicBool>,
    active: Mutex<Option<JobCancellation>>,
    standalone: Mutex<Vec<JoinHandle<()>>>,
}

impl AcquisitionOwner {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root,
            executor: None,
            stopped: Arc::new(AtomicBool::new(false)),
            active: Mutex::new(None),
            standalone: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn set_executor(&mut self, executor: Option<RootMaintenanceExecutor>) {
        self.executor = executor;
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(super) fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(token) = self.active.lock().unwrap().as_ref() {
            token.request_cancel();
        }
    }

    pub(super) fn shutdown(&self) {
        self.cancel();
        // Standalone has no executor to drain its workers. Keep their handles
        // so shutdown can cancel and join every acquisition before returning.
        let handles = std::mem::take(&mut *self.standalone.lock().unwrap());
        for handle in handles {
            let _ = handle.join();
        }
    }

    pub(super) fn spawn(
        &self,
        current: crate::migration::v3_ledger::ImportControl,
        acquire: impl FnOnce() -> ReaderAcquisition + Send + 'static,
        tx: crossbeam_channel::Sender<ReaderAcquisition>,
    ) -> Result<(), CallgraphViewError> {
        if self.stopped() {
            return Err(graph_building());
        }
        let cancellation = JobCancellation::new().with_root(&self.root);
        *self.active.lock().unwrap() = Some(cancellation.clone());
        let token = cancellation.clone();
        let stopped = self.stopped.clone();
        #[cfg(test)]
        let root = self.root.clone();
        let run = move || {
            let _cancel = crate::executor::current_job_cancellation()
                .is_none()
                .then(|| crate::executor::install_job_cancellation(token));
            let cancelled = || {
                crate::executor::current_job_cancelled()
                    || stopped.load(Ordering::Acquire)
                    || !current()
            };
            let result = if cancelled() {
                Err(graph_building())
            } else {
                acquire()
            };
            let result = if cancelled() {
                Err(graph_building())
            } else {
                result
            };
            #[cfg(test)]
            let finished = if result.is_ok() {
                "callgraph-reader-acquired"
            } else {
                "callgraph-acquisition-failed"
            };
            let _ = tx.send(result);
            #[cfg(test)]
            crate::storage_retention::test_hook(&root, finished);
        };
        if let Some(executor) = &self.executor {
            // A configured daemon submitter that has shut down must refuse;
            // falling back to a new thread would reactivate a retired root.
            if !executor.submit_heavy(
                cancellation,
                Box::new(move |_| {
                    run();
                    crate::protocol::Response::success(
                        "callgraph-view-acquire",
                        serde_json::Value::Null,
                    )
                }),
            ) {
                return Err(graph_building());
            }
        } else {
            let mut handles = self.standalone.lock().unwrap();
            handles.retain(|handle| !handle.is_finished());
            handles.push(
                std::thread::Builder::new()
                    .name("callgraph-view-acquire".into())
                    .spawn(run)
                    .map_err(|error| CallgraphViewError::from(error.to_string()))?,
            );
        }
        Ok(())
    }
}

impl Drop for AcquisitionOwner {
    fn drop(&mut self) {
        self.shutdown();
    }
}
