use crate::commands::semantic_search::extensions::{ExactMode, LanePlan, QueryFacts, Readiness};
use crate::commands::semantic_search::plan_table::{SearchLaneKind, SearchShape};

/// Builds the legal row and then removes only lanes gated by unavailable resources.
pub fn plan<'a>(
    shape: &SearchShape,
    facts: &QueryFacts,
    readiness: &Readiness<'a>,
) -> LanePlan<'a> {
    let legal = legal_lanes(*shape, facts);
    let mut selected_lanes = legal
        .iter()
        .copied()
        .filter(|lane| lane_is_ready(*lane, readiness))
        .collect::<Vec<_>>();

    if selected_lanes.is_empty() {
        selected_lanes.push(SearchLaneKind::FallbackWalk);
    }

    let mut executed_callbacks = selected_lanes.clone();
    if readiness_disclosure_required(&legal, readiness) {
        executed_callbacks.push(SearchLaneKind::ReadinessDisclosure);
    }

    let exact_mode = if !legal.contains(&SearchLaneKind::Exact) {
        ExactMode::NotApplicable
    } else if readiness.lexical_index && facts.exact_input_tokens > 0 {
        ExactMode::Ready
    } else {
        ExactMode::Fallback
    };

    LanePlan {
        shape: *shape,
        query_facts: facts.clone(),
        exact_input: None,
        exact_mode,
        selected_lanes,
        executed_callbacks,
        readiness: readiness.clone(),
        variants: Vec::new(),
    }
}

/// Returns the all-ready retrieval row before resource gating.
///
/// Chair ruling R2a makes exact evidence shape-independent for every non-regex
/// query. The exact lane's score-free, admission-exempt result can lead only
/// when it finds the verbatim phrase, so selecting it cannot displace semantic
/// ranking when no exact evidence exists. Chair ruling R1a also treats a path
/// fact as lane-selection evidence without changing the token-count-driven
/// shape; a runtime-resolved path lookup runs before exact evidence. Chair
/// ruling R1b selects symbol lookup for natural-language queries only when the
/// classifier found an identifier-shaped token, leaving ordinary prose
/// unchanged. Regex remains on its pinned owner.
pub fn legal_lanes(shape: SearchShape, facts: &QueryFacts) -> Vec<SearchLaneKind> {
    let mut lanes = match shape {
        SearchShape::Identifier => vec![
            SearchLaneKind::Symbol,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
        ],
        SearchShape::Short => vec![
            SearchLaneKind::Symbol,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
        ],
        SearchShape::CodeLiteral => vec![SearchLaneKind::Exact, SearchLaneKind::Lexical],
        SearchShape::NaturalLanguage => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
        ],
        SearchShape::LogExcerpt => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Anchored,
            SearchLaneKind::Lexical,
        ],
        SearchShape::Path => vec![
            SearchLaneKind::PathLookup,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
        ],
        SearchShape::Regex => vec![SearchLaneKind::FallbackWalk],
    };

    if shape == SearchShape::NaturalLanguage
        && facts.has_identifier_token
        && !lanes.contains(&SearchLaneKind::Symbol)
    {
        lanes.insert(0, SearchLaneKind::Symbol);
    }
    if shape != SearchShape::Regex
        && facts.has_path_token
        && !lanes.contains(&SearchLaneKind::PathLookup)
    {
        let before_exact = lanes
            .iter()
            .position(|lane| *lane == SearchLaneKind::Exact)
            .unwrap_or(0);
        lanes.insert(before_exact, SearchLaneKind::PathLookup);
    }
    lanes
}

fn lane_is_ready(lane: SearchLaneKind, readiness: &Readiness<'_>) -> bool {
    match lane {
        SearchLaneKind::Symbol => readiness.symbol_index,
        SearchLaneKind::Lexical | SearchLaneKind::Variants => readiness.lexical_index,
        SearchLaneKind::Semantic => readiness.semantic_index,
        SearchLaneKind::Exact
        | SearchLaneKind::Anchored
        | SearchLaneKind::PathLookup
        | SearchLaneKind::FallbackWalk
        | SearchLaneKind::ReadinessDisclosure => true,
    }
}

fn readiness_disclosure_required(legal: &[SearchLaneKind], readiness: &Readiness<'_>) -> bool {
    (!readiness.symbol_index && legal.contains(&SearchLaneKind::Symbol))
        || (!readiness.lexical_index
            && legal
                .iter()
                .any(|lane| matches!(lane, SearchLaneKind::Lexical | SearchLaneKind::Variants)))
        || (!readiness.semantic_index && legal.contains(&SearchLaneKind::Semantic))
}
