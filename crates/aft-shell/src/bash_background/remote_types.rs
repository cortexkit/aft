use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyedSubmission {
    pub(super) key: String,
    pub(super) created_at_ms: u64,
    pub(crate) request_digest: String,
    pub(super) attempts: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) recovery_started_at_ms: Option<u64>,
    pub(super) fresh_key_used: bool,
    pub(super) resuming: bool,
    pub(crate) reattached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) refusal: Option<TerminalRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DrainingRetry {
    pub(super) request_digest: String,
    pub(super) attempts: u32,
    pub(crate) first_refusal_at_ms: Option<u64>,
    pub(crate) next_retry_at_ms: Option<u64>,
    pub(super) waited_ms: Option<u64>,
    /// When AFT last submitted the job again after a draining refusal, in
    /// Unix milliseconds. The runner's queue wait and run timeout count from
    /// that submission, so re-attach limits do too. Records written before
    /// this field existed lack it and count from the task's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resubmitted_at_ms: Option<u64>,
}

#[cfg(feature = "test-support")]
pub trait KeyedSubmissionTestExt {
    fn reattached(&self) -> bool;
    fn request_digest(&self) -> &String;
}

#[cfg(feature = "test-support")]
impl KeyedSubmissionTestExt for KeyedSubmission {
    fn reattached(&self) -> bool {
        self.reattached
    }
    fn request_digest(&self) -> &String {
        &self.request_digest
    }
}

#[cfg(feature = "test-support")]
pub trait DrainingRetryTestExt {
    fn first_refusal_at_ms(&self) -> Option<u64>;
    fn next_retry_at_ms(&self) -> Option<u64>;
}

#[cfg(feature = "test-support")]
impl DrainingRetryTestExt for DrainingRetry {
    fn first_refusal_at_ms(&self) -> Option<u64> {
        self.first_refusal_at_ms
    }
    fn next_retry_at_ms(&self) -> Option<u64> {
        self.next_retry_at_ms
    }
}

#[cfg(unix)]
pub struct TaskSink {
    pub(super) registry: BgTaskRegistry,
    pub(super) task: Arc<BgTask>,
    pub(super) stdout: fs::File,
    pub(super) stderr: fs::File,
    pub(super) unknown: Option<fs::File>,
    pub(super) dirty: DirtyOutput,
    // Keep newly received byte counts and sequence numbers out of shared task
    // metadata until the files are synced. Cancellation also saves that metadata.
    pub(super) output_progress: OutputProgress,
    pub(super) last_progress_persist: Instant,
    pub(super) sent_at: Option<Instant>,
    #[cfg(any(test, feature = "test-support"))]
    pub(super) progress_clock: Option<Arc<dyn Fn() -> Instant + Send + Sync>>,
    /// Runs between the checkpoint snapshot and its file write, so a test can
    /// land another metadata write in that window.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) before_checkpoint_write: Option<Box<dyn FnMut() + Send>>,
    /// Stages the job's returned-file chunks until the runner's manifest
    /// arrives, then places the files that verify.
    pub(super) returns: Option<super::super::return_files::ReturnReceiver>,
    /// A handled manifest's report whose save failed, saved when the
    /// consumer delivers the manifest again.
    pub(super) unsaved_return_report: Option<Vec<super::super::return_files::ReturnedPath>>,
}
