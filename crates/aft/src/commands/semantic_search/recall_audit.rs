//! Benchmark-only recall audit for engine-ranked `aft_search` replies.
//!
//! A search can lose a known answer at several different stages, and each
//! stage calls for a different fix: the file was never indexed, no lane
//! produced it, a lane produced it beyond that lane's candidate limit, it
//! reached the ranked list but below the returned page, or its file was shown
//! with a different line span because only one row per file survives. The
//! public reply only shows the page, so none of those stages can be told apart
//! from outside.
//!
//! Setting `AFT_SEARCH_RECALL_AUDIT=1` in the aft process environment makes
//! every engine-ranked reply carry a `recall_audit` object with each lane's
//! complete candidate order and the whole canonical list. When
//! `AFT_SEARCH_RECALL_AUDIT_TARGETS` names a JSON file holding an array of
//! project-relative paths, the audit also reports, for each of those files,
//! whether the trigram index and the semantic store hold it and where the
//! lexical and semantic lanes would rank it with no candidate limit.
//!
//! This is deliberately an environment switch and not a tool parameter: an
//! agent cannot ask for it, and the shared daemon never sets it. Everything
//! here runs after the ranking and the confidence label have been computed and
//! only reads their results, so enabling it cannot change what a search
//! returns. The one observable side effect is in the engine-only route (plans
//! without a semantic lane), where the audit embeds the query once to report
//! where semantic retrieval would have ranked the targets; that embedding is
//! counted in the reply's embedding telemetry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::blocks::CanonicalList;
use super::comparator::CandidateResult;
use super::evidence_descriptor::EvidenceTier;
use super::extensions::LanePlan;
use crate::search_index::SearchIndexSnapshot;
use crate::semantic_index::SemanticResult;

pub(super) const ENV_ENABLE: &str = "AFT_SEARCH_RECALL_AUDIT";
pub(super) const ENV_TARGETS: &str = "AFT_SEARCH_RECALL_AUDIT_TARGETS";

pub(super) fn enabled() -> bool {
    std::env::var(ENV_ENABLE).is_ok_and(|value| value == "1")
}

/// Project-relative target paths, or the reason they could not be read. A
/// malformed target file is reported in the audit rather than treated as "no
/// targets", because an empty coverage table would read as a clean result.
fn targets() -> Result<Vec<String>, String> {
    let Ok(path) = std::env::var(ENV_TARGETS) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
    serde_json::from_str::<Vec<String>>(&text).map_err(|error| format!("{path}: {error}"))
}

/// Renders a result path relative to the project root. The root the caller
/// configured and the canonical root can differ (macOS temporary directories
/// live under both `/var` and `/private/var`), so both are tried.
pub(super) fn display_path(path: &Path, project_root: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(project_root) {
        return relative.to_string_lossy().replace('\\', "/");
    }
    if let Ok(canonical_root) = std::fs::canonicalize(project_root) {
        if let Ok(relative) = path.strip_prefix(&canonical_root) {
            return relative.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string_lossy().replace('\\', "/")
}

fn target_candidates(project_root: &Path, target: &str) -> Vec<PathBuf> {
    let mut paths = vec![project_root.join(target)];
    if let Ok(canonical_root) = std::fs::canonicalize(project_root) {
        let canonical = canonical_root.join(target);
        if !paths.contains(&canonical) {
            paths.push(canonical);
        }
    }
    paths
}

/// One semantic chunk as the lane saw it, kept before the per-file collapse
/// that lets only a file's best chunk into the ranking.
#[derive(Debug, Clone)]
pub(super) struct AuditChunk {
    pub file: PathBuf,
    pub name: String,
    pub kind: &'static str,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
}

impl AuditChunk {
    pub(super) fn from_result(result: &SemanticResult) -> Self {
        Self {
            file: result.file.clone(),
            name: result.name.clone(),
            kind: super::symbol_kind_label(&result.kind),
            start_line: result.start_line,
            end_line: result.end_line,
            score: result.score,
        }
    }

    fn to_json(&self, project_root: &Path, rank: usize) -> Value {
        json!({
            "rank": rank,
            "file": display_path(&self.file, project_root),
            "name": self.name,
            "kind": self.kind,
            // Semantic chunk lines are zero-based internally; the public reply
            // adds one, so the audit does the same to stay comparable with it.
            "start_line": super::display_line_number(self.start_line),
            "end_line": super::display_line_number(self.end_line),
            "score": self.score,
        })
    }
}

/// Everything the engine ranking already computed that the audit reports.
pub(super) struct EngineAuditInput<'a> {
    pub project_root: &'a Path,
    pub plan: &'a LanePlan<'a>,
    pub snapshot: &'a SearchIndexSnapshot,
    pub query_trigrams: &'a [u32],
    pub candidate_filter: &'a dyn Fn(&Path) -> bool,
    pub lexical_order: Vec<PathBuf>,
    pub lexical_pool_size: usize,
    pub exact_candidates: &'a [CandidateResult],
    pub semantic_chunks: Option<Vec<AuditChunk>>,
    pub path_lookup_candidates: &'a [CandidateResult],
    pub canonical_list: &'a CanonicalList,
    pub page_len: usize,
    pub retrieval_depth: usize,
    pub lanes_exhausted: bool,
}

pub(super) fn engine_audit(input: EngineAuditInput<'_>) -> Value {
    let root = input.project_root;
    let lanes_run = input
        .plan
        .selected_lanes
        .iter()
        .map(|lane| lane.as_str())
        .collect::<Vec<_>>();
    let exact = input
        .exact_candidates
        .iter()
        .enumerate()
        .map(|(position, candidate)| {
            json!({
                "rank": position + 1,
                "file": display_path(&candidate.path, root),
                "kind": candidate.evidence.kind,
            })
        })
        .collect::<Vec<_>>();
    let lexical = input
        .lexical_order
        .iter()
        .map(|path| Value::String(display_path(path, root)))
        .collect::<Vec<_>>();
    let semantic_chunks = input.semantic_chunks.as_ref().map(|chunks| {
        chunks
            .iter()
            .enumerate()
            .map(|(position, chunk)| chunk.to_json(root, position + 1))
            .collect::<Vec<_>>()
    });
    let path_lookup = input
        .path_lookup_candidates
        .iter()
        .map(|candidate| Value::String(display_path(&candidate.path, root)))
        .collect::<Vec<_>>();
    let canonical = input
        .canonical_list
        .entries()
        .enumerate()
        .map(|(position, entry)| {
            json!({
                "rank": position + 1,
                "file": display_path(&entry.result.path, root),
                "tier": match entry.result.evidence.tier {
                    EvidenceTier::Exact => "exact",
                    EvidenceTier::NonExact => "non_exact",
                },
                "evidence_kind": entry.result.evidence.kind,
                "block": entry.tier_index,
                "best_lane": entry.result.best_lane.map(|lane| lane.as_str()),
                "lanes": entry
                    .lane_attribution
                    .iter()
                    .map(|attribution| json!({
                        "lane": attribution.lane.as_str(),
                        "position": attribution.position + 1,
                        "disposition": attribution.disposition,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();

    let mut audit = Map::new();
    audit.insert("schema".into(), json!("aft-search-recall-audit-v1"));
    audit.insert("shape".into(), json!(input.plan.shape));
    audit.insert("lanes_run".into(), json!(lanes_run));
    audit.insert(
        "limits".into(),
        json!({
            "semantic_enumeration_limit": super::SEMANTIC_ENUMERATION_LIMIT,
            "lexical_max_depth": super::lexical_lane::LEXICAL_MAX_DEPTH,
            "lexical_exact_verification_limit": super::lexical_lane::LEXICAL_ENUMERATION_LIMIT,
            "block_depths": super::blocks::BLOCK_DEPTHS,
        }),
    );
    audit.insert(
        "lanes".into(),
        json!({
            "exact": exact,
            "lexical": lexical,
            "lexical_pool_size": input.lexical_pool_size,
            "semantic_chunks": semantic_chunks,
            "path_lookup": path_lookup,
        }),
    );
    audit.insert("canonical_list".into(), Value::Array(canonical));
    audit.insert("page_len".into(), json!(input.page_len));
    audit.insert("retrieval_depth".into(), json!(input.retrieval_depth));
    audit.insert("lanes_exhausted".into(), json!(input.lanes_exhausted));

    match targets() {
        Ok(targets) if !targets.is_empty() => {
            let unpooled = unpooled_lexical_ranks(&input);
            let coverage = targets
                .iter()
                .map(|target| {
                    let candidates = target_candidates(root, target);
                    let index_state = candidates
                        .iter()
                        .map(|path| input.snapshot.audit_index_state(path))
                        .find(|state| *state != "absent")
                        .unwrap_or("absent");
                    let lexical_rank = candidates
                        .iter()
                        .find_map(|path| unpooled.get(path).copied());
                    (
                        target.clone(),
                        json!({
                            "trigram_index": index_state,
                            "lexical_unpooled_rank": lexical_rank,
                        }),
                    )
                })
                .collect::<Map<_, _>>();
            audit.insert("targets".into(), Value::Object(coverage));
            audit.insert("lexical_unpooled_count".into(), json!(unpooled.len()));
        }
        Ok(_) => {}
        Err(error) => {
            audit.insert("targets_error".into(), json!(error));
        }
    }
    Value::Object(audit)
}

/// Rank of every file that contains at least one query trigram, scored the way
/// the lexical lane scores but without the lane's discovery pool (files holding
/// one of the three rarest query trigrams). A target with a rank here and no
/// place in the lane's own order was cut by that pool, not by its score.
fn unpooled_lexical_ranks(input: &EngineAuditInput<'_>) -> HashMap<PathBuf, usize> {
    let mut unique = Vec::with_capacity(input.query_trigrams.len());
    for trigram in input.query_trigrams {
        if !unique.contains(trigram) {
            unique.push(*trigram);
        }
    }
    input
        .snapshot
        .lexical_rank_at_depth(&unique, Some(input.candidate_filter), usize::MAX)
        .files
        .into_iter()
        .enumerate()
        .map(|(position, (path, _))| (path, position + 1))
        .collect()
}

/// Adds each target file's semantic chunks, ranked against the whole semantic
/// store with no enumeration limit, to an audit built by [`engine_audit`].
/// `full_ranking` is `None` when no query vector could be produced; the audit
/// then says so instead of reporting the targets as absent from the store.
pub(super) fn attach_semantic_coverage(
    audit: &mut Value,
    project_root: &Path,
    full_ranking: Result<Vec<AuditChunk>, String>,
    semantic_ran: bool,
) {
    let Some(object) = audit.as_object_mut() else {
        return;
    };
    object.insert("semantic_ran".into(), json!(semantic_ran));
    let chunks = match full_ranking {
        Ok(chunks) => chunks,
        Err(error) => {
            object.insert("semantic_coverage_error".into(), json!(error));
            return;
        }
    };
    object.insert("semantic_store_ranked_chunks".into(), json!(chunks.len()));
    let Some(targets) = object.get_mut("targets").and_then(Value::as_object_mut) else {
        return;
    };
    for (target, coverage) in targets.iter_mut() {
        let wanted = target_candidates(project_root, target);
        let target_chunks = chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| wanted.iter().any(|path| *path == chunk.file))
            .map(|(position, chunk)| chunk.to_json(project_root, position + 1))
            .collect::<Vec<_>>();
        if let Some(coverage) = coverage.as_object_mut() {
            coverage.insert("semantic_chunks".into(), Value::Array(target_chunks));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_path_is_relative_to_the_project_root() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            display_path(Path::new("/tmp/project/src/lib.rs"), root),
            "src/lib.rs"
        );
        assert_eq!(
            display_path(Path::new("/elsewhere/lib.rs"), root),
            "/elsewhere/lib.rs"
        );
    }

    #[test]
    fn semantic_coverage_ranks_target_chunks_against_the_whole_store() {
        let root = Path::new("/tmp/project");
        let chunk = |file: &str, name: &str| AuditChunk {
            file: root.join(file),
            name: name.to_string(),
            kind: "function",
            start_line: 0,
            end_line: 4,
            score: 0.5,
        };
        let mut audit = json!({"targets": {"src/b.rs": {}, "src/missing.rs": {}}});
        attach_semantic_coverage(
            &mut audit,
            root,
            Ok(vec![
                chunk("src/a.rs", "a"),
                chunk("src/b.rs", "b1"),
                chunk("src/b.rs", "b2"),
            ]),
            true,
        );
        let b = &audit["targets"]["src/b.rs"]["semantic_chunks"];
        assert_eq!(b[0]["rank"], 2);
        assert_eq!(b[1]["rank"], 3);
        assert_eq!(b[0]["start_line"], 1);
        assert_eq!(
            audit["targets"]["src/missing.rs"]["semantic_chunks"],
            json!([])
        );
        assert_eq!(audit["semantic_store_ranked_chunks"], 3);
    }

    #[test]
    fn missing_query_vector_is_reported_not_treated_as_absent() {
        let mut audit = json!({"targets": {"src/b.rs": {}}});
        attach_semantic_coverage(
            &mut audit,
            Path::new("/tmp/project"),
            Err("semantic index not ready".into()),
            false,
        );
        assert_eq!(audit["semantic_coverage_error"], "semantic index not ready");
        assert!(audit["targets"]["src/b.rs"]
            .get("semantic_chunks")
            .is_none());
    }
}
