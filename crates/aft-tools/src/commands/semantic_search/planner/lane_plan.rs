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
///
/// `query` is prose: `pattern` carries exact names, text, regexes and
/// literals, so every shape the router gives `query` keeps the semantic lane.
/// A code-looking query (an identifier, a quoted literal, a log line, a path)
/// adds the exact, lexical and lookup lanes its shape calls for, and those
/// lanes plus the plan table's lexical weight keep exact-name hits strong;
/// the shape never drops the semantic lane. Regex is the one exception: a
/// query the router reads as a regex is searched as a regex on the grep
/// route, with no prose to embed.
pub fn legal_lanes(shape: SearchShape, facts: &QueryFacts) -> Vec<SearchLaneKind> {
    let mut lanes = match shape {
        SearchShape::Identifier => vec![
            SearchLaneKind::Symbol,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
        ],
        SearchShape::Short => vec![
            SearchLaneKind::Symbol,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
        ],
        SearchShape::CodeLiteral => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
        ],
        SearchShape::NaturalLanguage => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
        ],
        SearchShape::LogExcerpt => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Anchored,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
        ],
        SearchShape::Path => vec![
            SearchLaneKind::PathLookup,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
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

/// Lane plan for a request that supplies both `query` and `pattern`.
///
/// No router runs. The prose feeds only the lexical and semantic lanes
/// (whichever are ready), and the pattern feeds the two scored pattern lanes.
/// No exact, symbol, variants or path-lookup lane runs on the prose: the
/// agent has already put the names it wants matched verbatim in `pattern`,
/// and any of those lanes would place prose-derived files in the exact tier
/// ahead of every pattern and semantic result. The shape is recorded as
/// natural language for telemetry only; scoring comes from `split_query`.
pub fn split_plan<'a>(readiness: &Readiness<'a>) -> LanePlan<'a> {
    let legal = [
        SearchLaneKind::Lexical,
        SearchLaneKind::Semantic,
        SearchLaneKind::PatternDefinition,
        SearchLaneKind::PatternMention,
    ];
    let selected_lanes = legal
        .iter()
        .copied()
        .filter(|lane| lane_is_ready(*lane, readiness))
        .collect::<Vec<_>>();
    let mut executed_callbacks = selected_lanes.clone();
    if readiness_disclosure_required(&legal, readiness) {
        executed_callbacks.push(SearchLaneKind::ReadinessDisclosure);
    }
    LanePlan {
        shape: SearchShape::NaturalLanguage,
        query_facts: no_query_facts(),
        exact_input: None,
        exact_mode: ExactMode::NotApplicable,
        selected_lanes,
        executed_callbacks,
        readiness: readiness.clone(),
        variants: Vec::new(),
    }
}

/// Query facts for input the router did not read: an explicit `pattern`, or
/// the prose of a split request.
pub fn no_query_facts() -> QueryFacts {
    QueryFacts {
        embedded_span: None,
        exact_input_tokens: 0,
        has_path_token: false,
        has_timestamp_or_pid: false,
        has_identifier_token: false,
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
        | SearchLaneKind::ReadinessDisclosure
        | SearchLaneKind::PatternDefinition
        | SearchLaneKind::PatternMention => true,
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
