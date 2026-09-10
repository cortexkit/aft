use crate::commands::semantic_search::extensions::{
    Readiness, ReadinessObservation, ReadinessWait, RetainedReadinessSnapshots, Root,
    SemanticReadiness, SymbolIndexStatus, SymbolReadiness, TrigramReadiness,
};
use crate::commands::semantic_search::plan_table::SearchLaneKind;
use crate::context::SemanticIndexStatus;
use crate::search_index::IndexStatus;

const SEMANTIC_REASON: &str = "semantic:";
const TRIGRAM_REASON: &str = "trigram:";
const SYMBOL_REASON: &str = "symbol:";

/// Samples all sources at admission and retains the resources selected by that sample.
///
/// The expensive first-search wait is used only when the first observation has no
/// ready source. A completed wait is followed by exactly one new observation, and
/// only that second observation is allowed to select the plan.
pub fn sample<'a>(root: &Root<'a>) -> Readiness<'a> {
    if let Some(readiness) = root.fixed_readiness() {
        return readiness.clone();
    }
    let source = root
        .source()
        .expect("a non-fixed readiness root carries a runtime source");
    let first = sample_once(source.sample());
    if first.semantic_index || first.lexical_index || first.symbol_index {
        return first;
    }

    match source.bounded_first_search_wait() {
        ReadinessWait::Completed => sample_once(source.sample()),
        ReadinessWait::Cancelled => Readiness::observed(
            first.symbol_index,
            first.lexical_index,
            first.semantic_index,
            first.reasons.clone(),
            first.retained().clone(),
            true,
        ),
    }
}

fn sample_once<'a>(observation: ReadinessObservation<'a>) -> Readiness<'a> {
    let semantic_ready = semantic_ready(&observation.semantic);
    let trigram_ready = trigram_ready(&observation.trigram);
    let symbol_ready = symbol_ready(&observation.symbol);

    let mut reasons = Vec::new();
    if !trigram_ready {
        reasons.push(format!(
            "{TRIGRAM_REASON}{}",
            trigram_reason(&observation.trigram)
        ));
    }
    if !symbol_ready {
        reasons.push(format!(
            "{SYMBOL_REASON}{}",
            symbol_reason(&observation.symbol)
        ));
    }
    if !semantic_ready {
        reasons.push(format!(
            "{SEMANTIC_REASON}{}",
            semantic_reason(&observation.semantic)
        ));
    }

    let retained = RetainedReadinessSnapshots::new(
        semantic_ready
            .then(|| observation.semantic.snapshot)
            .flatten(),
        trigram_ready
            .then(|| observation.trigram.snapshot)
            .flatten(),
        symbol_ready.then(|| observation.symbol.snapshot).flatten(),
    );
    Readiness::observed(
        symbol_ready,
        trigram_ready,
        semantic_ready,
        reasons,
        retained,
        false,
    )
}

fn semantic_ready(state: &SemanticReadiness<'_>) -> bool {
    matches!(state.status, SemanticIndexStatus::Ready { .. })
        && state.snapshot.is_some()
        && !state.evicted
        && !state.lock_contended
}

fn trigram_ready(state: &TrigramReadiness) -> bool {
    state.status == IndexStatus::Ready
        && state.snapshot.is_some()
        && !state.evicted
        && !state.lock_contended
}

fn symbol_ready(state: &SymbolReadiness) -> bool {
    state.status == SymbolIndexStatus::Ready
        && state.snapshot.is_some()
        && !state.evicted
        && !state.lock_contended
}

fn semantic_reason(state: &SemanticReadiness<'_>) -> String {
    match &state.status {
        SemanticIndexStatus::Disabled => "disabled".to_string(),
        SemanticIndexStatus::Failed(code) => format!("failed:{code}"),
        _ if state.evicted
            || (matches!(state.status, SemanticIndexStatus::Ready { .. })
                && state.snapshot.is_none()
                && !state.lock_contended) =>
        {
            "evicted".to_string()
        }
        _ if state.lock_contended => "lock_contention".to_string(),
        SemanticIndexStatus::Building { stage, .. } => format!("building:{stage}"),
        SemanticIndexStatus::Ready { .. } => "evicted".to_string(),
    }
}

fn trigram_reason(state: &TrigramReadiness) -> String {
    match state.status {
        IndexStatus::Disabled => "disabled".to_string(),
        IndexStatus::Fallback => "failed:fallback".to_string(),
        _ if state.evicted
            || (state.status == IndexStatus::Ready
                && state.snapshot.is_none()
                && !state.lock_contended) =>
        {
            "evicted".to_string()
        }
        _ if state.lock_contended => "lock_contention".to_string(),
        IndexStatus::Building => "building:trigram_index".to_string(),
        IndexStatus::Ready => "evicted".to_string(),
    }
}

fn symbol_reason(state: &SymbolReadiness) -> String {
    match &state.status {
        SymbolIndexStatus::Disabled => "disabled".to_string(),
        SymbolIndexStatus::Failed(code) => format!("failed:{code}"),
        _ if state.evicted
            || (state.status == SymbolIndexStatus::Ready
                && state.snapshot.is_none()
                && !state.lock_contended) =>
        {
            "evicted".to_string()
        }
        _ if state.lock_contended => "lock_contention".to_string(),
        SymbolIndexStatus::Building => "building:symbol_cache".to_string(),
        SymbolIndexStatus::Ready => "evicted".to_string(),
    }
}

/// Renders disclosure only for unavailable sources used by the legal lane row.
///
/// A trigram outage is shared by lexical and variants, so it always produces one
/// lexical-skipped line rather than a second variants-unavailable line.
pub fn selected_lane_disclosures(
    legal_lanes: &[SearchLaneKind],
    readiness: &Readiness<'_>,
    contributing_variants: &[String],
    fallback_files_scanned: Option<usize>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if !readiness.lexical_index
        && legal_lanes
            .iter()
            .any(|lane| matches!(lane, SearchLaneKind::Lexical | SearchLaneKind::Variants))
    {
        lines.push(format!(
            "trigram index {}; lexical lane skipped",
            source_reason(readiness, TRIGRAM_REASON)
        ));
    }
    if !readiness.symbol_index && legal_lanes.contains(&SearchLaneKind::Symbol) {
        lines.push(format!(
            "symbol cache {}; definition-first skipped",
            source_reason(readiness, SYMBOL_REASON)
        ));
    }
    if !readiness.semantic_index && legal_lanes.contains(&SearchLaneKind::Semantic) {
        lines.push(format!(
            "semantic index {}; semantic lane skipped",
            source_reason(readiness, SEMANTIC_REASON)
        ));
    }
    if !contributing_variants.is_empty() {
        lines.push(format!(
            "variants applied: {}",
            contributing_variants.join(", ")
        ));
    }
    if let Some(files) = fallback_files_scanned {
        lines.push(format!("fallback walk: {files} files scanned"));
    }
    lines
}

fn source_reason<'a>(readiness: &'a Readiness<'_>, prefix: &str) -> &'a str {
    readiness
        .reasons
        .iter()
        .find_map(|reason| reason.strip_prefix(prefix))
        .unwrap_or("disabled")
}
