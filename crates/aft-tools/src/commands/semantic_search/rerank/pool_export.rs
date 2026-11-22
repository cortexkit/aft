//! Benchmark-only export of the pools the reranker sees.
//!
//! With `AFT_RERANK_POOL_EXPORT` naming a file, every canonical list that
//! reaches the rerank step is appended to it once, as one JSON line: the
//! query as sent, the prose the reranker would score, whether the prose gate
//! lets the query through, the first entries of the first block before the
//! rerank step (path, symbol start, exact or not, and whether the policy would
//! rerank it), the candidate text of every entry the policy would rerank, and
//! the first entries after the rerank step with its outcome. A benchmark can
//! then score the same candidates with other rerankers and apply the policy
//! offline, and compare that replay with what the engine served. The variable
//! is read per search and nothing is exported when it is unset.

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::super::blocks::{CanonicalList, FrozenBlock};
use super::super::evidence_descriptor::EvidenceTier;
use super::{candidate_text, display_path, is_prose_query, rerank_positions, RerankRequest};

pub(crate) const POOL_EXPORT_ENV: &str = "AFT_RERANK_POOL_EXPORT";
/// Entries of the first block exported before and after the rerank step.
/// The reranker reorders the first `top_n` non-exact entries of that block,
/// which sit within its first 100 entries whenever fewer than 80 exact
/// entries lead it, and a top-ten list of files is cut from the same head.
const EXPORT_DEPTH: usize = 100;

/// A pool captured before the rerank step, finished after it.
pub(crate) struct PendingExport {
    path: std::path::PathBuf,
    record: serde_json::Map<String, Value>,
}

fn block_zero(list: &CanonicalList) -> Option<&FrozenBlock> {
    list.blocks.iter().find(|block| block.tier_index == 0)
}

fn entries(list: &CanonicalList, project_root: &std::path::Path) -> Value {
    block_zero(list)
        .map(|block| {
            block
                .entries
                .iter()
                .take(EXPORT_DEPTH)
                .map(|entry| {
                    json!({
                        "path": display_path(project_root, &entry.result.path),
                        "symbol_start": entry.result.symbol_range.map(|range| range.start),
                        "exact": entry.result.evidence.tier == EvidenceTier::Exact,
                    })
                })
                .collect::<Vec<_>>()
        })
        .map(Value::Array)
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

/// Start an export for this list, or `None` when exporting is off or the
/// list was already exported by this process.
pub(crate) fn start(
    list: &CanonicalList,
    request: &RerankRequest<'_>,
    top_n: usize,
) -> Option<PendingExport> {
    let path = crate::environment::non_empty_os_var(POOL_EXPORT_ENV)?;
    let identity = format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        list.key.project_root.display(),
        list.key.snapshot_generation,
        list.key.normalized_query,
        list.key.include_tests,
        request.public_query
    );
    static EXPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    if !EXPORTED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(identity)
    {
        return None;
    }
    let prose = request.prose.map(str::trim).unwrap_or_default();
    let gate = !prose.is_empty() && is_prose_query(request.public_query);
    let mut record = serde_json::Map::new();
    record.insert("public_query".into(), json!(request.public_query));
    record.insert("prose".into(), json!(prose));
    record.insert("include_tests".into(), json!(list.key.include_tests));
    record.insert("gate".into(), json!(if gate { "rerank" } else { "skip" }));
    record.insert("before".into(), entries(list, request.project_root));
    let candidates = match (gate, block_zero(list)) {
        (true, Some(block)) => rerank_positions(block, top_n)
            .into_iter()
            .map(|position| {
                let result = &block.entries[position].result;
                json!({
                    "position": position,
                    "path": display_path(request.project_root, &result.path),
                    "symbol_start": result.symbol_range.map(|range| range.start),
                    "text": candidate_text(request.project_root, result, prose),
                })
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    record.insert("candidates".into(), Value::Array(candidates));
    Some(PendingExport {
        path: path.into(),
        record,
    })
}

impl PendingExport {
    /// Record the outcome and the order after the rerank step, and append the
    /// line. A write failure is logged and otherwise ignored: the export must
    /// never change a search.
    pub(crate) fn finish(
        mut self,
        list: &CanonicalList,
        project_root: &std::path::Path,
        outcome: &Result<Option<String>, String>,
    ) {
        let outcome = match outcome {
            Ok(None) => "served".to_string(),
            Ok(Some(note)) => note.clone(),
            Err(error) => format!("error: {error}"),
        };
        self.record.insert("outcome".into(), json!(outcome));
        self.record
            .insert("after".into(), entries(list, project_root));
        let line = Value::Object(self.record).to_string();
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(error) = written {
            crate::slog_warn!("rerank pool export {}: {error}", self.path.display());
        }
    }
}
