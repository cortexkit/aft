//! What an index-store semantic answer could not cover, and how the reply
//! names it.

use std::path::{Path, PathBuf};

use super::project_relative_path;

/// What an index-store semantic answer could not cover, named so an incomplete
/// answer says which files it is missing instead of looking complete.
pub(super) struct CheckoutSemanticGaps {
    /// Files excluded by grammar availability or parsing policy, with typed
    /// reasons. They are not waiting for a fill and are not fill failures.
    skipped: Vec<serde_json::Value>,
    /// Files whose current content has no vectors yet (awaiting a fill, or
    /// edited since the last one).
    pending: Vec<PathBuf>,
    /// Terminal failures extracting a file's outline or embedding its chunks.
    failed: Vec<(PathBuf, String)>,
    /// The watcher could not vouch for membership, so the checkout was
    /// re-walked and vectors may lag the disk.
    unvouched: bool,
    /// The checkout view could not be read at all.
    unavailable: Option<String>,
    /// Why the data layer's last fill stopped, while files still wait for
    /// vectors; it is tried again later. Never set for a views-on answer.
    fill_failure: Option<String>,
    /// Files past `semantic.max_files` that no checkout has embedded: they
    /// are not waiting for a fill, and the search cannot find them.
    past_cap: usize,
}

/// How many gap paths the response text names before summarizing the rest.
const CHECKOUT_SEMANTIC_GAP_NAMES: usize = 5;

impl CheckoutSemanticGaps {
    pub(super) fn status(&self) -> &'static str {
        if self.unavailable.is_some() {
            "unavailable"
        } else if self.pending.is_empty()
            && !self.skipped.iter().any(|gap| {
                matches!(
                    gap["state"].as_str(),
                    Some("not_downloaded" | "downloading" | "network_retry" | "storage_retry")
                )
            })
            && !self.unvouched
            && self.fill_failure.is_none()
        {
            "ready"
        } else {
            "refreshing"
        }
    }

    /// Pending work and recorded exclusions from data-layer semantic search.
    pub(super) fn from_data(meaning: &crate::commands::data_search::DataMeaning) -> Self {
        let mut gaps = Self::from_coverage(meaning.coverage());
        gaps.fill_failure = meaning.failure().map(str::to_string);
        gaps
    }

    fn from_coverage(coverage: &crate::data_layer::vectors::Coverage) -> Self {
        let reply = if coverage.skipped_parses.is_empty() {
            crate::data_layer::grammar::GapReply::default()
        } else {
            crate::data_layer::grammar::GapReply::capture()
        };
        Self {
            skipped: coverage
                .skipped_parses
                .iter()
                .filter_map(|(path, reason)| reply.value(reason, path))
                .collect(),
            pending: coverage.pending.clone(),
            failed: coverage.grammar_outline_failures().cloned().collect(),
            unvouched: false,
            unavailable: None,
            fill_failure: None,
            past_cap: coverage.past_cap,
        }
    }

    pub(super) fn unavailable(reason: String) -> Self {
        Self {
            skipped: Vec::new(),
            pending: Vec::new(),
            failed: Vec::new(),
            unvouched: false,
            unavailable: Some(reason),
            fill_failure: None,
            past_cap: 0,
        }
    }

    pub(super) fn disclose(
        &self,
        project_root: &Path,
        text: &mut String,
        extras: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        let relative = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|path| {
                    project_relative_path(path, project_root)
                        .display()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let pending = relative(&self.pending);
        let failed = relative(
            &self
                .failed
                .iter()
                .take(crate::data_layer::surfaces::INDEX_GAP_PREVIEW)
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
        );
        let named = |label: &str, paths: &[String]| {
            let shown = paths
                .iter()
                .take(CHECKOUT_SEMANTIC_GAP_NAMES)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let more = paths.len().saturating_sub(CHECKOUT_SEMANTIC_GAP_NAMES);
            if more > 0 {
                format!(
                    "semantic: {} file(s) {label}: {shown} (+{more} more)",
                    paths.len()
                )
            } else {
                format!("semantic: {} file(s) {label}: {shown}", paths.len())
            }
        };
        let mut lines = Vec::new();
        let skipped = self
            .skipped
            .iter()
            .take(crate::data_layer::surfaces::INDEX_GAP_PREVIEW)
            .map(|gap| {
                let mut gap = gap.clone();
                if let Some(path) = gap["path"].as_str() {
                    gap["path"] =
                        serde_json::json!(project_relative_path(Path::new(path), project_root));
                }
                gap
            })
            .collect::<Vec<_>>();
        for gap in &skipped {
            lines.push(format!(
                "{}: {}",
                gap["path"].as_str().unwrap_or_default(),
                gap["message"].as_str().unwrap_or_default()
            ));
        }
        let preview =
            crate::data_layer::surfaces::parse_gap_preview(skipped.len(), self.skipped.len());
        if let Some(trailer) = crate::subc_format::render_envelope_trailer(&preview) {
            lines.push(trailer);
        }
        if let Some(reason) = &self.unavailable {
            lines.push(format!("semantic: unavailable: {reason}"));
        }
        if !pending.is_empty() {
            lines.push(named("not yet embedded", &pending));
        }
        for (path, (_, reason)) in failed.iter().zip(&self.failed) {
            lines.push(format!("{path}: {reason}"));
        }
        let failed_preview =
            crate::data_layer::surfaces::parse_gap_preview(failed.len(), self.failed.len());
        if let Some(trailer) = crate::subc_format::render_envelope_trailer(&failed_preview) {
            lines.push(trailer);
        }
        if self.unvouched {
            lines.push(
                "semantic: file membership was re-walked because changes may have been missed; vectors can lag the disk"
                    .to_string(),
            );
        }
        if let Some(error) = &self.fill_failure {
            lines.push(format!(
                "semantic: embedding the waiting files stopped and is retried later: {error}"
            ));
        }
        if self.past_cap > 0 {
            lines.push(format!(
                "semantic: left {} file(s) past semantic.max_files without vectors; they are not searched",
                self.past_cap
            ));
        }
        extras.insert("complete".into(), serde_json::json!(false));
        let mut gap = serde_json::json!({
            "pending": pending,
            "failed": failed,
            "unvouched": self.unvouched,
            "unavailable": self.unavailable,
        });
        if !failed.is_empty() {
            gap["failed_files"] = serde_json::json!(self
                .failed
                .iter()
                .zip(&failed)
                .map(|((_, reason), path)| serde_json::json!({"path":path,"reason":reason}))
                .collect::<Vec<_>>());
            gap["failed_files_count"] = serde_json::json!(self.failed.len());
            gap["failed_files_omitted"] = serde_json::json!(self.failed.len() - failed.len());
            gap["failed_files_list"] = serde_json::json!(failed_preview);
        }
        if !skipped.is_empty() {
            gap["skipped_files"] = serde_json::json!(skipped);
            gap["skipped_files_count"] = serde_json::json!(self.skipped.len());
            let omitted =
                self.skipped.len() - gap["skipped_files"].as_array().expect("skip preview").len();
            gap["skipped_files_omitted"] = serde_json::json!(omitted);
            gap["skipped_files_list"] = serde_json::json!(preview);
        }
        if let Some(error) = &self.fill_failure {
            gap["fill_failure"] = serde_json::json!(error);
        }
        if self.past_cap > 0 {
            gap["past_cap"] = serde_json::json!(self.past_cap);
        }
        extras.insert("semantic_gap".into(), gap);
        for line in lines {
            text.push('\n');
            text.push_str(&line);
        }
    }
}
