//! Bounded status totals based on the same root observations as health detail.

use crate::context::RootHealthSnapshot;
use crate::semantic_index::EmbeddingBackendBuildHealth;
use serde::Serialize;
use serde_json::{json, Value};

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
}

impl StatusCounts {
    fn add(&mut self, status: Option<&str>) {
        match status {
            Some("ready" | "refreshing") => self.ready += 1,
            Some("building") => self.building += 1,
            Some("off" | crate::semantic_admission::NOT_OPENED_LABEL) => self.off += 1,
            Some("disabled") => self.disabled += 1,
            // A failed or unobserved plane still accounts for one held root.
            _ => self.unavailable += 1,
        }
    }
}

impl IndexPlaneCounts {
    pub(super) fn add_root(
        &mut self,
        root: &RootHealthSnapshot,
        semantic_enabled: bool,
        backend_failed: bool,
    ) {
        self.search_index
            .add(root.search_index.as_ref().map(|index| index.status));
        self.callgraph_store
            .add(root.callgraph_store.as_ref().map(|index| index.status));
        self.semantic_index.add(
            match root.semantic_index.as_ref().map(|index| index.status) {
                Some(_) if backend_failed => Some("backend_unavailable"),
                // An enabled meaning plane with no data-layer or legacy build
                // reports the legacy default (disabled). Distinguish no work
                // from an index explicitly switched off in configuration.
                Some("disabled") if semantic_enabled => Some("off"),
                status => status,
            },
        );
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
            counts.add(status);
        }
        assert_eq!(
            json!(counts),
            json!({"ready": 2, "building": 1, "unavailable": 3, "off": 2, "disabled": 1})
        );
    }
}
