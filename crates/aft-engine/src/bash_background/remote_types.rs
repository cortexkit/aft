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
