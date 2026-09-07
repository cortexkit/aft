//! Plane-worker refresh coordination.
//!
//! Watchers only enqueue invalidated paths. Plane workers receive prepared byte
//! payloads, deduplicate them by complete blob key, perform expensive work once,
//! and fan the result out to every affected path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use crate::blob_store::{BlobPlane, FullKey};
use crate::path_status::{PathStatusError, PathStatusStore};

/// A transient failure is retried three times with exponential backoff, then
/// increments the family-and-plane failure count that can trip its circuit breaker.
pub const TRANSIENT_RETRY_LIMIT: usize = 3;
/// Two deterministic failures of one full key quarantine that key.
pub const DETERMINISTIC_FAILURE_QUARANTINE_THRESHOLD: usize = 2;

/// A prepared path is created by a plane worker after it has made a stable read
/// and derived that plane's full key. Watchers never construct blob payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedWork {
    pub rel_path: Vec<u8>,
    pub full_key: FullKey,
    pub payload: Vec<u8>,
}

/// One expensive operation and every path that consumes its result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanoutWork {
    pub full_key: FullKey,
    pub payload: Vec<u8>,
    pub rel_paths: Vec<Vec<u8>>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum DedupError {
    ConflictingPayload { full_key: String },
}

impl fmt::Display for DedupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictingPayload { full_key } => write!(
                f,
                "prepared payloads for full key {full_key} differ; refusing to publish ambiguous work"
            ),
        }
    }
}

impl std::error::Error for DedupError {}

/// Groups byte-identical full keys before expensive extraction, embedding, or
/// blob insertion. The returned paths are bytewise ordered for deterministic
/// manifest assembly and telemetry correlation.
pub fn deduplicate_full_keys(
    work: impl IntoIterator<Item = PreparedWork>,
) -> Result<Vec<FanoutWork>, DedupError> {
    let mut grouped = BTreeMap::<(String, String), FanoutWork>::new();
    for item in work {
        let sort_key = (
            item.full_key.plane().as_str().to_owned(),
            item.full_key.to_hex(),
        );
        match grouped.get_mut(&sort_key) {
            Some(existing) => {
                if existing.payload != item.payload {
                    return Err(DedupError::ConflictingPayload {
                        full_key: item.full_key.to_hex(),
                    });
                }
                existing.rel_paths.push(item.rel_path);
            }
            None => {
                grouped.insert(
                    sort_key,
                    FanoutWork {
                        full_key: item.full_key,
                        payload: item.payload,
                        rel_paths: vec![item.rel_path],
                    },
                );
            }
        }
    }
    let mut result = grouped.into_values().collect::<Vec<_>>();
    for item in &mut result {
        item.rel_paths.sort();
        item.rel_paths.dedup();
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureClass {
    Transient,
    NonTransient,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerFailure {
    pub class: FailureClass,
    pub reason: String,
}

impl WorkerFailure {
    pub fn transient(reason: impl Into<String>) -> Self {
        Self {
            class: FailureClass::Transient,
            reason: reason.into(),
        }
    }

    pub fn non_transient(reason: impl Into<String>) -> Self {
        Self {
            class: FailureClass::NonTransient,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkDisposition {
    Published,
    Retry { retry: usize, backoff: Duration },
    Failed { failures: usize },
    Quarantined,
    BreakerRecorded { failures: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessedFanout {
    pub full_key: FullKey,
    pub rel_paths: Vec<Vec<u8>>,
    pub disposition: WorkDisposition,
    pub reason: Option<String>,
}

/// Records per-key deterministic failures and per-family, per-plane transient
/// breaker input. This is intentionally state-only: the BlobStore performs the
/// durable quarantine write after `Quarantined` is returned.
#[derive(Debug, Default)]
pub struct FailureTracker {
    deterministic_failures: HashMap<FullKey, usize>,
    transient_retries: HashMap<FullKey, usize>,
    quarantined: HashSet<FullKey>,
    breaker_failures: HashMap<(String, BlobPlane), usize>,
}

impl FailureTracker {
    pub fn record_failure(
        &mut self,
        family: &str,
        full_key: &FullKey,
        class: FailureClass,
    ) -> WorkDisposition {
        if self.quarantined.contains(full_key) {
            return WorkDisposition::Quarantined;
        }
        match class {
            FailureClass::Transient => {
                let retry = self.transient_retries.entry(full_key.clone()).or_default();
                if *retry < TRANSIENT_RETRY_LIMIT {
                    *retry += 1;
                    WorkDisposition::Retry {
                        retry: *retry,
                        backoff: transient_backoff(*retry),
                    }
                } else {
                    self.transient_retries.remove(full_key);
                    let failures = self
                        .breaker_failures
                        .entry((family.to_owned(), full_key.plane()))
                        .or_default();
                    *failures += 1;
                    WorkDisposition::BreakerRecorded {
                        failures: *failures,
                    }
                }
            }
            FailureClass::NonTransient => {
                let failures = self
                    .deterministic_failures
                    .entry(full_key.clone())
                    .or_default();
                *failures += 1;
                if *failures >= DETERMINISTIC_FAILURE_QUARANTINE_THRESHOLD {
                    self.quarantined.insert(full_key.clone());
                    WorkDisposition::Quarantined
                } else {
                    WorkDisposition::Failed {
                        failures: *failures,
                    }
                }
            }
        }
    }

    pub fn record_success(&mut self, full_key: &FullKey) {
        self.transient_retries.remove(full_key);
    }

    pub fn is_quarantined(&self, full_key: &FullKey) -> bool {
        self.quarantined.contains(full_key)
    }

    pub fn breaker_failures(&self, family: &str, plane: BlobPlane) -> usize {
        self.breaker_failures
            .get(&(family.to_owned(), plane))
            .copied()
            .unwrap_or_default()
    }
}

/// The retry number is one-based. The values are deliberately small enough for
/// worker scheduling tests while still providing a strict exponential sequence.
pub fn transient_backoff(retry: usize) -> Duration {
    let exponent = retry.saturating_sub(1).min(10) as u32;
    Duration::from_millis(100_u64.saturating_mul(1_u64 << exponent))
}

/// Runs a plane-worker operation once per full key and fans its disposition out
/// to all paths sharing the key. `operation` is where the worker hashes, puts,
/// derives, and publishes; this coordinator never runs on a watcher thread.
pub fn execute_plane_batch(
    tracker: &mut FailureTracker,
    family: &str,
    work: impl IntoIterator<Item = PreparedWork>,
    mut operation: impl FnMut(&FullKey, &[u8]) -> Result<(), WorkerFailure>,
) -> Result<Vec<ProcessedFanout>, DedupError> {
    deduplicate_full_keys(work)?
        .into_iter()
        .map(|work| match operation(&work.full_key, &work.payload) {
            Ok(()) => {
                tracker.record_success(&work.full_key);
                Ok(ProcessedFanout {
                    full_key: work.full_key,
                    rel_paths: work.rel_paths,
                    disposition: WorkDisposition::Published,
                    reason: None,
                })
            }
            Err(failure) => Ok(ProcessedFanout {
                disposition: tracker.record_failure(family, &work.full_key, failure.class),
                full_key: work.full_key,
                rel_paths: work.rel_paths,
                reason: Some(failure.reason),
            }),
        })
        .collect()
}

/// Applies plane-worker outcomes to the derived table. A publish removes the
/// annotation; retryable work remains pending, and exhausted or deterministic
/// failures remain failed while the last complete generation stays current.
pub fn apply_path_status(
    statuses: &mut PathStatusStore,
    outcomes: &[ProcessedFanout],
    since_generation: u64,
) -> Result<(), PathStatusError> {
    for outcome in outcomes {
        let reason = outcome.reason.as_deref().unwrap_or("refresh failed");
        for rel_path in &outcome.rel_paths {
            match outcome.disposition {
                WorkDisposition::Published => statuses.clear(rel_path)?,
                WorkDisposition::Retry { .. } => {
                    statuses.mark_pending(rel_path, reason, since_generation)?
                }
                WorkDisposition::Failed { .. }
                | WorkDisposition::Quarantined
                | WorkDisposition::BreakerRecorded { .. } => {
                    statuses.mark_failed(rel_path, reason, since_generation)?
                }
            }
        }
    }
    Ok(())
}

/// Records a persistent stable-read mismatch without publishing the path. The
/// next watcher event simply puts the path back through collection and can clear
/// this row after a later plane-worker publication.
pub fn preserve_pending_after_unstable_read(
    statuses: &mut PathStatusStore,
    rel_path: &[u8],
    since_generation: u64,
) -> Result<(), PathStatusError> {
    statuses.mark_pending(
        rel_path,
        "file changed while reading; awaiting a later watcher event",
        since_generation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::{CallgraphKey, SemanticKey};

    #[test]
    fn full_key_dedup_runs_one_operation_and_fans_out_bytewise_sorted_paths() {
        let key = CallgraphKey::for_current(b"same source", "rust").full_key();
        let mut tracker = FailureTracker::default();
        let mut operations = 0;
        let outcomes = execute_plane_batch(
            &mut tracker,
            "family-a",
            [
                PreparedWork {
                    rel_path: b"src/z.rs".to_vec(),
                    full_key: key.clone(),
                    payload: b"parse payload".to_vec(),
                },
                PreparedWork {
                    rel_path: b"src/a.rs".to_vec(),
                    full_key: key.clone(),
                    payload: b"parse payload".to_vec(),
                },
            ],
            |_, _| {
                operations += 1;
                Ok(())
            },
        )
        .expect("deduplicate work");

        assert_eq!(operations, 1);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].disposition, WorkDisposition::Published);
        assert_eq!(
            outcomes[0].rel_paths,
            vec![b"src/a.rs".to_vec(), b"src/z.rs".to_vec()]
        );
    }

    #[test]
    fn two_non_transient_failures_quarantine_only_that_full_key() {
        let first = SemanticKey::for_current(b"source", b"src/lib.rs", "model").full_key();
        let changed = SemanticKey::for_current(b"changed", b"src/lib.rs", "model").full_key();
        let mut tracker = FailureTracker::default();

        assert_eq!(
            tracker.record_failure("family-a", &first, FailureClass::NonTransient),
            WorkDisposition::Failed { failures: 1 }
        );
        assert_eq!(
            tracker.record_failure("family-a", &first, FailureClass::NonTransient),
            WorkDisposition::Quarantined
        );
        assert!(tracker.is_quarantined(&first));
        assert!(!tracker.is_quarantined(&changed));
        assert_eq!(
            tracker.record_failure("family-a", &changed, FailureClass::NonTransient),
            WorkDisposition::Failed { failures: 1 }
        );
    }

    #[test]
    fn three_transient_retries_back_off_before_the_plane_breaker_counts_failure() {
        let key = CallgraphKey::for_current(b"source", "rust").full_key();
        let mut tracker = FailureTracker::default();

        for retry in 1..=TRANSIENT_RETRY_LIMIT {
            assert_eq!(
                tracker.record_failure("family-a", &key, FailureClass::Transient),
                WorkDisposition::Retry {
                    retry,
                    backoff: transient_backoff(retry),
                }
            );
        }
        assert_eq!(
            tracker.record_failure("family-a", &key, FailureClass::Transient),
            WorkDisposition::BreakerRecorded { failures: 1 }
        );
        assert_eq!(
            tracker.breaker_failures("family-a", BlobPlane::Callgraph),
            1
        );
        assert_eq!(
            tracker.breaker_failures("other-family", BlobPlane::Callgraph),
            0
        );
    }

    #[test]
    fn persistent_read_and_worker_failures_annotate_without_a_publish() {
        let dir = tempfile::tempdir().expect("create view dir");
        let mut statuses = PathStatusStore::open(dir.path()).expect("open statuses");
        preserve_pending_after_unstable_read(&mut statuses, b"src/dirty.rs", 4)
            .expect("mark pending");
        let key = CallgraphKey::for_current(b"broken", "rust").full_key();
        let mut tracker = FailureTracker::default();
        let outcomes = execute_plane_batch(
            &mut tracker,
            "family-a",
            [PreparedWork {
                rel_path: b"src/broken.rs".to_vec(),
                full_key: key,
                payload: Vec::new(),
            }],
            |_, _| Err(WorkerFailure::non_transient("parse panic")),
        )
        .expect("process failed work");
        apply_path_status(&mut statuses, &outcomes, 4).expect("record failed status");

        let summary = statuses.summary().expect("summarize statuses");
        assert_eq!(summary.pending_count, 1);
        assert_eq!(summary.failed_count, 1);
        assert_eq!(summary.paths[0].rel_path, b"src/broken.rs");
        assert_eq!(summary.paths[1].rel_path, b"src/dirty.rs");
    }
}
