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
    active: Mutex<Vec<ActiveAcquisition>>,
    standalone: Mutex<Vec<JoinHandle<()>>>,
}

struct ActiveAcquisition {
    cancellation: JobCancellation,
    finished: Arc<AtomicBool>,
}

struct AcquisitionCompletion(Arc<AtomicBool>);

impl Drop for AcquisitionCompletion {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl AcquisitionOwner {

    pub(super) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(super) fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        for active in self.active.lock().unwrap().iter() {
            active.cancellation.request_cancel();
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
        let finished = Arc::new(AtomicBool::new(false));
        // Capturing the guard also completes the record if queued work is
        // dropped without running, or if source acquisition unwinds.
        let completion = AcquisitionCompletion(finished.clone());
        {
            let mut active = self.active.lock().unwrap();
            if self.stopped() {
                return Err(graph_building());
            }
            // The view's result receiver prevents overlapping source calls.
            // An older worker can still be returning after sending its result,
            // so keep its token until its closure finishes.
            active.retain(|job| !job.finished.load(Ordering::Acquire));
            active.push(ActiveAcquisition {
                cancellation: cancellation.clone(),
                finished,
            });
        }
        let token = cancellation.clone();
        let stopped = self.stopped.clone();
        #[cfg(test)]
        let root = self.root.clone();
        // Fixture attribute-source overrides are thread-local. Carry the
        // caller's overrides onto the acquisition worker.
        #[cfg(test)]
        let attribute_sources = super::verification::attribute_sources_snapshot_for_test();
        let run = move || {
            let _completion = completion;
            #[cfg(test)]
            let _attributes =
                super::verification::install_attribute_sources_for_test(attribute_sources);
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
            if !executor.submit_callgraph_acquisition(
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
            if self.stopped() {
                return Err(graph_building());
            }
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
