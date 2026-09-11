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
pub fn legal_lanes(shape: SearchShape, facts: &QueryFacts) -> Vec<SearchLaneKind> {
    match shape {
        SearchShape::Identifier => vec![
            SearchLaneKind::Symbol,
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
        SearchShape::NaturalLanguage => {
            let mut lanes = Vec::with_capacity(3);
            if facts.embedded_span.is_some() {
                lanes.push(SearchLaneKind::Exact);
            }
            lanes.push(SearchLaneKind::Lexical);
            lanes.push(SearchLaneKind::Semantic);
            lanes
        }
        SearchShape::LogExcerpt => vec![SearchLaneKind::Anchored, SearchLaneKind::Lexical],
        SearchShape::Path => vec![SearchLaneKind::PathLookup, SearchLaneKind::Lexical],
        SearchShape::Regex => vec![SearchLaneKind::FallbackWalk],
    }
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
