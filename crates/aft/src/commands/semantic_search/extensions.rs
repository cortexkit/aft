use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLockReadGuard};

use serde::Serialize;

use super::plan_table::{SearchLaneKind, SearchShape};
use super::{LaneExecution, LaneInput, SearchLane};
use crate::context::SemanticIndexStatus;
use crate::parser::SymbolCache;
use crate::query_shape::{classify, looks_like_regex, QueryKind};
use crate::search_index::{IndexStatus, SearchIndexSnapshot};
use crate::semantic_index::SemanticIndex;

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

/// Result of waiting for the first search index to become ready, with a time bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessWait {
    Completed,
    Cancelled,
}

/// Symbol-cache lifecycle state for the selected root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolIndexStatus {
    Ready,
    Building,
    Disabled,
    Failed(String),
}

/// A semantic index retained without copying its embedding storage.
#[derive(Clone)]
pub struct SemanticSnapshot<'a>(Arc<RwLockReadGuard<'a, Option<SemanticIndex>>>);

impl std::fmt::Debug for SemanticSnapshot<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SemanticSnapshot")
            .field("present", &self.index().is_some())
            .finish()
    }
}

impl<'a> SemanticSnapshot<'a> {
    pub fn from_guard(guard: RwLockReadGuard<'a, Option<SemanticIndex>>) -> Self {
        Self(Arc::new(guard))
    }

    pub fn index(&self) -> Option<&SemanticIndex> {
        self.0.as_ref().as_ref()
    }
}

/// One semantic-index observation and the resource that made it queryable.
#[derive(Debug, Clone)]
pub struct SemanticReadiness<'a> {
    pub status: SemanticIndexStatus,
    pub snapshot: Option<SemanticSnapshot<'a>>,
    pub evicted: bool,
    pub lock_contended: bool,
}

/// One trigram-index observation and the resource that made it queryable.
#[derive(Debug, Clone)]
pub struct TrigramReadiness {
    pub status: IndexStatus,
    pub snapshot: Option<Arc<SearchIndexSnapshot>>,
    pub evicted: bool,
    pub lock_contended: bool,
}

/// One symbol-cache observation and the resource that made it queryable.
#[derive(Clone)]
pub struct SymbolReadiness {
    pub status: SymbolIndexStatus,
    pub snapshot: Option<Arc<SymbolCache>>,
    pub evicted: bool,
    pub lock_contended: bool,
}

impl std::fmt::Debug for SymbolReadiness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SymbolReadiness")
            .field("status", &self.status)
            .field("has_snapshot", &self.snapshot.is_some())
            .field("evicted", &self.evicted)
            .field("lock_contended", &self.lock_contended)
            .finish()
    }
}

/// The three real runtime states captured in one admission sample.
#[derive(Debug, Clone)]
pub struct ReadinessObservation<'a> {
    pub semantic: SemanticReadiness<'a>,
    pub trigram: TrigramReadiness,
    pub symbol: SymbolReadiness,
}

/// Supplies root-scoped runtime state to the extension seam.
///
/// Implementations return retained handles so a selected plan keeps using the
/// exact resources it admitted even if the context evicts its live pointers.
pub trait ReadinessSource {
    fn sample<'a>(&'a self) -> ReadinessObservation<'a>;
    fn bounded_first_search_wait(&self) -> ReadinessWait;
}

/// Input accepted by [`Root::new`]. The fixed form keeps source compatibility
/// for older callers while public search samples from a runtime source.
#[doc(hidden)]
pub enum RootReadiness<'a> {
    Source(&'a dyn ReadinessSource),
    Fixed(Readiness<'a>),
}

#[doc(hidden)]
pub trait IntoRootReadiness<'a> {
    fn into_root_readiness(self) -> RootReadiness<'a>;
}

impl<'a> IntoRootReadiness<'a> for &'a dyn ReadinessSource {
    fn into_root_readiness(self) -> RootReadiness<'a> {
        RootReadiness::Source(self)
    }
}

impl<'a> IntoRootReadiness<'a> for Readiness<'a> {
    fn into_root_readiness(self) -> RootReadiness<'a> {
        RootReadiness::Fixed(self)
    }
}

/// Root-scoped resource source sampled before a lane plan is built.
pub struct Root<'a> {
    path: PathBuf,
    readiness: RootReadiness<'a>,
}

impl std::fmt::Debug for Root<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Root")
            .field("path", &self.path)
            .finish()
    }
}

impl<'a> Root<'a> {
    pub fn new(path: impl Into<PathBuf>, readiness: impl IntoRootReadiness<'a>) -> Self {
        Self {
            path: path.into(),
            readiness: readiness.into_root_readiness(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source(&self) -> Option<&'a dyn ReadinessSource> {
        match &self.readiness {
            RootReadiness::Source(source) => Some(*source),
            RootReadiness::Fixed(_) => None,
        }
    }

    pub fn fixed_readiness(&self) -> Option<&Readiness<'a>> {
        match &self.readiness {
            RootReadiness::Source(_) => None,
            RootReadiness::Fixed(readiness) => Some(readiness),
        }
    }
}

#[derive(Clone, Default)]
pub struct RetainedReadinessSnapshots<'a> {
    semantic: Option<SemanticSnapshot<'a>>,
    trigram: Option<Arc<SearchIndexSnapshot>>,
    symbol: Option<Arc<SymbolCache>>,
}

impl std::fmt::Debug for RetainedReadinessSnapshots<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedReadinessSnapshots")
            .field("semantic", &self.semantic.is_some())
            .field("trigram", &self.trigram.is_some())
            .field("symbol", &self.symbol.is_some())
            .finish()
    }
}

impl<'a> RetainedReadinessSnapshots<'a> {
    pub fn new(
        semantic: Option<SemanticSnapshot<'a>>,
        trigram: Option<Arc<SearchIndexSnapshot>>,
        symbol: Option<Arc<SymbolCache>>,
    ) -> Self {
        Self {
            semantic,
            trigram,
            symbol,
        }
    }

    pub fn semantic(&self) -> Option<&SemanticIndex> {
        self.semantic.as_ref().and_then(SemanticSnapshot::index)
    }

    pub fn trigram(&self) -> Option<&SearchIndexSnapshot> {
        self.trigram.as_deref()
    }

    pub fn symbol(&self) -> Option<&SymbolCache> {
        self.symbol.as_deref()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Readiness<'a> {
    pub symbol_index: bool,
    pub lexical_index: bool,
    pub semantic_index: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(skip)]
    retained: RetainedReadinessSnapshots<'a>,
    #[serde(skip)]
    cancelled: bool,
}

impl PartialEq for Readiness<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.symbol_index == other.symbol_index
            && self.lexical_index == other.lexical_index
            && self.semantic_index == other.semantic_index
            && self.reasons == other.reasons
            && self.cancelled == other.cancelled
    }
}

impl Eq for Readiness<'_> {}

impl<'a> Readiness<'a> {
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
        Self::observed(
            symbol_index,
            lexical_index,
            semantic_index,
            reasons,
            RetainedReadinessSnapshots::default(),
            false,
        )
    }

    pub fn observed(
        symbol_index: bool,
        lexical_index: bool,
        semantic_index: bool,
        reasons: Vec<String>,
        retained: RetainedReadinessSnapshots<'a>,
        cancelled: bool,
    ) -> Self {
        Self {
            symbol_index,
            lexical_index,
            semantic_index,
            reasons,
            retained,
            cancelled,
        }
    }

    pub fn retained(&self) -> &RetainedReadinessSnapshots<'a> {
        &self.retained
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanePlan<'a> {
    pub shape: SearchShape,
    pub selected_lanes: Vec<SearchLaneKind>,
    pub readiness: Readiness<'a>,
    pub variants: Vec<String>,
}

impl LanePlan<'_> {
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

    fn sample_readiness<'a>(&self, root: &Root<'a>) -> Readiness<'a> {
        crate::search_b2::readiness::sample(root)
    }

    fn plan<'a>(
        &self,
        _facts: &QueryFacts,
        shape: SearchShape,
        readiness: &Readiness<'a>,
    ) -> LanePlan<'a> {
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

fn default_lane_plan<'a>(shape: SearchShape, readiness: Readiness<'a>) -> LanePlan<'a> {
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
