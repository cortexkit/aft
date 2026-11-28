//! Bounded status totals based on the same root observations as health detail.

use super::HealthStatus;
use crate::config::IndexesConfig;
use crate::context::{RootHealthSnapshot, RootHealthState};
use crate::semantic_index::EmbeddingBackendBuildHealth;
use aft_engine::feature_status::cause;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;

const INDEX_PLANE_OUTAGE_MS: u64 = 60 * 60 * 1_000;

#[derive(Default, Serialize)]
pub(super) struct IndexPlaneCounts {
    search_index: StatusCounts,
    semantic_index: StatusCounts,
    callgraph_store: StatusCounts,
}

#[derive(Default, Serialize)]
struct StatusCounts {
    ready: usize,
    building: usize,
    unavailable: usize,
    off: usize,
    disabled: usize,
    #[serde(skip)]
    enabled_roots: usize,
    #[serde(skip)]
    has_progress: bool,
    #[serde(skip)]
    has_unmeasured_root: bool,
    #[serde(skip)]
    reasons: BTreeMap<&'static str, usize>,
}

impl StatusCounts {
    fn add(
        &mut self,
        status: Option<&str>,
        enabled: bool,
        unmeasured: bool,
        reason: impl FnOnce() -> &'static str,
    ) {
        let (bucket, progress) = match status {
            Some("ready" | "refreshing") => (&mut self.ready, true),
            Some("building") => (&mut self.building, true),
            Some("off" | crate::semantic_admission::NOT_OPENED_LABEL) => (&mut self.off, false),
            Some("disabled") => (&mut self.disabled, false),
            // A failed or unobserved plane still accounts for one held root.
            _ => (&mut self.unavailable, false),
        };
        *bucket += 1;
        if enabled {
            self.enabled_roots += 1;
            self.has_progress |= progress;
            self.has_unmeasured_root |= unmeasured;
            if !self.has_progress && !unmeasured {
                *self.reasons.entry(reason()).or_default() += 1;
            }
        }
    }
}

impl IndexPlaneCounts {
    pub(super) fn add_root(
        &mut self,
        root: &RootHealthSnapshot,
        indexes: Option<IndexesConfig>,
        provisional_identity: Option<&str>,
        backend_failed: bool,
        has_meaning_opener: bool,
    ) {
        let unmeasured = root.state == RootHealthState::Busy;
        let semantic_enabled = indexes.is_some_and(|config| config.semantic);
        let search_status = root.search_index.as_ref().map(|index| index.status);
        let graph_status = root.callgraph_store.as_ref().map(|index| index.status);
        let semantic_status = match root.semantic_index.as_ref().map(|index| index.status) {
            Some(_) if backend_failed => Some("backend_unavailable"),
            // No semantic build can report disabled even with the setting
            // on. Count that as off, not as explicitly disabled.
            Some("disabled") if semantic_enabled => Some("off"),
            status => status,
        };
        self.search_index.add(
            search_status,
            indexes.is_some_and(|config| config.trigram),
            unmeasured,
            || blocked_reason(root, "trigram", search_status, provisional_identity),
        );
        self.callgraph_store.add(
            graph_status,
            indexes.is_some_and(|config| config.callgraph),
            unmeasured,
            || blocked_reason(root, "callgraph", graph_status, provisional_identity),
        );
        // Without an installed route that can start semantic indexing, a
        // missing index is not an outage. Still include it in the status totals.
        self.semantic_index.add(
            semantic_status,
            semantic_enabled && has_meaning_opener,
            unmeasured,
            || blocked_reason(root, "semantic", semantic_status, provisional_identity),
        );
    }
}

fn blocked_reason(
    root: &RootHealthSnapshot,
    plane: &str,
    status: Option<&str>,
    provisional_identity: Option<&str>,
) -> &'static str {
    use crate::data_layer::surfaces::reason;
    if matches!(status, Some("backend_unavailable" | "paused")) {
        return cause::SEMANTIC_BACKEND_UNAVAILABLE;
    }
    if root.view_storage_reserve.contains_key(plane)
        || (plane == "semantic"
            && root
                .semantic_index
                .as_ref()
                .and_then(|index| index.reason.as_deref())
                .is_some_and(crate::view_storage_reserve::is_message))
    {
        return reason::DISK_RESERVE;
    }
    if let Some(reason) = provisional_identity {
        return if reason.starts_with(crate::root_identity::key::GIT_IDENTITY_UNAVAILABLE_REASON) {
            "git_identity_unavailable"
        } else {
            cause::IDENTITY_PROVISIONAL
        };
    }
    if let Some(reason) = root.cache_unavailable_reason {
        return reason;
    }
    if let Some(reason) = root.data_store.as_ref().and_then(data_store_failure_reason) {
        return reason;
    }
    match status {
        Some(crate::semantic_admission::NOT_OPENED_LABEL) => cause::SEMANTIC_NOT_OPENED,
        Some("degraded") if plane == "semantic" => cause::SEMANTIC_BUILD_FAILED,
        _ => cause::RUNTIME_NOT_OBSERVED,
    }
}

/// Only measured samples can start an outage or publish it. A busy root
/// leaves the current interval undecided unless another root is progressing.
#[derive(Default)]
pub(super) struct IndexPlaneOutageTracker(Mutex<[Option<u64>; 3]>);

#[derive(Clone)]
pub(in crate::subc) struct IndexPlaneOutage {
    pub plane: &'static str,
    label: &'static str,
    pub root_count: usize,
    pub reason: &'static str,
    pub reason_count: usize,
    pub elapsed_ms: u64,
}

impl IndexPlaneOutage {
    pub fn summary(&self) -> String {
        format!(
            "{} has not been ready on any open project for over an hour (most common reason: {}).",
            self.label, self.reason,
        )
    }
}

impl IndexPlaneOutageTracker {
    pub(super) fn observe(&self, counts: &IndexPlaneCounts, now_ms: u64) -> Vec<IndexPlaneOutage> {
        let mut starts = self.0.lock().unwrap_or_else(|error| error.into_inner());
        [
            ("trigram", "Text search", &counts.search_index),
            ("semantic", "Meaning search", &counts.semantic_index),
            ("callgraph", "The call graph", &counts.callgraph_store),
        ]
        .into_iter()
        .zip(starts.iter_mut())
        .filter_map(|((plane, label, counts), start)| {
            if counts.enabled_roots == 0 || counts.has_progress {
                *start = None;
                return None;
            }
            if counts.has_unmeasured_root {
                return None;
            }
            let elapsed_ms = now_ms.saturating_sub(*start.get_or_insert(now_ms));
            if elapsed_ms <= INDEX_PLANE_OUTAGE_MS {
                return None;
            }
            // With equal counts, report the first reason in lexical order.
            let (&reason, &reason_count) = counts
                .reasons
                .iter()
                .rev()
                .max_by_key(|(_, count)| **count)?;
            Some(IndexPlaneOutage {
                plane,
                label,
                root_count: counts.enabled_roots,
                reason,
                reason_count,
                elapsed_ms,
            })
        })
        .collect()
    }
}

pub(super) fn with_outages(
    state: (HealthStatus, Option<String>),
    outages: &[IndexPlaneOutage],
) -> (HealthStatus, Option<String>) {
    if outages.is_empty() || state.0 != HealthStatus::Ok {
        return state;
    }
    let detail = outages
        .iter()
        .map(IndexPlaneOutage::summary)
        .collect::<Vec<_>>()
        .join(" ");
    (HealthStatus::Degraded, Some(detail))
}

/// A build or refresh failure can leave an earlier publication serving, in
/// which case the store reports a failure without replacing the status reason.
pub(super) fn data_store_failure_reason(
    store: &crate::data_layer::surfaces::DataStoreHealth,
) -> Option<&'static str> {
    use crate::data_layer::surfaces::reason;
    match (store.reason, &store.failure) {
        (Some(code @ (reason::BIND_FAILED | reason::REQUIRES_NEWER_READER)), _) => Some(code),
        (_, None) => None,
        (
            Some(
                code @ (reason::BUILD_FAILED
                | reason::REFRESH_FAILED
                | reason::STORE_BUSY
                | reason::DISK_RESERVE),
            ),
            Some(_),
        ) => Some(code),
        (_, Some(failure)) => Some(if crate::view_storage_reserve::is_message(&failure.error) {
            reason::DISK_RESERVE
        } else if failure.busy {
            reason::STORE_BUSY
        } else if failure.stage == "refresh" {
            reason::REFRESH_FAILED
        } else {
            reason::BUILD_FAILED
        }),
    }
}

pub(super) fn annotate_backend_failure(value: &mut Value, backend: &EmbeddingBackendBuildHealth) {
    if let Some(semantic) = value
        .get_mut("semantic_index")
        .and_then(Value::as_object_mut)
    {
        semantic.insert("status".to_string(), json!("backend_unavailable"));
        semantic.insert("reason".to_string(), json!(backend.last_error));
        semantic.insert("since_ms".to_string(), json!(backend.since_ms));
        semantic.insert("next_retry_ms".to_string(), json!(backend.next_retry_ms));
        // A backend failure stops progress, but the plane's failure counts stay.
        for progress in [
            "stage",
            "embedded_chunks",
            "total_chunks",
            "current_batch",
            "total_batches",
            "files",
            "files_embedded",
            "files_pending",
            "files_too_large",
            "files_past_cap",
            "filling",
            "last_progress_at_ms",
        ] {
            semantic.remove(progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_plane_counts_keep_all_status_buckets() {
        let mut counts = StatusCounts::default();
        for status in [
            Some("ready"),
            Some("refreshing"),
            Some("building"),
            Some("off"),
            Some(crate::semantic_admission::NOT_OPENED_LABEL),
            Some("disabled"),
            Some("degraded"),
            Some("backend_unavailable"),
            None,
        ] {
            counts.add(status, true, false, || "unused");
        }
        assert_eq!(
            json!(counts),
            json!({"ready": 2, "building": 1, "unavailable": 3, "off": 2, "disabled": 1})
        );
    }

    #[test]
    fn index_plane_outage_reports_the_most_common_reason_per_plane() {
        let tracker = IndexPlaneOutageTracker::default();
        let mut counts = IndexPlaneCounts::default();
        let disk = crate::data_layer::surfaces::reason::DISK_RESERVE;
        for reason in [disk, cause::RUNTIME_NOT_OBSERVED, disk] {
            counts
                .search_index
                .add(Some("unavailable"), true, false, || reason);
        }
        counts
            .callgraph_store
            .add(Some("off"), true, false, || "git_identity_unavailable");
        counts
            .semantic_index
            .add(Some("disabled"), false, false, || unreachable!());

        assert!(tracker.observe(&counts, 0).is_empty());
        let outages = tracker.observe(&counts, INDEX_PLANE_OUTAGE_MS + 1);
        let evidence = outages
            .iter()
            .map(|outage| {
                (
                    outage.plane,
                    outage.root_count,
                    outage.reason,
                    outage.reason_count,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            evidence,
            [
                ("trigram", 3, disk, 2),
                ("callgraph", 1, "git_identity_unavailable", 1),
            ]
        );
        assert_eq!(
            outages[0].summary(),
            "Text search has not been ready on any open project for over an hour (most common reason: disk_reserve)."
        );
        assert_eq!(
            outages[1].summary(),
            "The call graph has not been ready on any open project for over an hour (most common reason: git_identity_unavailable)."
        );
    }

    #[test]
    fn index_plane_outage_requires_a_new_interval_after_progress_or_no_enabled_roots() {
        let at = |status, enabled, unmeasured| {
            let mut counts = IndexPlaneCounts::default();
            counts
                .semantic_index
                .add(status, enabled, unmeasured, || cause::RUNTIME_NOT_OBSERVED);
            counts
        };
        for restored in [
            at(Some("building"), true, false),
            at(Some("refreshing"), true, false),
            at(Some("disabled"), false, false),
            IndexPlaneCounts::default(),
        ] {
            let tracker = IndexPlaneOutageTracker::default();
            let stalled = at(Some("off"), true, false);
            let mut now = 10;
            assert!(tracker.observe(&stalled, now).is_empty());
            now += INDEX_PLANE_OUTAGE_MS + 1;
            assert_eq!(tracker.observe(&stalled, now).len(), 1);
            assert!(tracker.observe(&restored, now + 1).is_empty());
            assert!(tracker.observe(&stalled, now + 2).is_empty());
        }

        let tracker = IndexPlaneOutageTracker::default();
        let unknown = at(None, true, true);
        assert!(tracker.observe(&unknown, 0).is_empty());
        let stalled = at(Some("off"), true, false);
        assert!(tracker
            .observe(&stalled, INDEX_PLANE_OUTAGE_MS + 1)
            .is_empty());
    }
}
