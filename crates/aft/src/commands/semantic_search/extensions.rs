use std::path::{Path, PathBuf};

use serde::Serialize;

use super::plan_table::{SearchLaneKind, SearchShape};
use super::{LaneExecution, LaneInput, SearchLane};
use crate::query_shape::{classify, looks_like_regex, QueryKind};

/// Original request facts captured before any lane-specific query normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFacts {
    original_query: String,
}

impl QueryFacts {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            original_query: query.into(),
        }
    }

    pub fn original_query(&self) -> &str {
        &self.original_query
    }

    pub fn tokens(&self) -> impl Iterator<Item = Token<'_>> {
        self.original_query
            .split_whitespace()
            .enumerate()
            .map(|(index, text)| Token { index, text })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub index: usize,
    pub text: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenVariant {
    pub token_index: usize,
    pub text: String,
}

/// Root-scoped resource state sampled once before a lane plan is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    path: PathBuf,
    readiness: Readiness,
}

impl Root {
    pub fn new(path: impl Into<PathBuf>, readiness: Readiness) -> Self {
        Self {
            path: path.into(),
            readiness,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn readiness(&self) -> &Readiness {
        &self.readiness
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Readiness {
    pub symbol_index: bool,
    pub lexical_index: bool,
    pub semantic_index: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

impl Readiness {
    pub fn new(symbol_index: bool, lexical_index: bool, semantic_index: bool) -> Self {
        let mut reasons = Vec::new();
        if !symbol_index {
            reasons.push("symbol_index_unavailable".to_string());
        }
        if !lexical_index {
            reasons.push("lexical_index_unavailable".to_string());
        }
        if !semantic_index {
            reasons.push("semantic_index_unavailable".to_string());
        }
        Self {
            symbol_index,
            lexical_index,
            semantic_index,
            reasons,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanePlan {
    pub shape: SearchShape,
    pub selected_lanes: Vec<SearchLaneKind>,
    pub readiness: Readiness,
    pub variants: Vec<String>,
}

impl LanePlan {
    pub fn contains(&self, lane: SearchLaneKind) -> bool {
        self.selected_lanes.contains(&lane)
    }
}

/// A-side extension seam. Later ranking campaigns can override one hook at a
/// time while the base engine remains independently buildable.
pub trait SearchExtensions: Send + Sync {
    fn classify(&self, facts: &QueryFacts) -> SearchShape {
        classify_query_facts(facts)
    }

    fn variants(&self, _token: Token<'_>) -> Vec<TokenVariant> {
        Vec::new()
    }

    fn sample_readiness(&self, root: &Root) -> Readiness {
        root.readiness().clone()
    }

    fn plan(
        &self,
        _facts: &QueryFacts,
        shape: SearchShape,
        readiness: &Readiness,
    ) -> LanePlan {
        default_lane_plan(shape, readiness.clone())
    }

    fn execute_lane(&self, lane: &dyn SearchLane, input: &LaneInput<'_>) -> LaneExecution {
        lane.execute(input)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultSearchExtensions;

impl SearchExtensions for DefaultSearchExtensions {}

pub fn classify_query_facts(facts: &QueryFacts) -> SearchShape {
    let query = facts.original_query().trim();
    if looks_like_regex(query) {
        return SearchShape::Regex;
    }
    if is_quoted(query) {
        return SearchShape::CodeLiteral;
    }

    let query_shape = classify(query);
    if query_shape.kind == QueryKind::ErrorCode || looks_like_log_excerpt(query) {
        return SearchShape::LogExcerpt;
    }
    if query_shape.kind == QueryKind::Path {
        return SearchShape::Path;
    }
    if query_shape.kind == QueryKind::Identifier {
        return SearchShape::Identifier;
    }
    if query.split_whitespace().count() <= 2 {
        return SearchShape::Short;
    }
    SearchShape::NaturalLanguage
}

fn default_lane_plan(shape: SearchShape, readiness: Readiness) -> LanePlan {
    let mut selected_lanes = match shape {
        SearchShape::Identifier => vec![
            SearchLaneKind::Symbol,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
        ],
        SearchShape::CodeLiteral => vec![SearchLaneKind::Exact, SearchLaneKind::Lexical],
        SearchShape::Short => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
        ],
        SearchShape::NaturalLanguage => vec![
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::Variants,
            SearchLaneKind::Semantic,
        ],
        SearchShape::LogExcerpt => vec![
            SearchLaneKind::Anchored,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
        ],
        SearchShape::Path => vec![
            SearchLaneKind::PathLookup,
            SearchLaneKind::Exact,
            SearchLaneKind::Lexical,
            SearchLaneKind::FallbackWalk,
        ],
        SearchShape::Regex => vec![SearchLaneKind::FallbackWalk],
    };

    if !readiness.symbol_index {
        selected_lanes.retain(|lane| *lane != SearchLaneKind::Symbol);
    }
    if !readiness.lexical_index {
        selected_lanes.retain(|lane| {
            !matches!(
                lane,
                SearchLaneKind::Exact | SearchLaneKind::Anchored | SearchLaneKind::Lexical
            )
        });
    }
    if !readiness.semantic_index {
        selected_lanes.retain(|lane| *lane != SearchLaneKind::Semantic);
    }
    if !readiness.reasons.is_empty() {
        selected_lanes.push(SearchLaneKind::ReadinessDisclosure);
    }

    LanePlan {
        shape,
        selected_lanes,
        readiness,
        variants: Vec::new(),
    }
}

fn is_quoted(query: &str) -> bool {
    query.len() >= 2
        && ((query.starts_with('"') && query.ends_with('"'))
            || (query.starts_with('\'') && query.ends_with('\'')))
}

fn looks_like_log_excerpt(query: &str) -> bool {
    let upper = query.to_ascii_uppercase();
    [" ERROR ", " WARN ", " INFO ", " DEBUG ", " TRACE "]
        .iter()
        .any(|marker| upper.contains(marker))
        || query.contains("::") && query.contains('[') && query.contains(']')
}
