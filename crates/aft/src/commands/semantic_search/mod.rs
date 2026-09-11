pub mod anchored_lane;
pub mod blocks;
pub mod comparator;
pub mod confidence;
pub mod evidence_descriptor;
pub mod exact_lane;
pub mod extensions;
pub mod generation_token;
pub mod lexical_lane;
pub mod memo;
pub mod paging;
pub mod plan_table;
pub mod provenance;
pub mod scoring;
pub mod telemetry;
pub mod trailer;

pub use comparator::{r3_cmp, score_free_r3_cmp, CandidateResult, RankedTuple, SymbolOffsetRange};
pub use evidence_descriptor::{
    compute_evidence_descriptor, CandidateEvidenceProvider, EvidenceDescriptor, EvidenceKind,
    EvidenceTier,
};
pub use generation_token::GenerationToken;
pub use plan_table::{
    verify_pinned_plan_table_at_startup, LanePlanEntry, PlanTable, PlanTableError, SearchLaneKind,
    SearchShape, PINNED_PLAN_TABLE_JSON,
};

/// Immutable request input passed to a registered lane callback.
pub struct LaneInput<'a> {
    pub query: &'a str,
    pub shape: SearchShape,
    pub root: &'a Path,
    pub include_tests: bool,
    pub index: &'a SearchIndex,
}

#[derive(Debug, Clone)]
pub struct LaneExecution {
    pub kind: SearchLaneKind,
    pub candidates: Vec<CandidateResult>,
}

/// Lane-registration seam: every registration carries its execution callback.
pub trait SearchLane: Send + Sync {
    fn kind(&self) -> SearchLaneKind;
    fn plan_order_index(&self) -> usize {
        self.kind().default_plan_order_index()
    }
    fn execute(&self, _input: &LaneInput<'_>) -> LaneExecution {
        LaneExecution {
            kind: self.kind(),
            candidates: Vec::new(),
        }
    }
}

/// Lane-registration seam: registry holding participating search lanes.
#[derive(Default)]
pub struct LaneRegistry {
    lanes: HashMap<SearchLaneKind, Arc<dyn SearchLane>>,
}

impl LaneRegistry {
    pub fn new() -> Self {
        Self {
            lanes: HashMap::new(),
        }
    }

    pub fn register(&mut self, lane: Arc<dyn SearchLane>) {
        self.lanes.insert(lane.kind(), lane);
    }

    pub fn get(&self, kind: SearchLaneKind) -> Option<&Arc<dyn SearchLane>> {
        self.lanes.get(&kind)
    }

    pub fn registered_kinds(&self) -> Vec<SearchLaneKind> {
        let mut kinds: Vec<_> = self.lanes.keys().copied().collect();
        kinds.sort_by_key(|k| k.default_plan_order_index());
        kinds
    }
}

pub fn register_lane(registry: &mut LaneRegistry, lane: Arc<dyn SearchLane>) {
    registry.register(lane);
}

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

use crate::commands::callgraph_store_adapter::callers_result;
use crate::commands::symbol_render::{
    build_container_outline, might_have_container_members, render_symbol_within_budget,
    BudgetedSymbolRenderStatus,
};
use crate::config::IndexKind;
use crate::context::{AppContext, SemanticIndexStatus};
use crate::grep_executor::{self, GrepParams};
use crate::inspect::job::{is_test_file, is_test_support_file};
use crate::pattern_compile::{self, CompileOpts, CompileResult};
use crate::protocol::{RawRequest, Response};
use crate::query_shape::{self, QueryKind, QueryShape};
use crate::readonly_artifacts::{GitRootResolutionError, ReadOnlyArtifact, ReadOnlyDegradation};
use crate::search_index::{
    sort_grep_matches_by_mtime_desc, try_read_with_budget, walk_project_files_from, GrepMatch,
    GrepPathExclusion, GrepResult, IndexStatus, PathFilters, SearchIndex,
    INTERACTIVE_ARTIFACT_READ_BUDGET,
};
use crate::semantic_index::{
    query_embedding_timeout_budget, strip_query_embedding_timeout_marker, EmbeddingModel,
    QueryBudget, SemanticIndex, SemanticIndexFingerprint, SemanticResult,
};
use crate::symbols::{Range, Symbol, SymbolKind};

const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;
const DEGRADED_GREP_FILE_LIMIT: usize = 1_000;
const DEGRADED_GREP_RESULT_LIMIT: usize = 100;
const DEGRADED_GREP_WALK_BUDGET: Duration = Duration::from_secs(10);
/// Fresh borrowed trigram bases were measured ready 1.1–2.5 seconds after root
/// bind. Waiting through that observed window prevents a healthy first search
/// from returning an empty loading response while keeping a stuck load bounded.
const FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET: Duration = Duration::from_millis(2_500);
const SEARCH_INDEX_LOAD_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(2);
const SUPPRESS_STATUS_BAR_FIELD: &str = "_aft_suppress_status_bar";
const TRIGRAM_BUILDING_BOUNDED_WALK_DISCLOSURE: &str =
    "trigram index building; results from a bounded walk";

#[cfg(test)]
thread_local! {
    static FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

fn first_search_index_load_wait_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE.with(std::cell::Cell::get) {
        return budget;
    }
    FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET
}

#[cfg(test)]
fn with_first_search_index_load_wait_budget_for_test<T>(
    budget: Duration,
    action: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<Duration>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE.with(|slot| slot.set(self.0));
        }
    }

    let previous =
        FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET_OVERRIDE.with(|slot| slot.replace(Some(budget)));
    let _reset = Reset(previous);
    action()
}
const BORROWED_SEARCH_LOAD_WARNING: &str = "Borrowed search index loading stopped at the interactive budget; returning a bounded lexical scan (no semantic ranking).";
const BORROWED_SEARCH_LOAD_FOOTER: &str = "[Degraded: borrowed search index loading stopped at the interactive budget; bounded lexical scan only.]";
const BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS: &str = "Semantic lane is loading the shared index; lexical results below are complete for exact/identifier matches.";
const STALE_CLI_SNAPSHOT_WARNING: &str = "Serving the last usable standing-root CLI snapshot after its freshness could not be verified; rerun `npx @cortexkit/aft index` to refresh it.";
/// Cap on the rank-0 full-symbol preview. Sized to absorb the follow-up zoom for
/// virtually every real function/type so the agent doesn't re-read a file it
/// already saw in search; a symbol exceeding it falls back to the line-budget
/// preview + "+N more lines". aft_zoom itself is uncapped, but search expansion is
/// automatic (not explicitly requested), so a runaway giant stays bounded.
const RANK0_FULL_SNIPPET_MAX_LINES: usize = 250;

/// Appended under the rank-0 snippet ONLY when the complete symbol was shown
/// (full expansion, not the capped preview). Tells the agent the body is the live
/// on-disk content so it can edit directly instead of spending a redundant
/// zoom/read — which is the entire point of the full-symbol expansion. It must
/// never appear on a partial preview (ranks 1-2, or the >cap fallback that ends
/// in "+N more lines"), where re-reading IS needed.
const RANK0_FULL_SYMBOL_NOTICE: &str =
    "full symbol shown as-is from disk; edit directly from this context — no need to re-read or zoom first";

#[derive(Debug, Clone)]
pub struct HybridResult {
    pub file: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub exported: bool,
    pub score: f32,
    pub source: &'static str,
    pub semantic_score: Option<f32>,
    pub lexical_score: Option<f32>,
    pub hybrid_boosted: bool,
    pub exact: bool,
    pub(crate) exact_phrase_count: usize,
    pub(crate) exact_window_lines: Option<usize>,
    pub(crate) fusion_score: f32,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
struct SemanticSearchParams {
    query: String,
    #[serde(default = "default_top_k", alias = "topK")]
    top_k: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default, alias = "includeTests")]
    include_tests: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Regex,
    Literal,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchIndexWaitError {
    Cancelled,
    Contended,
}

struct RuntimeReadinessSource<'a> {
    ctx: &'a AppContext,
}

#[derive(Clone)]
struct ExternalBorrowedArtifacts {
    search: ReadOnlyArtifact<Arc<SearchIndex>>,
    semantic: ReadOnlyArtifact<Arc<SemanticIndex>>,
    search_generation: GenerationToken,
}

struct ExternalReadinessSource<'a> {
    ctx: &'a AppContext,
    root: &'a Path,
    storage_dir: Option<&'a Path>,
    loaded: OnceLock<ExternalBorrowedArtifacts>,
}

impl<'a> ExternalReadinessSource<'a> {
    fn new(ctx: &'a AppContext, root: &'a Path, storage_dir: Option<&'a Path>) -> Self {
        Self {
            ctx,
            root,
            storage_dir,
            loaded: OnceLock::new(),
        }
    }

    fn load(&self) -> &ExternalBorrowedArtifacts {
        self.loaded.get_or_init(|| {
            let generation = crate::readonly_artifacts::search_index_artifact_generation(
                self.root,
                self.storage_dir,
            )
            .map(|artifact| format!("{:?}", artifact.generation))
            .unwrap_or_else(|| "absent".to_string());
            ExternalBorrowedArtifacts {
                search: self
                    .ctx
                    .open_borrowed_search_index(self.root, self.storage_dir),
                semantic: self
                    .ctx
                    .open_borrowed_semantic_index(self.root, self.storage_dir),
                search_generation: GenerationToken::new_with_str(&format!(
                    "borrowed:{}:{generation}",
                    self.root.display()
                )),
            }
        })
    }

    fn loaded(&self) -> Option<&ExternalBorrowedArtifacts> {
        self.loaded.get()
    }
}

impl extensions::ReadinessSource for ExternalReadinessSource<'_> {
    fn sample(&self) -> extensions::ReadinessObservation<'_> {
        use extensions::{
            ReadinessObservation, SemanticReadiness, SemanticSnapshot, SymbolIndexStatus,
            SymbolReadiness, TrigramReadiness,
        };

        let Some(artifacts) = self.loaded() else {
            return ReadinessObservation {
                semantic: SemanticReadiness {
                    status: SemanticIndexStatus::Building {
                        stage: "loading_artifacts".to_string(),
                        files: None,
                        entries_done: None,
                        entries_total: None,
                    },
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
                trigram: TrigramReadiness {
                    status: IndexStatus::Building,
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
                symbol: SymbolReadiness {
                    status: SymbolIndexStatus::Disabled,
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
            };
        };

        let trigram = match &artifacts.search {
            ReadOnlyArtifact::Fresh(index)
            | ReadOnlyArtifact::Stale(crate::readonly_artifacts::ReadOnlyStale { index, .. }) => {
                TrigramReadiness {
                    status: IndexStatus::Ready,
                    snapshot: Some(Arc::new(index.snapshot())),
                    evicted: false,
                    lock_contended: false,
                }
            }
            ReadOnlyArtifact::Degraded(_) | ReadOnlyArtifact::Absent => TrigramReadiness {
                status: IndexStatus::Fallback,
                snapshot: None,
                evicted: false,
                lock_contended: false,
            },
            ReadOnlyArtifact::Cancelled => TrigramReadiness {
                status: IndexStatus::Building,
                snapshot: None,
                evicted: false,
                lock_contended: false,
            },
        };
        let semantic =
            match &artifacts.semantic {
                ReadOnlyArtifact::Fresh(index)
                | ReadOnlyArtifact::Stale(crate::readonly_artifacts::ReadOnlyStale {
                    index, ..
                }) if semantic_fingerprint_matches_session(self.ctx, index) => SemanticReadiness {
                    status: SemanticIndexStatus::ready(),
                    snapshot: Some(SemanticSnapshot::from_borrowed(Arc::clone(index))),
                    evicted: false,
                    lock_contended: false,
                },
                ReadOnlyArtifact::Cancelled => SemanticReadiness {
                    status: SemanticIndexStatus::Building {
                        stage: "loading_artifacts".to_string(),
                        files: None,
                        entries_done: None,
                        entries_total: None,
                    },
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
                ReadOnlyArtifact::Degraded(_) => SemanticReadiness {
                    status: SemanticIndexStatus::Failed(
                        "borrowed_semantic_index_load_budget".to_string(),
                    ),
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
                ReadOnlyArtifact::Fresh(_) | ReadOnlyArtifact::Stale(_) => SemanticReadiness {
                    status: SemanticIndexStatus::Failed("fingerprint_mismatch".to_string()),
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
                ReadOnlyArtifact::Absent => SemanticReadiness {
                    status: SemanticIndexStatus::Disabled,
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
            };

        ReadinessObservation {
            semantic,
            trigram,
            symbol: SymbolReadiness {
                status: SymbolIndexStatus::Disabled,
                snapshot: None,
                evicted: false,
                lock_contended: false,
            },
        }
    }

    fn bounded_first_search_wait(&self) -> extensions::ReadinessWait {
        let artifacts = self.load();
        if matches!(artifacts.search, ReadOnlyArtifact::Cancelled)
            || matches!(artifacts.semantic, ReadOnlyArtifact::Cancelled)
        {
            extensions::ReadinessWait::Cancelled
        } else {
            extensions::ReadinessWait::Completed
        }
    }
}

impl extensions::ReadinessSource for RuntimeReadinessSource<'_> {
    fn sample(&self) -> extensions::ReadinessObservation<'_> {
        use extensions::{
            ReadinessObservation, SemanticReadiness, SemanticSnapshot, SymbolIndexStatus,
            SymbolReadiness, TrigramReadiness,
        };

        let semantic = match try_read_with_budget(
            self.ctx.semantic_index_status(),
            INTERACTIVE_ARTIFACT_READ_BUDGET,
        ) {
            Some(status) => {
                let status = status.clone();
                if matches!(status, SemanticIndexStatus::Ready { .. }) {
                    match try_read_with_budget(
                        self.ctx.semantic_index(),
                        INTERACTIVE_ARTIFACT_READ_BUDGET,
                    ) {
                        Some(index) => {
                            let evicted = index.is_none();
                            let snapshot = (!evicted).then(|| SemanticSnapshot::from_guard(index));
                            SemanticReadiness {
                                evicted,
                                status,
                                snapshot,
                                lock_contended: false,
                            }
                        }
                        None => SemanticReadiness {
                            status,
                            snapshot: None,
                            evicted: false,
                            lock_contended: true,
                        },
                    }
                } else {
                    SemanticReadiness {
                        status,
                        snapshot: None,
                        evicted: false,
                        lock_contended: false,
                    }
                }
            }
            None => SemanticReadiness {
                status: SemanticIndexStatus::Building {
                    stage: "status_lock".to_string(),
                    files: None,
                    entries_done: None,
                    entries_total: None,
                },
                snapshot: None,
                evicted: false,
                lock_contended: true,
            },
        };

        let trigram =
            match try_read_with_budget(self.ctx.search_index(), INTERACTIVE_ARTIFACT_READ_BUDGET) {
                Some(index) if index.as_ref().is_some_and(SearchIndex::is_ready) => {
                    TrigramReadiness {
                        status: IndexStatus::Ready,
                        snapshot: index.as_ref().map(SearchIndex::snapshot).map(Arc::new),
                        evicted: false,
                        lock_contended: false,
                    }
                }
                Some(index) if index.is_some() => TrigramReadiness {
                    status: IndexStatus::Building,
                    snapshot: None,
                    evicted: false,
                    lock_contended: false,
                },
                Some(_) => {
                    let receiver = try_read_with_budget(
                        self.ctx.search_index_rx(),
                        INTERACTIVE_ARTIFACT_READ_BUDGET,
                    );
                    match receiver {
                        Some(receiver) if receiver.is_some() => TrigramReadiness {
                            status: IndexStatus::Building,
                            snapshot: None,
                            evicted: false,
                            lock_contended: false,
                        },
                        Some(_) => TrigramReadiness {
                            status: IndexStatus::Fallback,
                            snapshot: None,
                            evicted: false,
                            lock_contended: false,
                        },
                        None => TrigramReadiness {
                            status: IndexStatus::Building,
                            snapshot: None,
                            evicted: false,
                            lock_contended: true,
                        },
                    }
                }
                None => TrigramReadiness {
                    status: IndexStatus::Building,
                    snapshot: None,
                    evicted: false,
                    lock_contended: true,
                },
            };

        let symbol_cache = self.ctx.symbol_cache();
        let symbol = match try_read_with_budget(&symbol_cache, INTERACTIVE_ARTIFACT_READ_BUDGET) {
            Some(cache) if cache.len() > 0 => SymbolReadiness {
                status: SymbolIndexStatus::Ready,
                snapshot: Some(Arc::new(cache.clone())),
                evicted: false,
                lock_contended: false,
            },
            Some(_) => SymbolReadiness {
                status: SymbolIndexStatus::Building,
                snapshot: None,
                evicted: true,
                lock_contended: false,
            },
            None => SymbolReadiness {
                status: SymbolIndexStatus::Building,
                snapshot: None,
                evicted: false,
                lock_contended: true,
            },
        };

        ReadinessObservation {
            semantic,
            trigram,
            symbol,
        }
    }

    fn bounded_first_search_wait(&self) -> extensions::ReadinessWait {
        // A writer-held pointer cannot be advanced by the loader drain below;
        // preserve the interactive contention bound instead of sleeping 2.5s.
        if matches!(
            self.ctx.search_index().try_read(),
            Err(std::sync::TryLockError::WouldBlock)
        ) {
            return extensions::ReadinessWait::Completed;
        }
        match search_index_ready_with_budget(self.ctx, first_search_index_load_wait_budget()) {
            Err(SearchIndexWaitError::Cancelled) => extensions::ReadinessWait::Cancelled,
            Ok(_) | Err(SearchIndexWaitError::Contended) => extensions::ReadinessWait::Completed,
        }
    }
}

#[derive(Debug, Clone)]
struct DegradedGrepFallbackResult {
    grep: GrepResult,
    file_cap_reached: bool,
    file_limit: usize,
    candidate_files: usize,
    walk_budget_reached: bool,
}

#[derive(Debug, Clone, Default)]
struct ExternalBorrowMetadata {
    drift_count: usize,
    ignore_rules_differ: bool,
    degraded_reason: Option<&'static str>,
    standing_snapshot: bool,
    strict_verification_required: bool,
}

impl ExternalBorrowMetadata {
    fn record_drift(&mut self, drift_count: usize, ignore_rules_differ: bool) {
        self.drift_count = self.drift_count.max(drift_count);
        self.ignore_rules_differ |= ignore_rules_differ;
    }

    fn stale_cli_snapshot(&self) -> bool {
        self.standing_snapshot && (self.strict_verification_required || self.drift_count > 0)
    }
}

fn standing_snapshot_metadata(
    external_root: &Path,
    storage_dir: Option<&Path>,
) -> ExternalBorrowMetadata {
    let db_path = crate::bash_background::storage_dir(storage_dir).join("aft.db");
    if !db_path.is_file() {
        return ExternalBorrowMetadata::default();
    }

    let resolved_target = std::fs::canonicalize(external_root)
        .unwrap_or_else(|_| external_root.to_path_buf())
        .display()
        .to_string();
    let Ok(conn) = crate::db::open(&db_path) else {
        return ExternalBorrowMetadata::default();
    };
    let Ok(needs_strict_verify) =
        crate::db::standing_roots::needs_strict_verify_for_resolved_target(
            &conn,
            &resolved_target,
            IndexKind::Search,
        )
    else {
        return ExternalBorrowMetadata::default();
    };

    let Some(strict_verification_required) = needs_strict_verify else {
        return ExternalBorrowMetadata::default();
    };
    ExternalBorrowMetadata {
        standing_snapshot: true,
        strict_verification_required,
        ..ExternalBorrowMetadata::default()
    }
}

pub(crate) fn search_cancellation_requested() -> bool {
    crate::executor::current_job_cancelled()
}

fn cancelled_search_response(req: &RawRequest) -> Response {
    cancelled_search_response_from_id(&req.id)
}

fn cancelled_search_response_from_id(request_id: &str) -> Response {
    Response::error(
        request_id,
        "request_cancelled",
        "Search request cancelled because its route closed.",
    )
}

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
    use extensions::{RawQuery, Root, Token};

    let page_request = match paging::parse_public_page_request(&req.params) {
        Ok(request) => request,
        Err(error) => return Response::error(&req.id, error.code(), error.to_string()),
    };
    let raw_query = RawQuery::new(
        req.params
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
    );
    if raw_query.original_query().trim().is_empty() {
        return Response::error(&req.id, "invalid_request", "query must be non-empty");
    }
    let project_root = grep_executor::project_root(ctx);
    let requested_path = req
        .params
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let external_root = if let Some(requested_path) = requested_path {
        match ctx.resolve_external_git_root(&project_root, requested_path) {
            Ok(root) if root != project_root => {
                if ctx.config().restrict_to_project_root || ctx.request_force_restrict(&req.id) {
                    return Response::error(
                        &req.id,
                        "path_outside_root",
                        format!(
                            "aft_search path is outside the configured project root while path restriction is enabled: {}",
                            root.display()
                        ),
                    );
                }
                Some(root)
            }
            Ok(_) => None,
            Err(GitRootResolutionError::PathNotFound(path)) => {
                return Response::error(
                    &req.id,
                    "path_not_found",
                    format!("path does not exist: {}", path.display()),
                );
            }
            Err(GitRootResolutionError::NotAGitRoot) => {
                return Response::error(
                    &req.id,
                    "not_a_git_root",
                    format!("path is not inside a git repository: {requested_path}"),
                );
            }
            Err(GitRootResolutionError::Other(error)) => {
                return Response::error(&req.id, "path_resolution_failed", error);
            }
        }
    } else {
        None
    };

    // The extensions come from the search_b2 install point: A-side defaults
    // until campaign B2 installs its router, plans, variants and readiness.
    let extensions = crate::search_b2::install_defaults();
    let (shape, facts) = extensions.classify(&raw_query);
    let variants = extensions.variants(Token {
        index: 0,
        text: raw_query.original_query(),
    });
    let runtime_source = RuntimeReadinessSource { ctx };
    let storage_dir = ctx.config().storage_dir.clone();
    let external_source = external_root
        .as_deref()
        .map(|root| ExternalReadinessSource::new(ctx, root, storage_dir.as_deref()));
    let readiness_source: &dyn extensions::ReadinessSource = external_source
        .as_ref()
        .map(|source| source as &dyn extensions::ReadinessSource)
        .unwrap_or(&runtime_source);
    let root = Root::new(
        external_root.as_deref().unwrap_or(&project_root),
        readiness_source,
    );
    let readiness = extensions.sample_readiness(&root);
    if readiness.cancelled() {
        return cancelled_search_response(req);
    }
    let mut plan = extensions.plan(&shape, &facts, &readiness);
    if plan.contains(SearchLaneKind::Exact) {
        plan.exact_input = Some(crate::search_b2::router::exact_input(
            &raw_query, shape, &facts,
        ));
    }
    if plan.contains(SearchLaneKind::Variants) {
        plan.variants = variants.into_iter().map(|variant| variant.text).collect();
    }

    let _embedding_attribution = crate::search_b2::embed_counter::install(req.id.clone());
    let mut response = handle_semantic_search_inner(
        req,
        ctx,
        page_request,
        extensions,
        &plan,
        external_source.as_ref(),
    );
    if response.success {
        let embedding_counts = crate::search_b2::embed_counter::read(&req.id);
        attach_search_execution_metadata(&mut response, &plan, embedding_counts);
    }
    response
}

fn attach_search_execution_metadata(
    response: &mut Response,
    plan: &extensions::LanePlan<'_>,
    embedding_counts: crate::search_b2::embed_counter::EmbedCounts,
) {
    let Some(data) = response.data.as_object_mut() else {
        return;
    };
    let structured = data
        .entry("structuredContent".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(structured) = structured.as_object_mut() else {
        return;
    };
    let extension_plan = serde_json::to_value(plan).unwrap_or_else(|_| serde_json::json!({}));
    let structured_plan = structured
        .entry("plan".to_string())
        .or_insert_with(|| extension_plan.clone());
    if let (Some(structured_plan), Some(extension_plan)) =
        (structured_plan.as_object_mut(), extension_plan.as_object())
    {
        for (key, value) in extension_plan {
            structured_plan
                .entry(key.clone())
                .or_insert_with(|| value.clone());
        }
        structured_plan.insert(
            "embedding_calls".to_string(),
            serde_json::json!(embedding_counts.requested),
        );
        structured_plan.insert(
            "embedding_cache_hits".to_string(),
            serde_json::json!(embedding_counts.cache_hits),
        );
        structured_plan.insert(
            "live_embed_calls".to_string(),
            serde_json::json!(embedding_counts.live_calls),
        );
    }
    structured.insert(
        "search".to_string(),
        serde_json::json!({
            "embedding_calls": embedding_counts.requested,
            "embedding_cache_hits": embedding_counts.cache_hits,
            "live_embed_calls": embedding_counts.live_calls,
        }),
    );
}

fn handle_semantic_search_inner(
    req: &RawRequest,
    ctx: &AppContext,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
    external_source: Option<&ExternalReadinessSource<'_>>,
) -> Response {
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }
    let mut params = match serde_json::from_value::<SemanticSearchParams>(req.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("semantic_search: invalid params: {error}"),
            );
        }
    };

    if params.query.trim().is_empty() {
        return Response::error(&req.id, "invalid_request", "query must be non-empty");
    }

    // Quoting is part of QueryFacts and remains visible to the plan hook. Only
    // the code-literal execution route removes its balanced delimiter pair.
    if engine_plan.shape == SearchShape::CodeLiteral {
        params.query = strip_surrounding_quotes(params.query);
        if params.query.trim().is_empty() {
            return Response::error(&req.id, "invalid_request", "query must be non-empty");
        }
    }

    let top_k = page_request.top_k();
    params.top_k = top_k;
    params.offset = page_request.offset();
    let project_root = grep_executor::project_root(ctx);
    let shape = query_shape::classify(&params.query);
    if let Some(external_source) = external_source {
        return handle_external_search(
            req,
            ctx,
            params,
            shape,
            page_request,
            extensions,
            engine_plan,
            external_source,
        );
    }
    let semantic_status_snapshot = match try_read_with_budget(
        ctx.semantic_index_status(),
        INTERACTIVE_ARTIFACT_READ_BUDGET,
    ) {
        Some(status) => status.clone(),
        None => {
            return artifact_contention_fallback_response(
                req,
                ctx,
                &params,
                &shape,
                &project_root,
                top_k,
                "semantic index status remained busy",
            );
        }
    };
    let semantic_status = semantic_status_label(&semantic_status_snapshot);
    let mut warnings = Vec::new();

    let lexical_ready = match search_index_ready_with_budget(ctx, INTERACTIVE_ARTIFACT_READ_BUDGET)
    {
        Ok(ready) => ready,
        Err(SearchIndexWaitError::Cancelled) => return cancelled_search_response(req),
        Err(SearchIndexWaitError::Contended) => {
            return artifact_contention_fallback_response(
                req,
                ctx,
                &params,
                &shape,
                &project_root,
                top_k,
                "search index remained busy",
            );
        }
    };
    let mode = choose_mode(&params.query, &shape, lexical_ready, &mut warnings);
    if lexical_ready && mode != SearchMode::Regex && !engine_plan.contains(SearchLaneKind::Semantic)
    {
        return handle_engine_only_search(
            req,
            ctx,
            &params,
            &shape,
            semantic_status,
            warnings,
            &project_root,
            page_request,
            extensions,
            engine_plan,
        );
    }

    match mode {
        SearchMode::Regex | SearchMode::Literal => handle_grep_search(
            req,
            ctx,
            &params.query,
            params.offset,
            top_k,
            &shape,
            mode,
            semantic_status,
            warnings,
            &project_root,
            params.include_tests,
            page_request,
            extensions,
            engine_plan,
        ),
        SearchMode::Semantic | SearchMode::Hybrid => handle_semantic_or_hybrid_search(
            req,
            ctx,
            params,
            top_k,
            shape,
            mode,
            lexical_ready,
            semantic_status_snapshot,
            semantic_status,
            warnings,
            &project_root,
            page_request,
            extensions,
            engine_plan,
        ),
    }
}

fn handle_external_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    shape: QueryShape,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
    readiness_source: &ExternalReadinessSource<'_>,
) -> Response {
    let top_k = page_request.top_k();
    let external_root = readiness_source.root.to_path_buf();
    let artifacts = readiness_source
        .loaded()
        .expect("external readiness always loads artifacts before execution");
    let mut borrow_metadata = if ctx.daemonless_query_mode() {
        standing_snapshot_metadata(&external_root, readiness_source.storage_dir)
    } else {
        ExternalBorrowMetadata::default()
    };
    let mut warnings = Vec::new();
    let search_index = match &artifacts.search {
        ReadOnlyArtifact::Fresh(index) => Arc::clone(index),
        ReadOnlyArtifact::Degraded(degradation) => {
            if engine_plan.contains(SearchLaneKind::Semantic) {
                borrow_metadata.degraded_reason = Some(degradation.reason);
                warnings.push(
                    "Borrowed trigram index loading stopped at the interactive budget; continuing with the semantic lane.".to_string(),
                );
                Arc::new(SearchIndex::new())
            } else {
                return handle_external_borrowed_degraded_fallback(
                    req,
                    ctx,
                    &params,
                    top_k,
                    &shape,
                    &external_root,
                    *degradation,
                );
            }
        }
        ReadOnlyArtifact::Cancelled => return cancelled_search_response(req),
        ReadOnlyArtifact::Absent => {
            if engine_plan.contains(SearchLaneKind::Semantic) {
                warnings.push(
                    "External trigram index is not available; continuing with the semantic lane."
                        .to_string(),
                );
                Arc::new(SearchIndex::new())
            } else {
                return handle_external_unindexed_fallback(
                    req,
                    ctx,
                    &params,
                    top_k,
                    &shape,
                    &external_root,
                );
            }
        }
        ReadOnlyArtifact::Stale(stale) => {
            borrow_metadata.record_drift(stale.drift_count, stale.ignore_rules_differ);
            crate::slog_warn!(
                "{}",
                borrowed_drift_log_message("search", &external_root, stale.drift_count)
            );
            Arc::clone(&stale.index)
        }
    };
    if let ReadOnlyArtifact::Stale(stale) = &artifacts.semantic {
        borrow_metadata.record_drift(stale.drift_count, stale.ignore_rules_differ);
        crate::slog_warn!(
            "{}",
            borrowed_drift_log_message("semantic", &external_root, stale.drift_count)
        );
    }
    if borrow_metadata.standing_snapshot {
        borrow_metadata.record_drift(search_index.borrowed_stat_mismatch_count(), false);
    }

    if borrow_metadata.stale_cli_snapshot() {
        warnings.push(STALE_CLI_SNAPSHOT_WARNING.to_string());
    }
    let mode = choose_mode(
        &params.query,
        &shape,
        engine_plan.readiness.lexical_index,
        &mut warnings,
    );

    match mode {
        SearchMode::Regex | SearchMode::Literal => handle_external_grep_search(
            req,
            ctx,
            &params.query,
            page_request,
            &shape,
            mode,
            warnings,
            &external_root,
            params.include_tests,
            &search_index,
            &borrow_metadata,
            extensions,
            engine_plan,
            &artifacts.search_generation,
        ),
        SearchMode::Semantic | SearchMode::Hybrid => handle_external_semantic_or_hybrid_search(
            req,
            ctx,
            params,
            shape,
            mode,
            warnings,
            external_root,
            &search_index,
            &artifacts.semantic,
            &artifacts.search_generation,
            borrow_metadata,
            page_request,
            extensions,
            engine_plan,
        ),
    }
}

/// Bounded lexical scan of a foreign git root that has no borrowable AFT
/// index. Mirrors the `grep` tool's unindexed-external behavior (fallback
/// filesystem walk via the shared `grep_executor`) so `aft_search` degrades to
/// real results with a disclosure instead of a hard `not_indexed` error.
/// Semantic/hybrid intent cannot run without embeddings, so every mode degrades
/// to a lexical substring/regex scan here.
fn handle_external_unindexed_fallback(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    top_k: usize,
    shape: &QueryShape,
    external_root: &Path,
) -> Response {
    handle_external_bounded_lexical_fallback(req, ctx, params, top_k, shape, external_root, None)
}

fn handle_external_borrowed_degraded_fallback(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    top_k: usize,
    shape: &QueryShape,
    external_root: &Path,
    degradation: ReadOnlyDegradation,
) -> Response {
    handle_external_bounded_lexical_fallback(
        req,
        ctx,
        params,
        top_k,
        shape,
        external_root,
        Some(degradation),
    )
}

fn handle_external_bounded_lexical_fallback(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    top_k: usize,
    shape: &QueryShape,
    external_root: &Path,
    degradation: Option<ReadOnlyDegradation>,
) -> Response {
    let borrow_metadata = ExternalBorrowMetadata::default();
    let literal = true;
    let compiled = match pattern_compile::compile(
        &params.query,
        CompileOpts {
            literal,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            return Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                external_response_extras(external_root, &borrow_metadata),
            );
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            return Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                external_response_extras(external_root, &borrow_metadata),
            );
        }
    };

    let path_value = serde_json::Value::String(external_root.to_string_lossy().into_owned());
    let scope = match grep_executor::resolve_grep_scope(ctx, Some(&path_value), top_k, &req.id) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let grep_params = grep_executor::GrepParams {
        include: Vec::new(),
        exclude: Vec::new(),
        max_results: top_k,
        path_exclusion: grep_path_exclusion(params.include_tests),
    };
    let result = grep_executor::execute(ctx, &compiled, &scope, &grep_params);
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }

    let result_source = if literal { "literal" } else { "regex" };
    let interpreted_as = if literal { "literal" } else { "regex" };
    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let display_root = absolute_display_root(external_root);
    let mut text = format_grep_search_text(&result, &display_root, interpreted_as);
    if degradation.is_some() {
        text.push_str("\n\n");
        text.push_str(BORROWED_SEARCH_LOAD_FOOTER);
    }

    let mut warnings = Vec::new();
    if degradation.is_some() {
        warnings.push(BORROWED_SEARCH_LOAD_WARNING.to_string());
    } else {
        warnings.push(format!(
            "No AFT index exists for {} — returning a bounded lexical scan (no semantic ranking). Open a session in that project for full indexed search.",
            external_root.display()
        ));
    }
    if result.walk_truncated {
        warnings.push(
            "Lexical scan stopped early (file-count or time budget reached); results may be incomplete.".to_string(),
        );
    }

    let mut extras = external_response_extras(external_root, &borrow_metadata)
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(degradation) = degradation {
        extras.insert(
            "borrowed_index_degraded_reason".to_string(),
            serde_json::json!(degradation.reason),
        );
    }
    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as,
            query_kind: query_kind_label(shape.kind),
            semantic_status: if degradation.is_some() {
                "external_borrowed_degraded"
            } else {
                "external_unindexed"
            },
            status: "ready",
            complete: degradation.is_none() && !result.walk_truncated,
            text,
            results: result_values,
            more_available: result.walk_truncated
                || result.truncated
                || result.total_matches > result.matches.len(),
            engine_capped: result.engine_capped,
            fully_degraded: true,
            warnings,
            extras,
        },
    )
}

fn handle_external_grep_search(
    req: &RawRequest,
    ctx: &AppContext,
    query: &str,
    page_request: paging::ValidatedPageRequest,
    shape: &QueryShape,
    mode: SearchMode,
    mut warnings: Vec<String>,
    external_root: &Path,
    include_tests: bool,
    search_index: &SearchIndex,
    borrow_metadata: &ExternalBorrowMetadata,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
    search_generation: &GenerationToken,
) -> Response {
    let top_k = page_request.top_k();
    let auto_regex = mode == SearchMode::Regex;
    let mut effective_mode = mode;
    let compile_literal_fallback = || -> Result<_, Response> {
        match pattern_compile::compile(
            query,
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => Ok(compiled),
            CompileResult::InvalidPattern { message, .. } => Err(Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                external_response_extras(external_root, borrow_metadata),
            )),
            CompileResult::UnsupportedSyntax { feature, .. } => Err(Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                external_response_extras(external_root, borrow_metadata),
            )),
        }
    };

    let compiled = match pattern_compile::compile(
        query,
        CompileOpts {
            literal: mode == SearchMode::Literal,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(
                    short_regex_compile_reason(&message),
                ));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "invalid_pattern",
                    message,
                    external_response_extras(external_root, borrow_metadata),
                );
            }
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(format!(
                    "{feature} is not supported"
                )));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "unsupported_pattern",
                    format!(
                        "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                    ),
                    external_response_extras(external_root, borrow_metadata),
                );
            }
        }
    };

    let literal = effective_mode == SearchMode::Literal;
    let fetch_limit = page_request.offset().saturating_add(top_k);
    let mut result = search_index.snapshot().search_grep_bounded(
        &compiled,
        &[],
        &[],
        external_root,
        fetch_limit,
        grep_path_exclusion(include_tests),
        grep_executor::MAX_FALLBACK_WALK_FILES,
        grep_executor::FALLBACK_WALK_BUDGET,
    );
    result
        .matches
        .retain(|grep_match| grep_match.file.is_file());

    if result.matches.is_empty() {
        let extras = external_response_extras(external_root, borrow_metadata)
            .as_object()
            .cloned()
            .unwrap_or_default();
        return stale_cli_snapshot_partial_response(
            zero_result_escalation_response(
                req,
                ctx,
                query,
                shape,
                effective_mode,
                "external",
                warnings,
                include_tests,
                external_root,
                &absolute_display_root(external_root),
                extras,
                page_request,
                extensions,
                engine_plan,
                Some((search_index, search_generation)),
            ),
            borrow_metadata.stale_cli_snapshot(),
        );
    }

    let interval_end = page_request.offset().saturating_add(top_k);
    let interval_has_more = result.total_matches > interval_end || result.truncated;
    result.matches = result
        .matches
        .into_iter()
        .skip(page_request.offset())
        .take(top_k)
        .collect();
    let result_source = if literal { "literal" } else { "regex" };
    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let interpreted_as = interpreted_as_label(effective_mode);
    let display_root = absolute_display_root(external_root);
    let text = format_grep_search_text(&result, &display_root, interpreted_as);
    let extras = external_response_extras(external_root, borrow_metadata)
        .as_object()
        .cloned()
        .unwrap_or_default();
    search_response(
        req,
        SearchResponseParts {
            query,
            interpreted_as,
            query_kind: query_kind_label(shape.kind),
            semantic_status: "external",
            status: "ready",
            complete: !borrow_metadata.stale_cli_snapshot(),
            text,
            results: result_values,
            more_available: interval_has_more,
            engine_capped: result.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn handle_external_semantic_or_hybrid_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    shape: QueryShape,
    mode: SearchMode,
    mut warnings: Vec<String>,
    external_root: PathBuf,
    search_index: &SearchIndex,
    semantic_artifact: &ReadOnlyArtifact<Arc<SemanticIndex>>,
    search_generation: &GenerationToken,
    mut borrow_metadata: ExternalBorrowMetadata,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
) -> Response {
    let mut semantic_status = "ready";
    let semantic_index = match semantic_artifact {
        ReadOnlyArtifact::Fresh(index)
        | ReadOnlyArtifact::Stale(crate::readonly_artifacts::ReadOnlyStale { index, .. })
            if semantic_fingerprint_matches_session(ctx, index) =>
        {
            Some(index.as_ref())
        }
        ReadOnlyArtifact::Fresh(_) | ReadOnlyArtifact::Stale(_) => {
            semantic_status = "unavailable";
            warnings.push(
                "External semantic index was built for a different embedding backend or model; returning lexical-only results from the trigram index.".to_string(),
            );
            None
        }
        ReadOnlyArtifact::Degraded(degradation) => {
            semantic_status = "building";
            warnings.push(
                "Borrowed semantic index loading stopped at the interactive budget; returning lexical-only results from the trigram index.".to_string(),
            );
            borrow_metadata.degraded_reason = Some(degradation.reason);
            None
        }
        ReadOnlyArtifact::Cancelled => return cancelled_search_response(req),
        ReadOnlyArtifact::Absent => {
            semantic_status = "unavailable";
            warnings.push(
                "External semantic index is not available; returning lexical-only results from the trigram index.".to_string(),
            );
            None
        }
    };

    let mut semantic_more_available = false;
    let mut semantic_results = if engine_plan.contains(SearchLaneKind::Semantic) {
        semantic_index
            .map(|semantic_index| {
                embed_query_for_dimension(&params.query, ctx, Some(semantic_index.dimension())).map(
                    |query_vector| {
                        let mut results = semantic_index.search_filtered(
                            &query_vector,
                            MAX_TOP_K.saturating_add(1),
                            |file| {
                                path_allowed_by_include_tests(
                                    file,
                                    &external_root,
                                    params.include_tests,
                                )
                            },
                        );
                        results.retain(|result| result.file.is_file());
                        semantic_more_available = results.len() > MAX_TOP_K;
                        if semantic_more_available {
                            results.truncate(MAX_TOP_K);
                        }
                        rerank_semantic_candidates(&mut results, &shape, &params.query);
                        results
                    },
                )
            })
            .transpose()
            .unwrap_or_else(|error| {
                semantic_status = "unavailable";
                warnings.push(classify_embed_query_error(&error).detail);
                None
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut ranked = match run_engine_ranking(
        &req.id,
        ctx,
        &external_root,
        &params.query,
        params.include_tests,
        std::mem::take(&mut semantic_results),
        page_request,
        extensions,
        engine_plan,
        Some((search_index, search_generation)),
    ) {
        Ok(ranked) => ranked,
        Err(error) => return Response::error(&req.id, "search_engine_failed", error),
    };
    ranked.results.retain(|result| result.file.is_file());
    let more_available = ranked.more_available || semantic_more_available;
    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut ranked.results, &external_root, Some(ctx));
    let display_root = absolute_display_root(&external_root);
    let mut text = format_semantic_text_with_display_root(
        &ranked.results,
        &display_root,
        more_available,
        snippets_incomplete,
        Some(ctx),
    );
    if semantic_status != "ready" {
        let disclosure = if semantic_status == "building" {
            BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS
        } else {
            "Semantic search is not available for the external root; lexical engine results follow."
        };
        text = format!("{disclosure}\n\n{text}");
    }
    if let Some(line) = ranked.confidence_line {
        text.push_str("\n\n");
        text.push_str(line);
    }
    text.push_str("\n\n");
    text.push_str(&ranked.trailer);

    let mut extras = external_response_extras(&external_root, &borrow_metadata)
        .as_object()
        .cloned()
        .unwrap_or_default();
    extras.insert("structuredContent".to_string(), ranked.structured_content);
    extras.insert(
        "lexical_only_fallback".to_string(),
        serde_json::json!(semantic_status != "ready"),
    );
    extras.insert(
        "semantic_unavailable".to_string(),
        serde_json::json!(semantic_status != "ready"),
    );
    extras.insert(
        "lexical_engine_capped".to_string(),
        serde_json::json!(ranked.engine_capped),
    );

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: if semantic_status == "ready" {
                interpreted_as_label(mode)
            } else {
                "lexical"
            },
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: if semantic_status == "building" {
                "building"
            } else {
                "ready"
            },
            complete: semantic_status == "ready" && !borrow_metadata.stale_cli_snapshot(),
            text,
            results: ranked.results.iter().map(result_to_json).collect(),
            more_available,
            engine_capped: ranked.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn semantic_fingerprint_matches_session(ctx: &AppContext, index: &SemanticIndex) -> bool {
    let config = ctx.config().semantic.clone();
    let expected = SemanticIndexFingerprint::for_config_dimension(&config, index.dimension());
    index
        .fingerprint()
        .map(|fingerprint| fingerprint.as_string() == expected.as_string())
        .unwrap_or(false)
}

fn stale_cli_snapshot_partial_response(
    mut response: Response,
    stale_cli_snapshot: bool,
) -> Response {
    if stale_cli_snapshot {
        response.data["complete"] = serde_json::Value::Bool(false);
    }
    response
}

fn external_response_extras(
    external_root: &Path,
    borrow_metadata: &ExternalBorrowMetadata,
) -> serde_json::Value {
    let mut extras = serde_json::json!({
        "external_root": external_root.display().to_string(),
        "borrowed": true,
        "drift_count": borrow_metadata.drift_count,
        "ignore_rules_differ": borrow_metadata.ignore_rules_differ,
        SUPPRESS_STATUS_BAR_FIELD: true,
    });
    if let (Some(reason), Some(object)) = (borrow_metadata.degraded_reason, extras.as_object_mut())
    {
        object.insert(
            "borrowed_index_degraded_reason".to_string(),
            serde_json::json!(reason),
        );
    }
    extras
}

fn borrowed_drift_log_message(index_kind: &str, root: &Path, drift_count: usize) -> String {
    format!(
        "borrowed {index_kind} index for {} has {drift_count} drifted file(s); serving current-disk snippets without agent-facing stale prose",
        root.display()
    )
}

fn absolute_display_root(root: &Path) -> PathBuf {
    root.join(".aft-external-display-root-nonprefix")
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

fn project_relative_path<'a>(path: &'a Path, project_root: &'a Path) -> &'a Path {
    path.strip_prefix(project_root).unwrap_or(path)
}

fn path_is_test_support_file(path: &Path, project_root: &Path) -> bool {
    let relative = project_relative_path(path, project_root);
    is_test_support_file(relative.to_string_lossy().as_ref())
}

/// Whether `path` is something `aft_search` hides unless `include_tests` is set:
/// a test-support file (fixtures/mocks/snapshots) OR an actual test file
/// (`*.test.ts`, `__tests__/`, `*_test.rs`, …). Search is a code-discovery tool,
/// so test code is noise by default; `include_tests: true` shows both classes.
fn path_is_hidden_test_file(path: &Path, project_root: &Path) -> bool {
    let relative = project_relative_path(path, project_root);
    let rel = relative.to_string_lossy();
    is_test_support_file(rel.as_ref()) || is_test_file(rel.as_ref())
}

fn path_allowed_by_include_tests(path: &Path, project_root: &Path, include_tests: bool) -> bool {
    include_tests || !path_is_hidden_test_file(path, project_root)
}

fn grep_path_exclusion(include_tests: bool) -> Option<GrepPathExclusion> {
    (!include_tests).then_some(path_is_hidden_test_file)
}

fn lexical_candidate_exactness(
    file: &Path,
    query: &str,
    content_tokens: &[String],
) -> (bool, usize, Option<usize>) {
    let Ok(bytes) = fs::read(file) else {
        return (false, 0, None);
    };
    let text = String::from_utf8_lossy(&bytes);
    let normalized_text = normalize_exact_phrase(&text);
    let normalized_phrase = normalize_exact_phrase(exact_phrase(query));
    let phrase_count = if normalized_phrase.is_empty() {
        0
    } else {
        normalized_text.matches(&normalized_phrase).count()
    };
    if phrase_count > 0 {
        return (true, phrase_count, Some(1));
    }

    let lines = text.lines().collect::<Vec<_>>();
    for width in 1..=3 {
        if lines.len() < width {
            continue;
        }
        if lines.windows(width).any(|window| {
            query_shape::contains_all_content_tokens(&window.join("\n"), content_tokens)
        }) {
            return (true, 0, Some(width));
        }
    }
    (false, 0, None)
}

fn exact_phrase(query: &str) -> &str {
    let trimmed = query.trim();
    if trimmed.len() < 2 {
        return trimmed;
    }
    let first = trimmed.as_bytes()[0];
    let last = trimmed.as_bytes()[trimmed.len() - 1];
    if matches!(first, b'\'' | b'"') && first == last {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn normalize_exact_phrase(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn choose_mode(
    query: &str,
    shape: &QueryShape,
    lexical_ready: bool,
    warnings: &mut Vec<String>,
) -> SearchMode {
    if shape.kind == QueryKind::Regex {
        return SearchMode::Regex;
    }
    if shape.kind != QueryKind::NaturalLanguage && extracted_tokens_all_short(query, shape) {
        warnings.push(
            "Auto mode is using literal full-file scan for all-short exact tokens because the trigram index cannot rank tokens shorter than 3 chars.".to_string(),
        );
        return SearchMode::Literal;
    }
    if lexical_ready {
        SearchMode::Hybrid
    } else {
        warnings
            .push("Lexical trigram index is unavailable; using semantic search only.".to_string());
        SearchMode::Semantic
    }
}

fn handle_grep_search(
    req: &RawRequest,
    ctx: &AppContext,
    query: &str,
    offset: usize,
    top_k: usize,
    shape: &QueryShape,
    mode: SearchMode,
    semantic_status: &'static str,
    mut warnings: Vec<String>,
    project_root: &Path,
    include_tests: bool,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
) -> Response {
    let auto_regex = mode == SearchMode::Regex;
    let mut effective_mode = mode;
    let compile_literal_fallback = || -> Result<_, Response> {
        match pattern_compile::compile(
            query,
            CompileOpts {
                literal: true,
                ..CompileOpts::default()
            },
        ) {
            CompileResult::Ok(compiled) => Ok(compiled),
            CompileResult::InvalidPattern { message, .. } => Err(Response::error_with_data(
                &req.id,
                "invalid_pattern",
                message,
                serde_json::json!({"pattern": query}),
            )),
            CompileResult::UnsupportedSyntax { feature, .. } => Err(Response::error_with_data(
                &req.id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                serde_json::json!({"pattern": query, "feature": feature}),
            )),
        }
    };

    let compiled = match pattern_compile::compile(
        query,
        CompileOpts {
            literal: mode == SearchMode::Literal,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(
                    short_regex_compile_reason(&message),
                ));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "invalid_pattern",
                    message,
                    serde_json::json!({"pattern": query}),
                );
            }
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            if auto_regex {
                warnings.push(auto_regex_literal_fallback_warning(format!(
                    "{feature} is not supported"
                )));
                effective_mode = SearchMode::Literal;
                match compile_literal_fallback() {
                    Ok(compiled) => compiled,
                    Err(response) => return response,
                }
            } else {
                return Response::error_with_data(
                    &req.id,
                    "unsupported_pattern",
                    format!(
                        "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                    ),
                    serde_json::json!({"pattern": query, "feature": feature}),
                );
            }
        }
    };

    let literal = effective_mode == SearchMode::Literal;
    let fetch_limit = offset.saturating_add(top_k);
    let scope = match grep_executor::resolve_grep_scope(ctx, None, fetch_limit, &req.id) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let params = GrepParams {
        include: Vec::new(),
        exclude: Vec::new(),
        max_results: fetch_limit,
        path_exclusion: grep_path_exclusion(include_tests),
    };
    let mut result = grep_executor::execute(ctx, &compiled, &scope, &params);
    if result.fully_degraded {
        warnings.push(degraded_warning(ctx));
    }

    let result_source = if literal { "literal" } else { "regex" };
    if result.matches.is_empty() && search_index_ready(ctx) {
        return zero_result_escalation_response(
            req,
            ctx,
            query,
            shape,
            effective_mode,
            semantic_status,
            warnings,
            include_tests,
            project_root,
            project_root,
            serde_json::Map::new(),
            page_request,
            extensions,
            engine_plan,
            None,
        );
    }

    let interval_end = offset.saturating_add(top_k);
    let interval_has_more = result.total_matches > interval_end || result.truncated;
    result.matches = result
        .matches
        .into_iter()
        .skip(offset)
        .take(top_k)
        .collect();
    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, result_source))
        .collect::<Vec<_>>();
    let interpreted_as = interpreted_as_label(effective_mode);
    let trigram_index_building = semantic_status == "building"
        && matches!(
            result.index_status,
            IndexStatus::Building | IndexStatus::Fallback
        );
    let mut text = format_grep_search_text(&result, project_root, interpreted_as);
    let mut extras = serde_json::Map::new();
    if trigram_index_building {
        let envelope = bounded_walk_search_envelope(
            result_values.len(),
            interval_has_more,
            result.engine_capped,
        );
        // The trailer is not rendered here: the shared formatters append it
        // from the wire envelope below, and the contract keeps every trailer
        // on that one path so no tool can print it twice or word it differently.
        text = format!("{TRIGRAM_BUILDING_BOUNDED_WALK_DISCLOSURE}\n\n{text}");
        extras.insert(
            crate::list_surfaces::search::SEARCH_WIRE_KEY.to_string(),
            serde_json::json!(envelope),
        );
        extras.insert("lexical_only_fallback".to_string(), serde_json::json!(true));
        extras.insert("semantic_unavailable".to_string(), serde_json::json!(true));
    }
    search_response(
        req,
        SearchResponseParts {
            query,
            interpreted_as,
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: if trigram_index_building {
                "partial"
            } else {
                "ready"
            },
            complete: !trigram_index_building,
            text,
            results: result_values,
            more_available: interval_has_more,
            engine_capped: result.engine_capped,
            fully_degraded: result.fully_degraded,
            warnings,
            extras,
        },
    )
}

fn short_regex_compile_reason(message: &str) -> Cow<'_, str> {
    let trimmed = message.trim();
    let reason = trimmed
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.chars().all(|ch| ch == '^'))
        .unwrap_or(trimmed);
    Cow::Borrowed(
        reason
            .strip_prefix("error: ")
            .or_else(|| reason.strip_prefix("invalid regex: "))
            .unwrap_or(reason),
    )
}

fn auto_regex_literal_fallback_warning(reason: impl AsRef<str>) -> String {
    format!(
        "Query looked like a regex but failed to compile ({}); searched literally instead. Use grep when explicit regex lane control is required.",
        reason.as_ref()
    )
}

fn view_semantic_search(
    view: &crate::context::ViewRuntimeSnapshot,
    project_root: &Path,
    query_vector: &[f32],
    limit: usize,
    include_tests: bool,
) -> Result<Vec<SemanticResult>, String> {
    let Some(manifest) = view.manifest.as_ref() else {
        return Ok(Vec::new());
    };
    let database = view
        .storage
        .join("blobs")
        .join(&view.family)
        .join("semantic.sqlite");
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| error.to_string())?;
    let mut results = Vec::new();
    for (rel_path, entry) in manifest.entries() {
        let crate::views::ManifestEntry::Regular { planes, .. } = entry else {
            continue;
        };
        let Some(key) = planes.semantic.as_deref().and_then(decode_view_key) else {
            continue;
        };
        let payload = connection
            .query_row(
                "SELECT payload FROM blob_payloads WHERE full_key = ?1",
                [key],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some(payload) = payload else {
            continue;
        };
        let file = project_root.join(String::from_utf8_lossy(rel_path.as_bytes()).as_ref());
        if !path_allowed_by_include_tests(&file, project_root, include_tests) {
            continue;
        }
        decode_view_semantic_payload(&payload, &file, query_vector, &mut results)?;
    }
    results.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.start_line.cmp(&right.start_line))
    });
    results.truncate(limit);
    Ok(results)
}

fn decode_view_key(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

fn decode_view_semantic_payload(
    payload: &[u8],
    file: &Path,
    query_vector: &[f32],
    results: &mut Vec<SemanticResult>,
) -> Result<(), String> {
    let mut cursor = 0usize;
    let version = take_view_bytes(payload, &mut cursor, 1)?[0];
    if version != 1 {
        return Err(format!(
            "unsupported semantic view payload version {version}"
        ));
    }
    for _ in 0..3 {
        let _ = take_view_field(payload, &mut cursor)?;
    }
    let count = u32::from_le_bytes(
        take_view_bytes(payload, &mut cursor, 4)?
            .try_into()
            .map_err(|_| "invalid semantic entry count".to_string())?,
    );
    for _ in 0..count {
        let name = String::from_utf8(take_view_field(payload, &mut cursor)?.to_vec())
            .map_err(|error| error.to_string())?;
        let qualified = String::from_utf8(take_view_field(payload, &mut cursor)?.to_vec())
            .map_err(|error| error.to_string())?;
        let kind = view_symbol_kind(take_view_bytes(payload, &mut cursor, 1)?[0]);
        let start_line = u32::from_le_bytes(
            take_view_bytes(payload, &mut cursor, 4)?
                .try_into()
                .unwrap(),
        );
        let end_line = u32::from_le_bytes(
            take_view_bytes(payload, &mut cursor, 4)?
                .try_into()
                .unwrap(),
        );
        let exported = take_view_bytes(payload, &mut cursor, 1)?[0] != 0;
        let snippet = String::from_utf8(take_view_field(payload, &mut cursor)?.to_vec())
            .map_err(|error| error.to_string())?;
        let _embed_text = take_view_field(payload, &mut cursor)?;
        let vector_bytes = take_view_field(payload, &mut cursor)?;
        if vector_bytes.len() % 4 != 0 {
            return Err("semantic view vector has invalid byte length".to_string());
        }
        let vector = vector_bytes
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        if vector.len() != query_vector.len() {
            continue;
        }
        let dot = vector
            .iter()
            .zip(query_vector)
            .map(|(left, right)| left * right)
            .sum::<f32>();
        let left_norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        let right_norm = query_vector
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        let score = if left_norm == 0.0 || right_norm == 0.0 {
            0.0
        } else {
            dot / (left_norm * right_norm)
        };
        results.push(SemanticResult {
            file: file.to_path_buf(),
            name,
            qualified_name: (!qualified.is_empty()).then_some(qualified),
            kind,
            start_line,
            end_line,
            exported,
            snippet,
            score,
            rank_score: score,
            cap_protected: false,
            source: "semantic",
        });
    }
    Ok(())
}

fn take_view_field<'a>(payload: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], String> {
    let length = u32::from_le_bytes(
        take_view_bytes(payload, cursor, 4)?
            .try_into()
            .map_err(|_| "invalid semantic field length".to_string())?,
    ) as usize;
    take_view_bytes(payload, cursor, length)
}

fn take_view_bytes<'a>(
    payload: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], String> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| "semantic view payload length overflow".to_string())?;
    let bytes = payload
        .get(*cursor..end)
        .ok_or_else(|| "truncated semantic view payload".to_string())?;
    *cursor = end;
    Ok(bytes)
}

fn view_symbol_kind(value: u8) -> SymbolKind {
    match value {
        0 => SymbolKind::Function,
        1 => SymbolKind::Class,
        2 => SymbolKind::Method,
        3 => SymbolKind::Struct,
        4 => SymbolKind::Interface,
        5 => SymbolKind::Enum,
        6 => SymbolKind::TypeAlias,
        7 => SymbolKind::Variable,
        9 => SymbolKind::FileSummary,
        _ => SymbolKind::Heading,
    }
}

#[derive(Clone)]
struct PreparedEngineLane {
    kind: SearchLaneKind,
    candidates: Vec<CandidateResult>,
}

impl SearchLane for PreparedEngineLane {
    fn kind(&self) -> SearchLaneKind {
        self.kind
    }

    fn execute(&self, _input: &LaneInput<'_>) -> LaneExecution {
        LaneExecution {
            kind: self.kind,
            candidates: self.candidates.clone(),
        }
    }
}

struct EngineRanking {
    results: Vec<HybridResult>,
    more_available: bool,
    engine_capped: bool,
    trailer: String,
    confidence_line: Option<&'static str>,
    structured_content: serde_json::Value,
}

fn definition_matches_identifier_token(candidate: &CandidateResult, query: &str) -> bool {
    let Some(range) = candidate.symbol_range else {
        return false;
    };
    let Ok(source) = std::fs::read(&candidate.path) else {
        return false;
    };
    let Some(symbol) = source.get(range.start..range.end) else {
        return false;
    };
    query
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                !character.is_alphanumeric()
                    && character != '_'
                    && character != ':'
                    && character != '.'
            })
        })
        .filter(|token| crate::search_b2::router::is_identifier_shaped_token(token))
        .any(|token| {
            symbol
                .windows(token.len())
                .any(|window| window == token.as_bytes())
        })
}

fn path_scope_contains(path_scope: &HashSet<PathBuf>, path: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path_scope.contains(&path)
}

fn run_engine_ranking(
    request_id: &str,
    ctx: &AppContext,
    project_root: &Path,
    query: &str,
    include_tests: bool,
    semantic_results: Vec<SemanticResult>,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    plan: &extensions::LanePlan<'_>,
    borrowed_index: Option<(&SearchIndex, &GenerationToken)>,
) -> Result<EngineRanking, String> {
    use blocks::{BlockBuilder, CanonicalLane, CanonicalListKey, LaneCandidate, BLOCK_DEPTHS};
    use confidence::{Confidence, ConfidenceEngine};
    use lexical_lane::CanonicalLexicalLane;
    use provenance::ObservedProvenance;
    use scoring::ScoringPolicy;
    use telemetry::{ConfidenceTelemetry, TelemetryAssembler, TelemetryRun};
    use trailer::{ExactPassState, SearchTrailer};

    let context_index = borrowed_index
        .is_none()
        .then(|| try_read_with_budget(ctx.search_index(), INTERACTIVE_ARTIFACT_READ_BUDGET));
    let empty_index = SearchIndex::new();
    let index = borrowed_index
        .map(|(index, _)| index)
        .or_else(|| {
            context_index
                .as_ref()
                .and_then(|guard| guard.as_ref())
                .and_then(|guard| guard.as_ref())
        })
        .unwrap_or(&empty_index);
    let generation = borrowed_index
        .map(|(_, generation)| generation.clone())
        .unwrap_or_else(|| {
            GenerationToken::new_with_str(&format!(
                "{}:{}",
                ctx.search_index_rx_generation(),
                ctx.semantic_index_rx_generation()
            ))
        });
    let snapshot = index.snapshot();
    let content_tokens = query_shape::extract_content_tokens(query);
    let token_refs = content_tokens
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&token_refs);
    let candidate_filter =
        |path: &Path| path_allowed_by_include_tests(path, project_root, include_tests);
    let lexical = CanonicalLexicalLane::from_snapshot(
        &snapshot,
        &query_trigrams,
        Some(&candidate_filter),
        lexical_lane::LEXICAL_ENUMERATION_LIMIT,
    )
    .map_err(|error| error.to_string())?;
    let lexical_scores = lexical
        .canonical_order()
        .iter()
        .map(|candidate| (candidate.result.path.clone(), candidate.raw_score))
        .collect::<HashMap<_, _>>();
    let lexical_candidates = lexical
        .canonical_order()
        .iter()
        .map(|candidate| candidate.result.clone())
        .collect::<Vec<_>>();
    let lexical_verifications = lexical
        .canonical_order()
        .iter()
        .take(lexical_lane::LEXICAL_ENUMERATION_LIMIT)
        .filter_map(|candidate| {
            let (exact, occurrences, window_lines) =
                lexical_candidate_exactness(&candidate.result.path, query, &content_tokens);
            if !exact {
                return None;
            }
            let evidence = if occurrences > 0 {
                EvidenceDescriptor::for_e1(occurrences, true, false)
            } else {
                EvidenceDescriptor::for_e2(window_lines?, true, false)
            };
            Some(CandidateResult::new_exact(
                candidate.result.path.clone(),
                None,
                evidence,
            ))
        })
        .collect::<Vec<_>>();
    let exact_input = plan.exact_input.as_deref().unwrap_or(query);
    let mut exact_candidates =
        if plan.contains(SearchLaneKind::Exact) || plan.shape == SearchShape::Identifier {
            exact_lane::ExactLane::with_memo(ctx.search_exact_memo())
                .search(
                    Some(&index),
                    project_root,
                    generation.clone(),
                    exact_input,
                    include_tests,
                    0,
                    usize::MAX,
                    None,
                )
                .map_err(|error| error.to_string())?
                .results
        } else {
            Vec::new()
        };
    let retain_definition_evidence = plan.shape == SearchShape::Identifier
        || (plan.shape == SearchShape::NaturalLanguage && plan.query_facts.has_identifier_token);
    if !retain_definition_evidence {
        exact_candidates.retain(|candidate| candidate.evidence.kind != EvidenceKind::Definition);
    } else if plan.shape == SearchShape::NaturalLanguage {
        exact_candidates.retain(|candidate| {
            candidate.evidence.kind != EvidenceKind::Definition
                || definition_matches_identifier_token(candidate, query)
        });
    }
    exact_candidates.extend(lexical_verifications);
    exact_candidates.sort_by(score_free_r3_cmp);
    let mut seen_exact = HashSet::new();
    exact_candidates
        .retain(|candidate| seen_exact.insert((candidate.path.clone(), candidate.symbol_range)));

    let path_lookup_candidates = if plan.contains(SearchLaneKind::PathLookup) {
        let query_path_tokens = query
            .split_whitespace()
            .map(|token| {
                token.trim_matches(|ch: char| {
                    !ch.is_alphanumeric()
                        && ch != '.'
                        && ch != '_'
                        && ch != '-'
                        && ch != '/'
                        && ch != '\\'
                })
            })
            .filter(|token| token.contains('.'))
            .collect::<Vec<_>>();
        walk_project_files_from(project_root, project_root, &PathFilters::default())
            .into_iter()
            .filter(|path| candidate_filter(path))
            .filter(|path| {
                let relative = path
                    .strip_prefix(project_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let file_name = path.file_name().and_then(|name| name.to_str());
                query_path_tokens.iter().any(|token| {
                    let normalized = token.replace('\\', "/");
                    file_name == Some(normalized.as_str()) || relative.ends_with(&normalized)
                })
            })
            .map(|path| {
                CandidateResult::new_exact(path, None, EvidenceDescriptor::for_e1(1, true, false))
            })
            .collect()
    } else {
        Vec::new()
    };
    let path_scope =
        (!path_lookup_candidates.is_empty() && plan.query_facts.has_path_token).then(|| {
            path_lookup_candidates
                .iter()
                .map(|candidate| {
                    std::fs::canonicalize(&candidate.path)
                        .unwrap_or_else(|_| candidate.path.clone())
                })
                .collect::<HashSet<_>>()
        });
    if let Some(path_scope) = &path_scope {
        let (mut in_scope, out_of_scope): (Vec<_>, Vec<_>) =
            exact_candidates.into_iter().partition(|candidate| {
                let path = std::fs::canonicalize(&candidate.path)
                    .unwrap_or_else(|_| candidate.path.clone());
                path_scope.contains(&path)
            });
        in_scope.extend(out_of_scope);
        exact_candidates = in_scope;
    }

    let mut semantic_metadata = HashMap::new();
    let mut seen_semantic_paths = HashSet::new();
    let mut prepared_semantic = Vec::new();
    for result in semantic_results {
        let path = result.file.clone();
        semantic_metadata
            .entry(path.clone())
            .or_insert_with(|| HybridResult {
                file: result.file.clone(),
                name: result.name.clone(),
                kind: result.kind,
                start_line: result.start_line,
                end_line: result.end_line,
                exported: result.exported,
                score: result.score,
                source: "semantic",
                semantic_score: Some(result.score),
                lexical_score: None,
                hybrid_boosted: false,
                exact: false,
                exact_phrase_count: 0,
                exact_window_lines: None,
                fusion_score: 0.0,
                snippet: result.snippet.clone(),
            });
        if seen_semantic_paths.insert(path.clone()) {
            prepared_semantic.push(CandidateResult {
                path,
                symbol_range: None,
                evidence: EvidenceDescriptor::for_non_exact(false, false),
                fusion_score: None,
                lane_score: Some(result.score),
                best_lane: Some(SearchLaneKind::Semantic),
            });
        }
    }

    let mut identifier_exact_capped = false;
    let lexical_execution_candidates =
        if plan.shape == SearchShape::Identifier && !plan.contains(SearchLaneKind::Symbol) {
            let fallback_depth = BLOCK_DEPTHS
                .iter()
                .copied()
                .find(|depth| (*depth as u64) >= page_request.interval_end())
                .unwrap_or_else(|| *BLOCK_DEPTHS.last().expect("block depths are non-empty"));
            identifier_exact_capped = exact_candidates.len() > fallback_depth;
            let bounded_exact = exact_candidates
                .iter()
                .take(fallback_depth)
                .cloned()
                .collect::<Vec<_>>();
            let exact_identities = bounded_exact
                .iter()
                .map(|candidate| (candidate.path.clone(), candidate.symbol_range))
                .collect::<HashSet<_>>();
            bounded_exact
                .into_iter()
                .chain(
                    lexical_candidates
                        .iter()
                        .filter(|candidate| {
                            !exact_identities
                                .contains(&(candidate.path.clone(), candidate.symbol_range))
                        })
                        .cloned(),
                )
                .collect()
        } else {
            lexical_candidates.clone()
        };

    let mut registry = LaneRegistry::new();
    for kind in &plan.executed_callbacks {
        let lane: Arc<dyn SearchLane> = match kind {
            SearchLaneKind::Symbol => Arc::new(PreparedEngineLane {
                kind: *kind,
                candidates: exact_candidates.clone(),
            }),
            SearchLaneKind::Exact => Arc::new(PreparedEngineLane {
                kind: *kind,
                candidates: exact_candidates.clone(),
            }),
            SearchLaneKind::Anchored => Arc::new(anchored_lane::AnchoredLane::new()),
            SearchLaneKind::Lexical => Arc::new(PreparedEngineLane {
                kind: *kind,
                candidates: lexical_execution_candidates.clone(),
            }),
            SearchLaneKind::Semantic => Arc::new(PreparedEngineLane {
                kind: *kind,
                candidates: prepared_semantic.clone(),
            }),
            SearchLaneKind::PathLookup => Arc::new(PreparedEngineLane {
                kind: *kind,
                candidates: path_lookup_candidates.clone(),
            }),
            _ => Arc::new(PreparedEngineLane {
                kind: *kind,
                candidates: Vec::new(),
            }),
        };
        register_lane(&mut registry, lane);
    }

    let input = LaneInput {
        query,
        shape: plan.shape,
        root: project_root,
        include_tests,
        index: &index,
    };
    let mut executions = Vec::new();
    let mut callback_counts = HashMap::new();
    for kind in &plan.executed_callbacks {
        let lane = registry
            .get(*kind)
            .ok_or_else(|| format!("selected callback {kind} was not registered"))?;
        let execution = extensions.execute_lane(lane.as_ref(), &input);
        *callback_counts.entry(*kind).or_insert(0usize) += 1;
        if execution.kind != *kind {
            return Err(format!(
                "selected callback {kind} returned execution for {}",
                execution.kind
            ));
        }
        if plan.selected_lanes.contains(kind) {
            executions.push(execution);
        }
    }

    let mut canonical_descriptors = HashMap::new();
    for candidate in executions
        .iter()
        .flat_map(|execution| execution.candidates.iter())
        .filter(|candidate| candidate.evidence.tier == EvidenceTier::NonExact)
    {
        canonical_descriptors
            .entry((candidate.path.clone(), candidate.symbol_range))
            .and_modify(|(exact_form, generated): &mut (bool, bool)| {
                *exact_form |= candidate.evidence.exact_form;
                *generated &= candidate.evidence.generated;
            })
            .or_insert((candidate.evidence.exact_form, candidate.evidence.generated));
    }

    let mut lanes = Vec::new();
    for execution in executions {
        let candidates = execution
            .candidates
            .into_iter()
            .map(|candidate| {
                let is_test = path_is_hidden_test_file(&candidate.path, project_root);
                match candidate.evidence.tier {
                    EvidenceTier::Exact => LaneCandidate::exact(
                        candidate.path,
                        candidate.symbol_range,
                        candidate.evidence,
                        is_test,
                    ),
                    EvidenceTier::NonExact => {
                        let (exact_form, generated) = canonical_descriptors
                            .get(&(candidate.path.clone(), candidate.symbol_range))
                            .copied()
                            .expect("every non-exact candidate has a canonical descriptor");
                        LaneCandidate::non_exact(
                            candidate.path,
                            candidate.symbol_range,
                            EvidenceDescriptor::for_non_exact(exact_form, generated),
                            candidate
                                .lane_score
                                .expect("prepared non-exact candidates carry a raw lane score"),
                            is_test,
                        )
                    }
                }
            })
            .collect();
        lanes.push(
            CanonicalLane::new(execution.kind, candidates).map_err(|error| error.to_string())?,
        );
    }

    let key = CanonicalListKey {
        project_root: project_root.to_path_buf(),
        snapshot_generation: generation.as_str().to_string(),
        normalized_query: exact_lane::normalize_exact_phrase(query),
        include_tests,
    };
    let policy = ScoringPolicy::from_plan_table(&PlanTable::running_table(), plan.shape)
        .map_err(|error| error.to_string())?;
    let builder = BlockBuilder::new(key, policy, lanes).map_err(|error| error.to_string())?;
    let mut page =
        paging::serve_public_page(&builder, page_request).map_err(|error| error.to_string())?;
    if let Some(path_scope) = &path_scope {
        let mut order_index = 0;
        for block in &mut page.reply.canonical_list.blocks {
            let entries = std::mem::take(&mut block.entries);
            let (exact, non_exact): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|entry| entry.result.evidence.tier == EvidenceTier::Exact);
            let (mut exact_in_scope, exact_outside): (Vec<_>, Vec<_>) = exact
                .into_iter()
                .partition(|entry| path_scope_contains(path_scope, &entry.result.path));
            let (mut non_exact_in_scope, non_exact_outside): (Vec<_>, Vec<_>) = non_exact
                .into_iter()
                .partition(|entry| path_scope_contains(path_scope, &entry.result.path));
            exact_in_scope.extend(exact_outside);
            exact_in_scope.append(&mut non_exact_in_scope);
            exact_in_scope.extend(non_exact_outside);
            for entry in &mut exact_in_scope {
                entry.r3_order_index = order_index;
                order_index += 1;
            }
            block.entries = exact_in_scope;
        }
        page.reply.page = page
            .reply
            .canonical_list
            .entries()
            .skip(page_request.offset())
            .take(page_request.top_k())
            .cloned()
            .collect();
    }
    let confidence = ConfidenceEngine::running()
        .evaluate_reply(&page.reply)
        .map_err(|error| error.to_string())?;
    let confidence_telemetry = match confidence.confidence {
        Some(Confidence::High) => Some(ConfidenceTelemetry::High),
        Some(Confidence::Low) => Some(ConfidenceTelemetry::Low),
        None => None,
    };
    let provenance =
        ObservedProvenance::from_reply(&page.reply).map_err(|error| error.to_string())?;
    let structured = TelemetryAssembler::new(&page, provenance)
        .assemble(TelemetryRun {
            shape: plan.shape,
            confidence: confidence_telemetry,
            variants: plan.variants.clone(),
            embedding_calls: crate::search_b2::embed_counter::read(request_id).requested as usize,
            snapshot_generation: generation,
        })
        .map_err(|error| error.to_string())?;
    let mut structured_content =
        serde_json::to_value(structured).map_err(|error| error.to_string())?;
    if let Some(plan_object) = structured_content
        .get_mut("plan")
        .and_then(serde_json::Value::as_object_mut)
    {
        plan_object.insert(
            "callback_counts".to_string(),
            serde_json::Value::Object(
                callback_counts
                    .into_iter()
                    .map(|(lane, count)| (lane.as_str().to_string(), serde_json::json!(count)))
                    .collect(),
            ),
        );
    }
    let trailer = SearchTrailer::from_page(&page, ExactPassState::Complete)
        .map_err(|error| error.to_string())?
        .render();

    let mut results = Vec::with_capacity(page.reply.page.len());
    for entry in &page.reply.page {
        let ranked = &entry.result;
        let semantic_backed = semantic_metadata.contains_key(&ranked.path);
        let mut result = semantic_metadata
            .remove(&ranked.path)
            .unwrap_or_else(|| HybridResult {
                file: ranked.path.clone(),
                name: ranked
                    .path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string(),
                kind: SymbolKind::FileSummary,
                start_line: 0,
                end_line: 0,
                exported: false,
                score: 0.0,
                source: "lexical",
                semantic_score: None,
                lexical_score: None,
                hybrid_boosted: false,
                exact: false,
                exact_phrase_count: 0,
                exact_window_lines: None,
                fusion_score: 0.0,
                snippet: String::new(),
            });
        result.exact = ranked.evidence.tier == EvidenceTier::Exact;
        result.exact_phrase_count = ranked.evidence.occurrences.unwrap_or_default();
        result.exact_window_lines = ranked.evidence.window_lines;
        result.fusion_score = ranked.fusion_score.unwrap_or(1.0);
        result.score = ranked.lane_score.unwrap_or(result.fusion_score);
        result.lexical_score = lexical_scores.get(&ranked.path).copied().or_else(|| {
            entry
                .admitted_contributions
                .iter()
                .find(|contribution| contribution.lane == SearchLaneKind::Lexical)
                .map(|contribution| contribution.raw_score)
        });
        result.hybrid_boosted = semantic_backed && result.lexical_score.is_some();
        result.source = match ranked.evidence.tier {
            EvidenceTier::Exact if semantic_backed => "semantic",
            EvidenceTier::Exact => match ranked.evidence.kind {
                EvidenceKind::Anchored => "anchored",
                _ => "exact",
            },
            EvidenceTier::NonExact => match ranked.best_lane {
                Some(SearchLaneKind::Semantic) => "semantic",
                Some(SearchLaneKind::Lexical) => "lexical",
                _ => "hybrid",
            },
        };
        results.push(result);
    }

    let page_end = page_request.offset().saturating_add(page_request.top_k());
    Ok(EngineRanking {
        results,
        more_available: page_end < page.reply.canonical_list.len()
            || !matches!(page.stop_state, paging::StopState::S2Exhausted),
        engine_capped: identifier_exact_capped
            || matches!(page.stop_state, paging::StopState::S3DepthCap),
        trailer,
        confidence_line: confidence.flat_head_line,
        structured_content,
    })
}

fn handle_engine_only_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    shape: &QueryShape,
    semantic_status: &'static str,
    mut warnings: Vec<String>,
    project_root: &Path,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    plan: &extensions::LanePlan<'_>,
) -> Response {
    if semantic_status != "ready" {
        warnings.push("Semantic search unavailable; using lexical-only fallback.".to_string());
    }
    let mut lexical_plan = plan.clone();
    lexical_plan
        .selected_lanes
        .retain(|lane| *lane != SearchLaneKind::Semantic);
    let mut ranked = match run_engine_ranking(
        &req.id,
        ctx,
        project_root,
        &params.query,
        params.include_tests,
        Vec::new(),
        page_request,
        extensions,
        &lexical_plan,
        None,
    ) {
        Ok(ranked) => ranked,
        Err(error) => return Response::error(&req.id, "search_engine_failed", error),
    };
    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut ranked.results, project_root, Some(ctx));
    let mut text = format_semantic_text(
        &ranked.results,
        project_root,
        ranked.more_available,
        snippets_incomplete,
        Some(ctx),
    );
    if semantic_status == "building" {
        let disclosure = if ctx.shared_artifacts_read_only() {
            BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS
        } else {
            "Semantic index is rebuilding; lexical fallback results follow."
        };
        text = format!("{disclosure}\n\n{text}");
    }
    if let Some(line) = ranked.confidence_line {
        text.push_str("\n\n");
        text.push_str(line);
    }
    text.push_str("\n\n");
    text.push_str(&ranked.trailer);
    let mut extras = serde_json::Map::new();
    extras.insert("structuredContent".to_string(), ranked.structured_content);
    extras.insert(
        "lexical_only_fallback".to_string(),
        serde_json::json!(semantic_status != "ready"),
    );
    extras.insert(
        "semantic_unavailable".to_string(),
        serde_json::json!(semantic_status != "ready"),
    );
    extras.insert(
        "lexical_engine_capped".to_string(),
        serde_json::json!(ranked.engine_capped),
    );
    if semantic_status == "building" {
        extras.insert(
            "note".to_string(),
            serde_json::json!(building_lexical_note(ctx.shared_artifacts_read_only())),
        );
    }
    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: if semantic_status == "ready" {
                "engine"
            } else {
                "lexical"
            },
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: if semantic_status == "building" {
                "building"
            } else {
                "ready"
            },
            complete: semantic_status == "ready",
            text,
            results: ranked.results.iter().map(result_to_json).collect(),
            more_available: ranked.more_available,
            engine_capped: ranked.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn handle_semantic_or_hybrid_search(
    req: &RawRequest,
    ctx: &AppContext,
    params: SemanticSearchParams,
    top_k: usize,
    shape: QueryShape,
    mode: SearchMode,
    lexical_ready: bool,
    status: SemanticIndexStatus,
    semantic_status: &'static str,
    mut warnings: Vec<String>,
    project_root: &Path,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
) -> Response {
    match status {
        SemanticIndexStatus::Disabled => {
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                "disabled",
                "disabled",
                "Semantic search is not enabled.".to_string(),
                "disabled",
                false,
                warnings,
                project_root,
                top_k,
                page_request,
                extensions,
                engine_plan,
            );
        }
        SemanticIndexStatus::Failed(error) => {
            let retrying_read_only_snapshot = ctx.shared_artifacts_read_only()
                && super::configure::trigger_semantic_index_reload_if_evicted(ctx);
            let (semantic_status, status, detail, footer_reason) = if retrying_read_only_snapshot {
                (
                    "building",
                    "reloading",
                    "Semantic index is reloading from the shared read-only snapshot; retry shortly."
                        .to_string(),
                    "reloading",
                )
            } else {
                (
                    "unavailable",
                    "unavailable",
                    format!("Semantic search unavailable: {error}"),
                    "unavailable",
                )
            };
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                semantic_status,
                status,
                detail,
                footer_reason,
                false,
                warnings,
                project_root,
                top_k,
                page_request,
                extensions,
                engine_plan,
            );
        }
        SemanticIndexStatus::Building { .. } => {
            ctx.note_index_query(
                crate::logging::IndexPlane::Semantic,
                "semantic_search",
                0,
                "building",
            );
            if mode == SearchMode::Semantic && !lexical_ready {
                return handle_grep_search(
                    req,
                    ctx,
                    &params.query,
                    params.offset,
                    top_k,
                    &shape,
                    SearchMode::Literal,
                    "building",
                    warnings,
                    project_root,
                    params.include_tests,
                    page_request,
                    extensions,
                    engine_plan,
                );
            }

            return handle_engine_only_search(
                req,
                ctx,
                &params,
                &shape,
                "building",
                warnings,
                project_root,
                page_request,
                extensions,
                engine_plan,
            );
        }
        SemanticIndexStatus::Ready { refreshing, .. } => {
            if !refreshing.is_empty() {
                warnings.push(format!(
                    "{} file(s) refreshing; results for those files may be temporarily missing",
                    refreshing.len()
                ));
            }
            ctx.note_index_query(
                crate::logging::IndexPlane::Semantic,
                "semantic_search",
                0,
                if refreshing.is_empty() {
                    "ok"
                } else {
                    "partial"
                },
            );
        }
    }

    let pinned_semantic_view = ctx.pinned_view_runtime().filter(|view| {
        view.manifest.as_ref().is_some_and(|manifest| {
            manifest.entries().any(|(_, entry)| {
                matches!(
                    entry,
                    crate::views::ManifestEntry::Regular { planes, .. }
                        if planes.semantic.is_some()
                )
            })
        })
    });
    let semantic_loaded = match semantic_index_loaded_with_budget(ctx) {
        Ok(loaded) => loaded,
        Err(()) => {
            return artifact_contention_fallback_response(
                req,
                ctx,
                &params,
                &shape,
                project_root,
                top_k,
                "semantic index remained busy",
            );
        }
    };
    if !semantic_loaded && pinned_semantic_view.is_none() {
        let reloading = super::configure::trigger_semantic_index_reload_if_evicted(ctx);
        let detail = if reloading {
            "Semantic index is reloading; retry shortly."
        } else {
            "Semantic index is not ready yet."
        };
        return semantic_unavailable_or_fallback_response(
            req,
            ctx,
            &params,
            mode,
            &shape,
            "unavailable",
            "not_ready",
            detail.to_string(),
            "not_ready",
            false,
            warnings,
            project_root,
            top_k,
            page_request,
            extensions,
            engine_plan,
        );
    }

    let query_vector = match embed_query(&params.query, ctx) {
        Ok(query_vector) => query_vector,
        Err(error) => {
            if search_cancellation_requested() {
                return cancelled_search_response(req);
            }
            let classified = classify_embed_query_error(&error);
            return semantic_unavailable_or_fallback_response(
                req,
                ctx,
                &params,
                mode,
                &shape,
                "unavailable",
                "unavailable",
                classified.detail,
                classified.footer_reason,
                true,
                warnings,
                project_root,
                top_k,
                page_request,
                extensions,
                engine_plan,
            );
        }
    };
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }

    // Candidate enumeration is fixed across page sizes so every requested
    // interval is cut from the same ranked tuple.
    let semantic_limit = MAX_TOP_K;
    let semantic_fetch_limit = semantic_limit.saturating_add(1);
    let mut semantic_results = if let Some(view) = pinned_semantic_view.as_ref() {
        match view_semantic_search(
            view,
            project_root,
            &query_vector,
            semantic_fetch_limit,
            params.include_tests,
        ) {
            Ok(results) => results,
            Err(error) => {
                warnings.push(format!("view semantic read failed: {error}"));
                Vec::new()
            }
        }
    } else {
        match try_read_with_budget(ctx.semantic_index(), INTERACTIVE_ARTIFACT_READ_BUDGET) {
            Some(semantic_index) => semantic_index
                .as_ref()
                .map(|index| {
                    index.search_filtered(&query_vector, semantic_fetch_limit, |file| {
                        path_allowed_by_include_tests(file, project_root, params.include_tests)
                    })
                })
                .unwrap_or_default(),
            None => {
                return semantic_unavailable_or_fallback_response(
                    req,
                    ctx,
                    &params,
                    mode,
                    &shape,
                    "unavailable",
                    "unavailable",
                    format!(
                        "Semantic search artifacts remained busy beyond {}ms.",
                        INTERACTIVE_ARTIFACT_READ_BUDGET.as_millis()
                    ),
                    "artifact contention",
                    true,
                    warnings,
                    project_root,
                    top_k,
                    page_request,
                    extensions,
                    engine_plan,
                );
            }
        }
    };
    if ctx.shared_artifacts_read_only() {
        semantic_results.retain(|result| result.file.is_file());
    }
    let semantic_more_available = semantic_results.len() > semantic_limit;
    if semantic_more_available {
        semantic_results.truncate(semantic_limit);
    }
    let mut engine_ranking = match run_engine_ranking(
        &req.id,
        ctx,
        project_root,
        &params.query,
        params.include_tests,
        semantic_results,
        page_request,
        extensions,
        engine_plan,
        None,
    ) {
        Ok(ranking) => ranking,
        Err(error) => return Response::error(&req.id, "search_engine_failed", error),
    };
    if ctx.shared_artifacts_read_only() {
        engine_ranking
            .results
            .retain(|result| result.file.is_file());
    }
    let more_available = engine_ranking.more_available || semantic_more_available;
    let mut results = engine_ranking.results;

    if mode == SearchMode::Semantic
        && shape.kind == QueryKind::NaturalLanguage
        && results.is_empty()
        && lexical_ready
    {
        return zero_result_escalation_response(
            req,
            ctx,
            &params.query,
            &shape,
            mode,
            semantic_status,
            warnings,
            params.include_tests,
            project_root,
            project_root,
            serde_json::Map::new(),
            page_request,
            extensions,
            engine_plan,
            None,
        );
    }

    // No score threshold: silent filtering produced "0 results" even when the
    // model had reasonable matches the agent could have judged. Surface every
    // hit so the caller can decide.

    // Read display snippets from source on the fly (top 3 only, rank-budgeted)
    // so both the text rendering and the JSON `results` carry fresh, correctly
    // sized previews. Drives the conditional zoom hint.
    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut results, project_root, Some(ctx));

    let mut text = format_semantic_text(
        &results,
        project_root,
        more_available,
        snippets_incomplete,
        Some(ctx),
    );
    if let Some(line) = engine_ranking.confidence_line {
        text.push_str("\n\n");
        text.push_str(line);
    }
    text.push_str("\n\n");
    text.push_str(&engine_ranking.trailer);
    let mut extras = serde_json::Map::new();
    extras.insert(
        "structuredContent".to_string(),
        engine_ranking.structured_content,
    );

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: true,
            text,
            results: results.iter().map(result_to_json).collect::<Vec<_>>(),
            more_available,
            engine_capped: engine_ranking.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

struct SearchResponseParts<'a> {
    query: &'a str,
    interpreted_as: &'static str,
    query_kind: &'static str,
    semantic_status: &'static str,
    status: &'static str,
    complete: bool,
    text: String,
    results: Vec<serde_json::Value>,
    more_available: bool,
    engine_capped: bool,
    fully_degraded: bool,
    warnings: Vec<String>,
    extras: serde_json::Map<String, serde_json::Value>,
}

impl<'a> SearchResponseParts<'a> {
    fn result_count(&self) -> usize {
        self.results.len()
    }
}

fn zero_result_escalation_disclosure(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Regex => "[interpreted_as: regex; no exact match — ranked by terms instead]",
        SearchMode::Literal => {
            "[interpreted_as: literal; no exact match — ranked by terms instead]"
        }
        SearchMode::Semantic => {
            "[interpreted_as: semantic; no result above cutoff — ranked by terms instead]"
        }
        SearchMode::Hybrid => {
            "[interpreted_as: hybrid; no result — no further escalation available]"
        }
    }
}

/// Render the single lexical second chance used after an auto-routed lane
/// returns no results. The escalation uses the same exact and lexical engine
/// lanes as an ordinary natural-language request.
fn zero_result_escalation_response(
    req: &RawRequest,
    ctx: &AppContext,
    query: &str,
    shape: &QueryShape,
    mode: SearchMode,
    semantic_status: &'static str,
    warnings: Vec<String>,
    include_tests: bool,
    project_root: &Path,
    display_root: &Path,
    mut extras: serde_json::Map<String, serde_json::Value>,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    base_plan: &extensions::LanePlan<'_>,
    borrowed_index: Option<(&SearchIndex, &GenerationToken)>,
) -> Response {
    use extensions::RawQuery;

    let (_, facts) = extensions.classify(&RawQuery::new(query));
    let mut escalation_plan =
        extensions.plan(&SearchShape::NaturalLanguage, &facts, &base_plan.readiness);
    escalation_plan
        .selected_lanes
        .retain(|lane| !matches!(*lane, SearchLaneKind::Semantic | SearchLaneKind::Exact));
    let mut ranked = match run_engine_ranking(
        &req.id,
        ctx,
        project_root,
        query,
        include_tests,
        Vec::new(),
        page_request,
        extensions,
        &escalation_plan,
        borrowed_index,
    ) {
        Ok(ranked) => ranked,
        Err(error) => return Response::error(&req.id, "search_engine_failed", error),
    };
    if borrowed_index.is_some() {
        ranked.results.retain(|result| result.file.is_file());
    }
    let snippets_incomplete =
        enrich_snippets_from_source_with_context(&mut ranked.results, project_root, Some(ctx));
    let mut text = format_semantic_text_with_display_root(
        &ranked.results,
        display_root,
        ranked.more_available,
        snippets_incomplete,
        Some(ctx),
    );
    text.push('\n');
    text.push_str(zero_result_escalation_disclosure(mode));
    if let Some(line) = ranked.confidence_line {
        text.push_str("\n\n");
        text.push_str(line);
    }
    text.push_str("\n\n");
    text.push_str(&ranked.trailer);

    extras.insert(
        "zero_result_escalation".to_string(),
        serde_json::json!(true),
    );
    extras.insert("escalation_target".to_string(), serde_json::json!("hybrid"));
    extras.insert("structuredContent".to_string(), ranked.structured_content);
    search_response(
        req,
        SearchResponseParts {
            query,
            interpreted_as: interpreted_as_label(mode),
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: true,
            text,
            results: ranked
                .results
                .iter()
                .map(result_to_json)
                .collect::<Vec<_>>(),
            more_available: ranked.more_available,
            engine_capped: ranked.engine_capped,
            fully_degraded: false,
            warnings,
            extras,
        },
    )
}

fn search_response(req: &RawRequest, parts: SearchResponseParts<'_>) -> Response {
    if search_cancellation_requested() {
        return cancelled_search_response(req);
    }
    let result_count = parts.result_count();
    let mut object = serde_json::Map::new();
    object.insert("status".to_string(), serde_json::json!(parts.status));
    object.insert("complete".to_string(), serde_json::json!(parts.complete));
    object.insert("text".to_string(), serde_json::json!(parts.text));
    object.insert("query".to_string(), serde_json::json!(parts.query));
    object.insert(
        "interpreted_as".to_string(),
        serde_json::json!(parts.interpreted_as),
    );
    object.insert(
        "query_kind".to_string(),
        serde_json::json!(parts.query_kind),
    );
    object.insert("result_count".to_string(), serde_json::json!(result_count));
    object.insert(
        "results".to_string(),
        serde_json::Value::Array(parts.results),
    );
    object.insert(
        "more_available".to_string(),
        serde_json::json!(parts.more_available),
    );
    object.insert(
        "engine_capped".to_string(),
        serde_json::json!(parts.engine_capped),
    );
    object.insert(
        "fully_degraded".to_string(),
        serde_json::json!(parts.fully_degraded),
    );
    object.insert(
        "semantic_status".to_string(),
        serde_json::json!(parts.semantic_status),
    );
    if !parts.warnings.is_empty() {
        object.insert("warnings".to_string(), serde_json::json!(parts.warnings));
    }
    crate::list_surfaces::search::attach_search_envelope(
        &mut object,
        result_count,
        parts.more_available,
        parts.engine_capped,
    );
    for (key, value) in parts.extras {
        object.insert(key, value);
    }
    Response::success(&req.id, serde_json::Value::Object(object))
}

fn artifact_contention_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    shape: &QueryShape,
    project_root: &Path,
    top_k: usize,
    artifact: &str,
) -> Response {
    semantic_unavailable_grep_fallback_response(
        req,
        ctx,
        params,
        shape,
        "unavailable",
        format!(
            "Search artifacts were busy beyond the {}ms interactive budget ({artifact}).",
            INTERACTIVE_ARTIFACT_READ_BUDGET.as_millis()
        ),
        "artifact contention",
        false,
        Vec::new(),
        project_root,
        top_k,
    )
}

fn semantic_unavailable_or_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    mode: SearchMode,
    shape: &QueryShape,
    semantic_status: &'static str,
    _unavailable_status: &'static str,
    detail: String,
    footer_reason: &str,
    _force_lexical_fallback: bool,
    mut warnings: Vec<String>,
    project_root: &Path,
    top_k: usize,
    page_request: paging::ValidatedPageRequest,
    extensions: &dyn extensions::SearchExtensions,
    engine_plan: &extensions::LanePlan<'_>,
) -> Response {
    if engine_plan.readiness.lexical_index {
        let mut lexical_plan = engine_plan.clone();
        lexical_plan
            .selected_lanes
            .retain(|lane| *lane != SearchLaneKind::Semantic);
        let mut ranked = match run_engine_ranking(
            &req.id,
            ctx,
            project_root,
            &params.query,
            params.include_tests,
            Vec::new(),
            page_request,
            extensions,
            &lexical_plan,
            None,
        ) {
            Ok(ranked) => ranked,
            Err(error) => return Response::error(&req.id, "search_engine_failed", error),
        };
        let snippets_incomplete =
            enrich_snippets_from_source_with_context(&mut ranked.results, project_root, Some(ctx));
        let mut text =
            format_lexical_unavailable_text(&detail, &ranked.results, project_root, footer_reason);
        if snippets_incomplete && !ranked.results.is_empty() {
            text.push_str(
                "\n\nSome snippets were truncated; use read or aft_zoom for full context.",
            );
        }
        if let Some(line) = ranked.confidence_line {
            text.push_str("\n\n");
            text.push_str(line);
        }
        text.push_str("\n\n");
        text.push_str(&ranked.trailer);
        warnings.push(
            "Semantic search unavailable; returning lexical-only fallback results.".to_string(),
        );
        let mut extras = semantic_unavailable_extras(true);
        extras.insert("structuredContent".to_string(), ranked.structured_content);

        return search_response(
            req,
            SearchResponseParts {
                query: &params.query,
                interpreted_as: fallback_executed_label(mode, true),
                query_kind: query_kind_label(shape.kind),
                semantic_status,
                status: "ready",
                complete: false,
                text,
                results: ranked.results.iter().map(result_to_json).collect(),
                more_available: ranked.more_available,
                engine_capped: ranked.engine_capped,
                fully_degraded: false,
                warnings,
                extras,
            },
        );
    }

    semantic_unavailable_grep_fallback_response(
        req,
        ctx,
        params,
        shape,
        semantic_status,
        detail,
        footer_reason,
        false,
        warnings,
        project_root,
        top_k,
    )
}

fn semantic_unavailable_extras(
    lexical_only_fallback: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let mut extras = serde_json::Map::new();
    extras.insert("semantic_unavailable".to_string(), serde_json::json!(true));
    extras.insert(
        "lexical_only_fallback".to_string(),
        serde_json::json!(lexical_only_fallback),
    );
    extras
}

fn semantic_unavailable_grep_fallback_response(
    req: &RawRequest,
    ctx: &AppContext,
    params: &SemanticSearchParams,
    shape: &QueryShape,
    semantic_status: &'static str,
    detail: String,
    footer_reason: &str,
    borrowed_loading: bool,
    mut warnings: Vec<String>,
    project_root: &Path,
    top_k: usize,
) -> Response {
    let fallback = match execute_degraded_grep_fallback(
        &params.query,
        project_root,
        top_k,
        params.include_tests,
        &req.id,
    ) {
        Ok(result) => result,
        Err(response) => return response,
    };
    let result = &fallback.grep;
    let detail = if borrowed_loading && !result.matches.is_empty() {
        BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS.to_string()
    } else {
        detail
    };
    if result.fully_degraded {
        warnings.push(degraded_warning(ctx));
    }
    if fallback.file_cap_reached {
        warnings.push(format!(
            "Degraded grep reached its {}-file scan cap; additional files were not scanned.",
            fallback.file_limit
        ));
    }
    if fallback.walk_budget_reached {
        warnings.push(
            "Degraded grep reached its 10-second walk budget; additional files were not scanned."
                .to_string(),
        );
    }
    warnings
        .push("Semantic search unavailable; returning lexical-only fallback results.".to_string());

    let result_values = result
        .matches
        .iter()
        .map(|grep_match| grep_match_to_json(grep_match, "literal"))
        .collect::<Vec<_>>();
    let more_available = result.truncated
        || result.total_matches > result.matches.len()
        || fallback.file_cap_reached
        || fallback.walk_budget_reached
        || result.skipped_foreign_mounts > 0;
    let mut extras = semantic_unavailable_extras(true);
    if fallback.file_cap_reached || fallback.walk_budget_reached {
        extras.insert(
            "degraded_grep_walk_truncated".to_string(),
            serde_json::json!(true),
        );
    }
    if result.skipped_foreign_mounts > 0 {
        extras.insert(
            "degraded_grep_skipped_foreign_mounts".to_string(),
            serde_json::json!(result.skipped_foreign_mounts),
        );
    }
    if fallback.file_cap_reached {
        extras.insert(
            "degraded_grep_file_limit".to_string(),
            serde_json::json!(fallback.file_limit),
        );
        extras.insert(
            "degraded_grep_candidate_files".to_string(),
            serde_json::json!(fallback.candidate_files),
        );
    }

    search_response(
        req,
        SearchResponseParts {
            query: &params.query,
            // This path ran a literal grep scan over the corpus (the results are
            // GrepLine entries), so report "literal" — not the routed
            // semantic/hybrid mode that never executed.
            interpreted_as: "literal",
            query_kind: query_kind_label(shape.kind),
            semantic_status,
            status: "ready",
            complete: false,
            text: format_grep_lexical_unavailable_text(
                &detail,
                result,
                project_root,
                footer_reason,
            ),
            results: result_values,
            more_available,
            engine_capped: result.engine_capped,
            fully_degraded: result.fully_degraded,
            warnings,
            extras,
        },
    )
}

fn bounded_walk_search_envelope(
    shown: usize,
    more_available: bool,
    engine_capped: bool,
) -> crate::list_envelope::ListEnvelope {
    let mut causes = vec![crate::list_envelope::Reason::Walk];
    if engine_capped {
        causes.push(crate::list_envelope::Reason::Budget);
    }
    if more_available {
        causes.push(crate::list_envelope::Reason::Cap);
    }
    let total = if more_available {
        crate::list_envelope::Total::AtLeast(shown.saturating_add(1))
    } else {
        crate::list_envelope::Total::AtLeast(shown)
    };
    crate::list_envelope::ListEnvelope::new(
        shown,
        total,
        crate::list_envelope::Unit::Results,
        causes,
        crate::list_surfaces::search::SEARCH_NARROW,
    )
}

fn execute_degraded_grep_fallback(
    query: &str,
    project_root: &Path,
    top_k: usize,
    include_tests: bool,
    request_id: &str,
) -> Result<DegradedGrepFallbackResult, Response> {
    let compiled = match pattern_compile::compile(
        query,
        CompileOpts {
            literal: true,
            ..CompileOpts::default()
        },
    ) {
        CompileResult::Ok(compiled) => compiled,
        CompileResult::InvalidPattern { message, .. } => {
            return Err(Response::error_with_data(
                request_id,
                "invalid_pattern",
                message,
                serde_json::json!({"pattern": query}),
            ));
        }
        CompileResult::UnsupportedSyntax { feature, .. } => {
            return Err(Response::error_with_data(
                request_id,
                "unsupported_pattern",
                format!(
                    "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
                ),
                serde_json::json!({"pattern": query, "feature": feature}),
            ));
        }
    };

    let max_results = top_k.clamp(1, DEGRADED_GREP_RESULT_LIMIT);
    let started = Instant::now();
    let (files, file_cap_reached, walk_budget_reached, skipped_foreign_mounts) =
        collect_degraded_grep_files(project_root, include_tests, started);
    if search_cancellation_requested() {
        return Err(cancelled_search_response_from_id(request_id));
    }
    let candidate_files = files.len();
    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut files_searched = 0usize;
    let mut files_with_matches = 0usize;
    let mut truncated = false;
    let mut engine_capped = file_cap_reached || walk_budget_reached;

    let read_budget_reached = AtomicBool::new(walk_budget_reached);
    let cancellation = crate::executor::current_job_cancellation();
    let mut readable_files = files
        .par_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            if cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
            {
                return None;
            }
            if started.elapsed() >= DEGRADED_GREP_WALK_BUDGET {
                read_budget_reached.store(true, Ordering::Relaxed);
                return None;
            }
            crate::search_index::read_searchable_text(file)
                .map(|content| (index, file.clone(), content))
        })
        .collect::<Vec<_>>();
    if search_cancellation_requested() {
        return Err(cancelled_search_response_from_id(request_id));
    }
    // Rayon collection order is not part of the response contract; restore the
    // original walker order before applying the existing result-cap semantics.
    readable_files.sort_by_key(|(index, _, _)| *index);

    for (_, file, content) in readable_files {
        if search_cancellation_requested() {
            return Err(cancelled_search_response_from_id(request_id));
        }
        if truncated {
            engine_capped = true;
            break;
        }

        files_searched += 1;

        if search_degraded_grep_file(
            &file,
            &content,
            &compiled,
            max_results,
            &mut total_matches,
            &mut truncated,
            &mut matches,
        ) {
            files_with_matches += 1;
        }
    }

    if truncated {
        engine_capped = true;
    }
    sort_grep_matches_by_mtime_desc(&mut matches, project_root);

    let walk_budget_reached = read_budget_reached.load(Ordering::Relaxed);
    Ok(DegradedGrepFallbackResult {
        grep: GrepResult {
            matches,
            total_matches,
            files_searched,
            files_with_matches,
            index_status: IndexStatus::Fallback,
            truncated,
            fully_degraded: true,
            engine_capped,
            walk_truncated: walk_budget_reached,
            skipped_foreign_mounts,
        },
        file_cap_reached,
        file_limit: DEGRADED_GREP_FILE_LIMIT,
        candidate_files,
        walk_budget_reached,
    })
}

fn collect_degraded_grep_files(
    project_root: &Path,
    include_tests: bool,
    started: Instant,
) -> (Vec<PathBuf>, bool, bool, usize) {
    // Keep degraded semantic search on the root filesystem: ReadDir::drop can
    // abort the daemon if a disappearing child mount reports ENXIO.
    let skipped_foreign_mounts = Arc::new(AtomicUsize::new(0));
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(project_root).ok();
    let walker = ignore::WalkBuilder::new(project_root)
        .same_file_system(true)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry({
            let skipped_foreign_mounts = Arc::clone(&skipped_foreign_mounts);
            move |entry| {
                if entry.depth() > 0
                    && entry
                        .file_type()
                        .is_some_and(|file_type| file_type.is_dir())
                    && matches!(
                        boundary
                            .as_ref()
                            .map(|boundary| boundary.should_descend(entry.path())),
                        Some(Ok(false))
                    )
                {
                    skipped_foreign_mounts.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                let name = entry.file_name().to_string_lossy();
                if entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_dir())
                {
                    return !matches!(
                        name.as_ref(),
                        "node_modules"
                            | "target"
                            | "venv"
                            | ".venv"
                            | ".git"
                            | "__pycache__"
                            | ".tox"
                            | "dist"
                            | "build"
                    );
                }
                true
            }
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.filter_map(Result::ok) {
        if search_cancellation_requested() {
            return (
                files,
                false,
                true,
                skipped_foreign_mounts.load(Ordering::Relaxed),
            );
        }
        if started.elapsed() >= DEGRADED_GREP_WALK_BUDGET {
            return (
                files,
                false,
                true,
                skipped_foreign_mounts.load(Ordering::Relaxed),
            );
        }
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let path = entry.into_path();
        if !include_tests && path_is_hidden_test_file(&path, project_root) {
            continue;
        }
        if files.len() >= DEGRADED_GREP_FILE_LIMIT {
            return (
                files,
                true,
                false,
                skipped_foreign_mounts.load(Ordering::Relaxed),
            );
        }
        files.push(path);
    }

    (
        files,
        false,
        false,
        skipped_foreign_mounts.load(Ordering::Relaxed),
    )
}

fn search_degraded_grep_file(
    file: &Path,
    content: &str,
    compiled: &pattern_compile::CompiledPattern,
    max_results: usize,
    total_matches: &mut usize,
    truncated: &mut bool,
    matches: &mut Vec<GrepMatch>,
) -> bool {
    let line_starts = grep_executor::line_starts(content);
    let mut seen_lines = HashSet::new();
    let mut matched_this_file = false;

    match compiled {
        pattern_compile::CompiledPattern::Literal(literal) => {
            let Some(needle) = std::str::from_utf8(&literal.needle).ok() else {
                return false;
            };
            let haystack = if literal.case_insensitive_ascii {
                Cow::Owned(content.to_ascii_lowercase())
            } else {
                Cow::Borrowed(content)
            };

            for (offset, matched) in haystack.match_indices(needle) {
                if search_cancellation_requested() {
                    break;
                }
                let match_text = content[offset..offset + matched.len()].to_string();
                let (counted, should_continue) = record_degraded_grep_match(
                    file,
                    content,
                    &line_starts,
                    &mut seen_lines,
                    offset,
                    match_text,
                    max_results,
                    total_matches,
                    truncated,
                    matches,
                );
                matched_this_file |= counted;
                if !should_continue {
                    break;
                }
            }
        }
        pattern_compile::CompiledPattern::Regex { compiled, .. } => {
            for matched in compiled.find_iter(content.as_bytes()) {
                if search_cancellation_requested() {
                    break;
                }
                let (counted, should_continue) = record_degraded_grep_match(
                    file,
                    content,
                    &line_starts,
                    &mut seen_lines,
                    matched.start(),
                    String::from_utf8_lossy(matched.as_bytes()).into_owned(),
                    max_results,
                    total_matches,
                    truncated,
                    matches,
                );
                matched_this_file |= counted;
                if !should_continue {
                    break;
                }
            }
        }
    }

    matched_this_file
}

fn record_degraded_grep_match(
    file: &Path,
    content: &str,
    line_starts: &[usize],
    seen_lines: &mut HashSet<u32>,
    offset: usize,
    match_text: String,
    max_results: usize,
    total_matches: &mut usize,
    truncated: &mut bool,
    matches: &mut Vec<GrepMatch>,
) -> (bool, bool) {
    let (line, column, line_text) = grep_executor::line_details(content, line_starts, offset);
    if !seen_lines.insert(line) {
        return (false, true);
    }

    *total_matches += 1;
    if matches.len() >= max_results {
        *truncated = true;
        return (true, false);
    }

    matches.push(GrepMatch {
        file: file.to_path_buf(),
        line,
        column,
        line_text,
        match_text,
    });
    (true, true)
}

fn semantic_index_loaded_with_budget(ctx: &AppContext) -> Result<bool, ()> {
    let semantic_index =
        try_read_with_budget(ctx.semantic_index(), INTERACTIVE_ARTIFACT_READ_BUDGET).ok_or(())?;
    Ok(semantic_index.is_some())
}

fn search_index_ready_with_budget(
    ctx: &AppContext,
    wait_budget: Duration,
) -> Result<bool, SearchIndexWaitError> {
    let deadline = Instant::now() + wait_budget;
    loop {
        if search_cancellation_requested() {
            return Err(SearchIndexWaitError::Cancelled);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let read_budget = remaining.min(SEARCH_INDEX_LOAD_WAIT_POLL_INTERVAL);
        let Some(search_index) = try_read_with_budget(ctx.search_index(), read_budget) else {
            if Instant::now() >= deadline {
                return Err(SearchIndexWaitError::Contended);
            }
            continue;
        };
        if search_index.as_ref().is_some_and(|index| index.ready) {
            return Ok(true);
        }
        drop(search_index);

        // The loader publishes through a channel; the query holds neither the
        // index nor receiver lock while draining, so it cannot block publication.
        let Some(search_receiver) = try_read_with_budget(ctx.search_index_rx(), read_budget) else {
            if Instant::now() >= deadline {
                return Err(SearchIndexWaitError::Contended);
            }
            continue;
        };
        let load_in_progress = search_receiver.is_some();
        drop(search_receiver);
        if !load_in_progress {
            return Ok(false);
        }

        crate::runtime_drain::drain_search_index_events(ctx);
        if search_cancellation_requested() {
            return Err(SearchIndexWaitError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(remaining.min(SEARCH_INDEX_LOAD_WAIT_POLL_INTERVAL));
    }
}

fn search_index_ready(ctx: &AppContext) -> bool {
    search_index_ready_with_budget(ctx, INTERACTIVE_ARTIFACT_READ_BUDGET).unwrap_or(false)
}

fn embed_query(query: &str, ctx: &AppContext) -> Result<Vec<f32>, String> {
    let index_dimension = {
        let semantic_index =
            try_read_with_budget(ctx.semantic_index(), INTERACTIVE_ARTIFACT_READ_BUDGET)
                .ok_or_else(|| {
                    format!(
                        "semantic index remained busy beyond {}ms",
                        INTERACTIVE_ARTIFACT_READ_BUDGET.as_millis()
                    )
                })?;
        semantic_index
            .as_ref()
            .filter(|index| index.len() > 0)
            .map(|index| index.dimension())
    };
    embed_query_for_dimension(query, ctx, index_dimension)
}

fn embed_query_for_dimension(
    query: &str,
    ctx: &AppContext,
    index_dimension: Option<usize>,
) -> Result<Vec<f32>, String> {
    let semantic_config = ctx.config().semantic.clone();
    let query_budget = QueryBudget::from_config(&semantic_config);
    let mut model_ref = ctx.semantic_embedding_model().lock();

    if model_ref.is_none() {
        drop(model_ref);

        let constructed_model = EmbeddingModel::from_config_for_query(&semantic_config)?;

        model_ref = ctx.semantic_embedding_model().lock();
        if model_ref.is_none() {
            *model_ref = Some(constructed_model);
        } else {
            drop(model_ref);
            {
                let _discarded_model = constructed_model;
            }
            model_ref = ctx.semantic_embedding_model().lock();
        }
    }

    let model = model_ref
        .as_mut()
        .ok_or_else(|| "embedding model was not initialized".to_string())?;
    // Preserve the raw error so the timeout marker injected by
    // `send_embedding_request` survives — `classify_embed_query_error` reads it
    // to decide whether the configured query budget fired.
    let query_vector = model
        .embed_query_cached(query, query_budget)
        .map_err(|error| format!("failed to embed query: {error}"))?;
    drop(model_ref);

    if let Some(index_dimension) = index_dimension {
        if index_dimension != query_vector.len() {
            return Err(format!(
                "semantic embedding dimension mismatch: query backend returned {}, index expects {}. Rebuild the semantic index for the active backend/model.",
                query_vector.len(),
                index_dimension
            ));
        }
    }

    Ok(query_vector)
}

/// Classified query-embedding failure: the user-facing `detail` line that names
/// the mechanism and the remedy when the configured query budget fired, and a
/// short `footer_reason` token for the `[semantic: ...]` status footer the agent
/// sees. Non-timeout errors keep the current message shape and an `unavailable`
/// footer.
///
/// The timeout case is detected via the stable marker
/// [`crate::semantic_index::query_embedding_timeout_budget`] injects at the one
/// site that knows both the typed reqwest error and the Query budget — never by
/// substring-matching reqwest's rendered text, which varies by backend/locale.
struct ClassifiedEmbedQueryError {
    detail: String,
    footer_reason: &'static str,
}

fn classify_embed_query_error(error: &str) -> ClassifiedEmbedQueryError {
    if let Some(timeout_ms) = query_embedding_timeout_budget(error) {
        let clean = strip_query_embedding_timeout_marker(error);
        // Name the mechanism (the budget fired) and the remedy (raise the knob
        // for slow providers) so the agent can act, not just observe.
        let detail = format!(
            "Semantic search unavailable: query embedding timed out after {timeout_ms}ms (semantic.query_timeout_ms; raise it for slow providers). {clean}"
        );
        let footer_reason =
            Box::leak(format!("query embed timeout ({timeout_ms}ms)").into_boxed_str());
        ClassifiedEmbedQueryError {
            detail,
            footer_reason,
        }
    } else {
        ClassifiedEmbedQueryError {
            detail: format!("Semantic search unavailable: {error}"),
            footer_reason: "unavailable",
        }
    }
}

fn rerank_semantic_candidates(results: &mut Vec<SemanticResult>, shape: &QueryShape, query: &str) {
    let (tokens, allow_case_fold) = semantic_rerank_tokens(query, shape);
    let type_concept = query_shape::is_type_concept_identifier_query(query, shape);
    let apply_definition_priors = shape.kind == QueryKind::NaturalLanguage || type_concept;
    let kind_prior_strength = semantic_kind_prior_strength(shape, apply_definition_priors);

    for result in results.iter_mut() {
        result.rank_score = result.score;
        result.cap_protected = false;
        result.rank_score *= semantic_kind_multiplier(&result.kind, kind_prior_strength);

        if !tokens.is_empty()
            && is_definition_kind(&result.kind)
            && tokens
                .iter()
                .any(|token| token_matches_candidate_name(token, result, allow_case_fold))
        {
            result.rank_score *= EXACT_NAME_DEFINITION_BOOST;
            if result.score >= P2_CAP_PROTECTED_COSINE_FLOOR {
                result.cap_protected = true;
            }
        }
    }

    if apply_definition_priors {
        apply_natural_language_diversity_cap(results);
    }
}

#[derive(Clone, Copy)]
enum SemanticKindPriorStrength {
    NaturalLanguage,
    Mixed,
    Inert,
}

fn semantic_kind_prior_strength(
    shape: &QueryShape,
    apply_definition_priors: bool,
) -> SemanticKindPriorStrength {
    if apply_definition_priors {
        SemanticKindPriorStrength::NaturalLanguage
    } else if shape.kind == QueryKind::Mixed {
        SemanticKindPriorStrength::Mixed
    } else {
        SemanticKindPriorStrength::Inert
    }
}

fn semantic_rerank_tokens(query: &str, shape: &QueryShape) -> (Vec<String>, bool) {
    match shape.kind {
        QueryKind::Identifier => (query_shape::extract_tokens(query, shape), false),
        QueryKind::Mixed => (query_shape::extract_tokens(query, shape), true),
        QueryKind::NaturalLanguage => (query_shape::extract_explicit_code_tokens(query), true),
        QueryKind::Path | QueryKind::ErrorCode | QueryKind::Regex => (Vec::new(), false),
    }
}

fn semantic_kind_multiplier(kind: &SymbolKind, strength: SemanticKindPriorStrength) -> f32 {
    match strength {
        SemanticKindPriorStrength::NaturalLanguage => match kind {
            SymbolKind::Function
            | SymbolKind::Kernel
            | SymbolKind::Class
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias => 1.08,
            SymbolKind::Variable => 0.92,
            SymbolKind::FileSummary => 0.80,
            SymbolKind::Heading => 1.0,
        },
        SemanticKindPriorStrength::Mixed => match kind {
            SymbolKind::Function
            | SymbolKind::Kernel
            | SymbolKind::Class
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias => 1.03,
            SymbolKind::FileSummary => 0.90,
            SymbolKind::Variable | SymbolKind::Heading => 1.0,
        },
        SemanticKindPriorStrength::Inert => 1.0,
    }
}

fn is_definition_kind(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function
            | SymbolKind::Class
            | SymbolKind::Method
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::TypeAlias
    )
}

fn token_matches_candidate_name(
    token: &str,
    result: &SemanticResult,
    allow_case_fold: bool,
) -> bool {
    names_equal(token, &result.name, allow_case_fold)
        || result
            .qualified_name
            .as_deref()
            .is_some_and(|qualified_name| names_equal(token, qualified_name, allow_case_fold))
}

fn names_equal(token: &str, name: &str, allow_case_fold: bool) -> bool {
    token == name || (allow_case_fold && token.eq_ignore_ascii_case(name))
}

fn apply_natural_language_diversity_cap(results: &mut Vec<SemanticResult>) {
    let mut cluster_counts: HashMap<(String, SymbolKind), usize> = HashMap::new();
    results.retain(|result| {
        let key = (
            result
                .qualified_name
                .as_deref()
                .unwrap_or(&result.name)
                .to_string(),
            result.kind.clone(),
        );
        let count = cluster_counts.entry(key).or_insert(0);
        if *count < NATURAL_LANGUAGE_CLUSTER_CAP {
            *count += 1;
            true
        } else {
            false
        }
    });
}

fn format_lexical_unavailable_text(
    detail: &str,
    results: &[HybridResult],
    project_root: &Path,
    footer_reason: &str,
) -> String {
    if results.is_empty() {
        return format!(
            "{detail}\n0 lexical matches; the semantic lane is unavailable ({footer_reason}), so prose-style queries may match only via semantic. Retry in a few seconds. [semantic: {footer_reason}]"
        );
    }

    format!(
        "{detail}\nSemantic search unavailable; returning lexical-only fallback results.\n\n{}\n\nFound {} lexical fallback result(s). [semantic: {footer_reason}]",
        format_result_sections(results, project_root),
        results.len()
    )
}

fn format_grep_lexical_unavailable_text(
    detail: &str,
    result: &GrepResult,
    project_root: &Path,
    footer_reason: &str,
) -> String {
    if result.matches.is_empty() {
        return format!(
            "{detail}\n0 lexical matches; the semantic lane is unavailable ({footer_reason}), so prose-style queries may match only via semantic. Retry in a few seconds. [semantic: {footer_reason}]"
        );
    }

    format!(
        "{detail}\nSemantic search unavailable; returning lexical-only fallback results.\n\n{}\n\nFound {} lexical fallback result(s). [semantic: {footer_reason}]",
        crate::commands::grep::format_grep_text(result, project_root),
        result.matches.len()
    )
}

fn building_lexical_note(borrowed_loading_with_results: bool) -> &'static str {
    if borrowed_loading_with_results {
        BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS
    } else {
        "Semantic index is rebuilding; results are lexical-only fallback results from the trigram index."
    }
}

/// Top semantic cosine below this floor means the embedder found nothing
/// genuinely relevant — the query likely whiffed. We don't show the raw score
/// (uncalibrated for ranking), but its absolute floor is a real signal: an
/// all-weak result set looks identical to a strong one without it.
const WEAK_MATCH_COSINE_FLOOR: f32 = 0.35;
const P2_CAP_PROTECTED_COSINE_FLOOR: f32 = WEAK_MATCH_COSINE_FLOOR;
const EXACT_NAME_DEFINITION_BOOST: f32 = 1.20;
const NATURAL_LANGUAGE_CLUSTER_CAP: usize = 2;
// Intentionally high: the default MiniLM scores are uncalibrated, so under-trigger rather than over-promise.
const HIGH_CONFIDENCE_COSINE_FLOOR: f32 = 0.60;

/// True when the best result's raw semantic cosine is below the weak floor.
/// Uses `semantic_score` (the raw cosine), not the fused `score`. Lexical-only
/// top results have no cosine and are not flagged here (lexical relevance is
/// judged differently).
fn results_are_low_confidence(results: &[HybridResult]) -> bool {
    results
        .first()
        .and_then(|r| r.semantic_score)
        .is_some_and(|cosine| cosine < WEAK_MATCH_COSINE_FLOOR)
}

fn format_semantic_text(
    results: &[HybridResult],
    project_root: &Path,
    more_available: bool,
    snippets_incomplete: bool,
    ctx: Option<&AppContext>,
) -> String {
    format_semantic_text_with_display_root(
        results,
        project_root,
        more_available,
        snippets_incomplete,
        ctx,
    )
}

fn format_semantic_text_with_display_root(
    results: &[HybridResult],
    display_root: &Path,
    more_available: bool,
    snippets_incomplete: bool,
    ctx: Option<&AppContext>,
) -> String {
    if results.is_empty() {
        return "Found 0 results.".to_string();
    }

    let mut text = format_result_sections_with_context(results, display_root, ctx);
    // Drop the unconditional "[index: ready]" tag — it was pure per-call tax on
    // the common path. Degraded/building/unavailable paths carry their own
    // distinct "[semantic: ...]" labels, so absence of a label means ready.
    text.push_str(&format!("\n\nFound {} result(s).", results.len()));
    if more_available {
        text.push_str(" More results available; raise topK to see more.");
    }
    // Recover the "did the search whiff" signal we lost by hiding the score:
    // one coarse flag when the top match is weak, so the agent reformulates or
    // falls back to grep instead of trusting a uniformly-weak ranking.
    if results_are_low_confidence(results) {
        text.push_str("\nTop match is weak — consider rephrasing or using grep for exact terms.");
    }
    // Only when snippet content was actually withheld (omitted for rank 4+, or
    // truncated within the top 3) — so the hint appears exactly when it's
    // actionable, not on every search.
    if snippets_incomplete {
        if ctx.map_or(true, |ctx| ctx.tool_enabled("aft_zoom")) {
            text.push_str("\nZoom any result for full source: aft_zoom <file> <symbol>.");
        } else {
            text.push_str("\nRead any result for full source: read <file> [startLine..endLine].");
        }
    }
    text
}

fn format_grep_search_text(
    result: &GrepResult,
    project_root: &Path,
    interpreted_as: &str,
) -> String {
    let base = crate::commands::grep::format_grep_text(result, project_root);
    format!("{base}\n[interpreted_as: {interpreted_as}]")
}

/// Snippet line budget by global rank (0-based). The fused score is an
/// uncalibrated, scale-mixed artifact (raw cosine for semantic-only hits,
/// cosine×boost for lexically-co-matched hits), so it is NOT shown to the
/// agent — position conveys rank. We spend snippet tokens by rank instead: the
/// top hit is disproportionately likely to be the final answer (a fuller
/// preview there can save a follow-up aft_zoom), tail hits only need to be
/// identifiable. Snippets are limited to the top 3; rank 4+ shows the symbol
/// header only and the agent zooms the ones it cares about.
fn snippet_line_budget(global_rank: usize) -> usize {
    match global_rank {
        // Rank 0 gets a fuller preview: 10 lines was often half a real function,
        // forcing a zoom anyway and defeating the "preview saves a follow-up"
        // goal. 20 (capped at the symbol's real length) clears most functions.
        0 => 20,
        1 | 2 => 5,
        _ => 0,
    }
}

/// Replace each result's display snippet with source lines read on the fly from
/// disk, bounded by the rank budget. Snippets are display-only (they never
/// affect embeddings), so reading them at query time keeps the on-disk index
/// free of display text, lets snippet sizing change without a re-index, and
/// shows the current file content instead of whatever was captured at index
/// time. Only the top 3 carry snippets; rank 4+ get a header only and the agent
/// zooms the ones it cares about. Lexical rows keep their placeholder and file
/// summaries keep the generated summary (not source lines). Returns true when
/// any snippet was truncated or omitted, so the caller emits the zoom hint only
/// when it is actionable.
#[cfg(test)]
fn enrich_snippets_from_source(results: &mut [HybridResult], project_root: &Path) -> bool {
    enrich_snippets_from_source_with_context(results, project_root, None)
}

#[derive(Debug, Clone, Copy, Default)]
struct SnippetReadPlan {
    fixed_last_line: Option<usize>,
    summary_nonempty_lines: usize,
}

impl SnippetReadPlan {
    fn include_through(&mut self, line: u32) {
        let line = line as usize;
        self.fixed_last_line = Some(
            self.fixed_last_line
                .map_or(line, |current| current.max(line)),
        );
    }

    fn include_summary_lines(&mut self, count: usize) {
        self.summary_nonempty_lines = self.summary_nonempty_lines.max(count);
    }

    fn is_satisfied(&self, line_index: usize, nonempty_lines: usize) -> bool {
        self.fixed_last_line
            .is_none_or(|last_line| line_index >= last_line)
            && nonempty_lines >= self.summary_nonempty_lines
    }
}

fn snippet_read_plans(
    results: &[HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> (HashMap<PathBuf, SnippetReadPlan>, HashMap<usize, Symbol>) {
    let mut plans = HashMap::<PathBuf, SnippetReadPlan>::new();
    let mut rank0_targets = HashMap::new();

    for (rank, result) in results.iter().enumerate() {
        if result.source == "lexical" {
            continue;
        }

        let budget = snippet_line_budget(rank);
        if budget == 0 {
            continue;
        }

        let plan = plans.entry(result.file.clone()).or_default();
        if matches!(result.kind, SymbolKind::FileSummary) {
            plan.include_summary_lines(budget);
            continue;
        }

        plan.include_through(result.end_line);
        if should_expand_rank0_snippet(rank, result, project_root) {
            let target =
                symbol_for_rank0_render(result, ctx).unwrap_or_else(|| symbol_from_result(result));
            plan.include_through(target.range.end_line);
            rank0_targets.insert(rank, target);
        }
    }

    (plans, rank0_targets)
}

fn read_bounded_snippet_lines(path: &Path, plan: SnippetReadPlan) -> Option<Vec<String>> {
    let file = fs::File::open(path).ok()?;
    let mut lines = Vec::new();
    let mut nonempty_lines = 0usize;

    // Read each source once and stop after every requested symbol range and
    // FileSummary budget is satisfied. Any error before that boundary discards
    // the whole partial read, matching the previous read_to_string behavior.
    for (line_index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.ok()?;
        if !line.trim().is_empty() {
            nonempty_lines += 1;
        }
        lines.push(line);

        if plan.is_satisfied(line_index, nonempty_lines) {
            break;
        }
    }

    Some(lines)
}

fn enrich_snippets_from_source_with_context(
    results: &mut [HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> bool {
    let (plans, rank0_targets) = snippet_read_plans(results, project_root, ctx);
    let file_lines = plans
        .into_iter()
        .map(|(path, plan)| {
            let lines = read_bounded_snippet_lines(&path, plan);
            (path, lines)
        })
        .collect::<HashMap<_, _>>();
    let mut incomplete = false;

    for (rank, result) in results.iter_mut().enumerate() {
        if result.source == "lexical" {
            continue;
        }

        let budget = snippet_line_budget(rank);
        if budget == 0 {
            // Header-only tier: a real body means there is more to see.
            if result.end_line >= result.start_line {
                incomplete = true;
            }
            result.snippet = String::new();
            continue;
        }

        let lines = file_lines
            .get(&result.file)
            .and_then(|lines| lines.as_ref());

        if matches!(result.kind, SymbolKind::FileSummary) {
            if let Some(lines) = lines {
                result.snippet = lines
                    .iter()
                    .filter(|line| !line.trim().is_empty())
                    .take(budget)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            continue;
        }

        let Some(lines) = lines else {
            // File unreadable or gone — no snippet beats a stale one.
            result.snippet = String::new();
            continue;
        };

        // start_line/end_line are 0-based inclusive; +1 makes an exclusive bound.
        let start = (result.start_line as usize).min(lines.len());
        let end = ((result.end_line as usize) + 1).min(lines.len());
        if start >= end {
            result.snippet = String::new();
            continue;
        }

        if should_expand_rank0_snippet(rank, result, project_root) {
            let rendered =
                render_rank0_symbol_snippet(result, lines, ctx, rank0_targets.get(&rank));
            match rendered.status {
                BudgetedSymbolRenderStatus::Complete => {
                    // Append the full-body notice only for Complete so callers know
                    // they received the entire symbol source. Skip it for Truncated
                    // or Menu results.
                    result.snippet = append_rank0_full_symbol_notice(rendered.content);
                    continue;
                }
                BudgetedSymbolRenderStatus::Truncated | BudgetedSymbolRenderStatus::Menu => {
                    result.snippet = rendered.content;
                    incomplete = true;
                    continue;
                }
            }
        }

        let range_len = end - start;
        let shown = range_len.min(budget);
        let mut snippet = lines[start..start + shown].join("\n");
        let remaining = range_len - shown;
        if remaining > 0 {
            // "lines" is load-bearing: a bare "+N more" reads as "N more
            // results" to a weak model, prompting a wrong topK bump. This is
            // N more lines of THIS symbol's body — zoom to see them.
            snippet.push_str(&format!("\n+{remaining} more lines"));
            incomplete = true;
        }
        result.snippet = snippet;
    }

    incomplete
}

#[cfg(test)]
fn enrich_snippets_from_source_reference(
    results: &mut [HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> bool {
    let mut file_lines: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    let mut incomplete = false;

    for (rank, result) in results.iter_mut().enumerate() {
        if result.source == "lexical" {
            continue;
        }

        let budget = snippet_line_budget(rank);
        if budget == 0 {
            if result.end_line >= result.start_line {
                incomplete = true;
            }
            result.snippet = String::new();
            continue;
        }

        let lines = file_lines.entry(result.file.clone()).or_insert_with(|| {
            fs::read_to_string(&result.file)
                .ok()
                .map(|content| content.lines().map(str::to_string).collect())
        });

        if matches!(result.kind, SymbolKind::FileSummary) {
            if let Some(lines) = lines {
                result.snippet = lines
                    .iter()
                    .filter(|line| !line.trim().is_empty())
                    .take(budget)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            continue;
        }

        let Some(lines) = lines else {
            result.snippet = String::new();
            continue;
        };

        let start = (result.start_line as usize).min(lines.len());
        let end = ((result.end_line as usize) + 1).min(lines.len());
        if start >= end {
            result.snippet = String::new();
            continue;
        }

        if should_expand_rank0_snippet(rank, result, project_root) {
            let rendered = render_rank0_symbol_snippet(result, lines, ctx, None);
            match rendered.status {
                BudgetedSymbolRenderStatus::Complete => {
                    result.snippet = append_rank0_full_symbol_notice(rendered.content);
                    continue;
                }
                BudgetedSymbolRenderStatus::Truncated | BudgetedSymbolRenderStatus::Menu => {
                    result.snippet = rendered.content;
                    incomplete = true;
                    continue;
                }
            }
        }

        let range_len = end - start;
        let shown = range_len.min(budget);
        let mut snippet = lines[start..start + shown].join("\n");
        let remaining = range_len - shown;
        if remaining > 0 {
            snippet.push_str(&format!("\n+{remaining} more lines"));
            incomplete = true;
        }
        result.snippet = snippet;
    }

    incomplete
}

fn render_rank0_symbol_snippet(
    result: &HybridResult,
    lines: &[String],
    ctx: Option<&AppContext>,
    planned_target: Option<&Symbol>,
) -> crate::commands::symbol_render::BudgetedSymbolRender {
    let fallback_target;
    let target = match planned_target {
        Some(target) => target,
        None => {
            fallback_target =
                symbol_for_rank0_render(result, ctx).unwrap_or_else(|| symbol_from_result(result));
            &fallback_target
        }
    };
    let outline = ctx.and_then(|ctx| {
        if might_have_container_members(target) {
            build_container_outline(ctx, &result.file, target).ok()
        } else {
            None
        }
    });

    render_symbol_within_budget(
        target,
        lines,
        crate::parser::detect_language(&result.file),
        outline.as_ref(),
        RANK0_FULL_SNIPPET_MAX_LINES,
        ctx.map_or(true, |ctx| ctx.tool_enabled("aft_zoom")),
    )
}

fn symbol_for_rank0_render(ctx_result: &HybridResult, ctx: Option<&AppContext>) -> Option<Symbol> {
    let symbols = ctx?.provider().list_symbols(&ctx_result.file).ok()?;
    symbols
        .iter()
        .find(|symbol| symbol_matches_result(symbol, ctx_result, true))
        .cloned()
        .or_else(|| {
            symbols
                .into_iter()
                .find(|symbol| symbol_matches_result(symbol, ctx_result, false))
        })
}

fn symbol_matches_result(symbol: &Symbol, result: &HybridResult, exact_range: bool) -> bool {
    symbol.name == result.name
        && symbol.kind == result.kind
        && (!exact_range
            || (symbol.range.start_line == result.start_line
                && symbol.range.end_line == result.end_line))
}

fn symbol_from_result(result: &HybridResult) -> Symbol {
    Symbol {
        name: result.name.clone(),
        kind: result.kind.clone(),
        range: Range {
            start_line: result.start_line,
            start_col: 0,
            end_line: result.end_line,
            end_col: 0,
        },
        signature: None,
        scope_chain: Vec::new(),
        exported: result.exported,
        parent: None,
    }
}

fn append_rank0_full_symbol_notice(content: String) -> String {
    if content.is_empty() {
        RANK0_FULL_SYMBOL_NOTICE.to_string()
    } else {
        format!("{content}\n{RANK0_FULL_SYMBOL_NOTICE}")
    }
}

fn should_expand_rank0_snippet(rank: usize, result: &HybridResult, project_root: &Path) -> bool {
    rank == 0
        && result
            .semantic_score
            .is_some_and(|cosine| cosine >= HIGH_CONFIDENCE_COSINE_FLOOR)
        && !path_is_test_support_file(&result.file, project_root)
}

fn format_result_sections(results: &[HybridResult], project_root: &Path) -> String {
    format_result_sections_with_context(results, project_root, None)
}

fn format_result_sections_with_context(
    results: &[HybridResult],
    project_root: &Path,
    ctx: Option<&AppContext>,
) -> String {
    // Results arrive sorted by fused score desc. Group by file preserving
    // first-appearance order so the most relevant file's group renders first.
    // A BTreeMap would re-sort groups alphabetically by path and scramble the
    // ranking the agent relies on to read most-relevant-first. Snippets are
    // already budgeted by enrich_snippets_from_source; render them verbatim.
    let annotations = ctx
        .map(|ctx| blast_radius_annotations(ctx, results))
        .unwrap_or_else(|| vec![None; results.len()]);
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<(usize, &HybridResult)>> = HashMap::new();

    for (index, result) in results.iter().enumerate() {
        let display_path = result
            .file
            .strip_prefix(project_root)
            .unwrap_or(&result.file)
            .display()
            .to_string();
        if !groups.contains_key(&display_path) {
            group_order.push(display_path.clone());
        }
        groups
            .entry(display_path)
            .or_default()
            .push((index, result));
    }

    group_order
        .iter()
        .map(|file| {
            let mut section = file.clone();
            if groups[file].iter().any(|(_, result)| result.exact) {
                section.push_str(" [exact]");
            }

            // Three distinct indent levels disambiguate the three roles for a
            // weak model at a glance: file path at col 0 (with its `/` and
            // extension), symbol header at 2 spaces, snippet body at 6. Without
            // this, file paths and symbol headers were both at col 0 and could
            // only be told apart by parsing the "[kind] lines X-Y" suffix.
            for (index, result) in &groups[file] {
                if result.source == "lexical" {
                    // Whole-file lexical match (no specific symbol).
                    section.push_str(" [lexical match]");
                    continue;
                }
                if matches!(result.kind, SymbolKind::FileSummary) {
                    section.push_str(&format!("\n  {} [file summary]", result.name));
                } else {
                    section.push_str(&format!(
                        "\n  {} [{}] lines {}-{}{}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        display_line_number(result.start_line),
                        display_line_number(result.end_line),
                        annotations
                            .get(*index)
                            .and_then(|annotation| annotation.as_deref())
                            .unwrap_or("")
                    ));
                }
                if !result.snippet.trim().is_empty() {
                    for line in result.snippet.lines() {
                        section.push_str("\n      ");
                        section.push_str(line);
                    }
                }
            }

            section
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn blast_radius_annotations(ctx: &AppContext, results: &[HybridResult]) -> Vec<Option<String>> {
    let Some(store) = warm_callgraph_store(ctx) else {
        return vec![None; results.len()];
    };

    results
        .iter()
        .map(|result| blast_radius_annotation_for_result(&store, result))
        .collect()
}

fn warm_callgraph_store(
    ctx: &AppContext,
) -> Option<std::sync::Arc<crate::callgraph_store::ReadonlyCallGraphStore>> {
    let receiver = ctx.callgraph_store_rx().try_lock()?;
    if receiver.is_some() {
        return None;
    }
    drop(receiver);
    try_read_with_budget(ctx.callgraph_store(), INTERACTIVE_ARTIFACT_READ_BUDGET)
        .and_then(|store| store.as_ref().map(std::sync::Arc::clone))
}

fn blast_radius_annotation_for_result(
    store: &crate::callgraph_store::ReadonlyCallGraphStore,
    result: &HybridResult,
) -> Option<String> {
    if result.source == "lexical" || matches!(result.kind, SymbolKind::FileSummary) {
        return None;
    }
    if result.name.trim().is_empty() {
        return None;
    }

    let callers = callers_result(store, &result.file, &result.name, 1, true).ok()?;
    let mut caller_basenames = Vec::new();
    let mut seen_files = HashSet::new();
    for group in &callers.callers {
        if seen_files.insert(group.file.clone()) {
            caller_basenames.push(compact_caller_basename(&group.file));
        }
    }

    let mut suffix = format!("  ↩{}", callers.total_callers);
    if !caller_basenames.is_empty() {
        let more = caller_basenames.len() > 2;
        let names = caller_basenames
            .iter()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        suffix.push(' ');
        suffix.push_str(&names);
        if more {
            suffix.push_str(",…");
        }
    }
    Some(suffix)
}

fn compact_caller_basename(file: &str) -> String {
    let basename = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(file);
    truncate_chars(basename, 18)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn result_to_json(result: &HybridResult) -> serde_json::Value {
    let is_file_level = matches!(result.kind, SymbolKind::FileSummary);
    let (start_line, end_line) = if is_file_level {
        (serde_json::Value::Null, serde_json::Value::Null)
    } else {
        (
            serde_json::json!(display_line_number(result.start_line)),
            serde_json::json!(display_line_number(result.end_line)),
        )
    };

    serde_json::json!({
        "file": result.file.display().to_string(),
        "name": result.name,
        "kind": result.kind,
        "start_line": start_line,
        "end_line": end_line,
        "location": if result.source == "lexical" { "[lexical match]" } else if is_file_level { "[file summary]" } else { "line range" },
        "score": result.score,
        "source": result.source,
        "semantic_score": result.semantic_score,
        "lexical_score": result.lexical_score,
        "hybrid_boosted": result.hybrid_boosted,
        "exact": result.exact,
        "snippet": result.snippet,
    })
}

fn grep_match_to_json(grep_match: &GrepMatch, source: &'static str) -> serde_json::Value {
    serde_json::json!({
        "kind": "GrepLine",
        "source": source,
        "file": grep_match.file.display().to_string(),
        "line": grep_match.line,
        "column": grep_match.column,
        "line_text": grep_match.line_text,
        "match_text": grep_match.match_text,
    })
}

fn display_line_number(line: u32) -> u32 {
    line.saturating_add(1)
}

fn symbol_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Kernel => "kernel",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file-summary",
    }
}

fn semantic_status_label(status: &SemanticIndexStatus) -> &'static str {
    match status {
        SemanticIndexStatus::Ready { .. } => "ready",
        SemanticIndexStatus::Building { .. } => "building",
        SemanticIndexStatus::Disabled => "disabled",
        SemanticIndexStatus::Failed(_) => "unavailable",
    }
}

fn interpreted_as_label(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Regex => "regex",
        SearchMode::Literal => "literal",
        SearchMode::Semantic => "semantic",
        SearchMode::Hybrid => "hybrid",
    }
}

/// Honest `interpreted_as` for a response built on a semantic-unavailable
/// fallback path. The query may have been *routed* as semantic/hybrid, but if
/// semantic never executed, the field must report what actually produced the
/// results — otherwise an agent reads "hybrid" and trusts a semantic ranking
/// that never ran. `lexical_ran` is true when the lexical (trigram) lane
/// produced the returned results; otherwise we report the routed mode (the
/// attempt), with the `semantic_unavailable`/`status` fields conveying that it
/// could not run.
fn fallback_executed_label(mode: SearchMode, lexical_ran: bool) -> &'static str {
    if lexical_ran {
        "lexical"
    } else {
        interpreted_as_label(mode)
    }
}

fn query_kind_label(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Identifier => "Identifier",
        QueryKind::Mixed => "Mixed",
        QueryKind::ErrorCode => "ErrorCode",
        QueryKind::Path => "Path",
        QueryKind::Regex => "Regex",
        QueryKind::NaturalLanguage => "NaturalLanguage",
    }
}

/// Strip one matched surrounding delimiter from a literal query. Quotes and
/// backticks are recognized because all three can select the code-literal
/// route; mismatched or already stripped input is left unchanged.
fn strip_surrounding_quotes(query: String) -> String {
    let trimmed = query.trim();
    if trimmed.len() < 2 {
        return query;
    }
    let first = trimmed.chars().next().unwrap();
    let last = trimmed.chars().next_back().unwrap();
    if matches!(first, '"' | '\'' | '`') && first == last {
        let mut chars = trimmed.chars();
        chars.next();
        chars.next_back();
        return chars.as_str().to_string();
    }
    query
}

fn extracted_tokens_all_short(query: &str, shape: &QueryShape) -> bool {
    let tokens = query_shape::extract_tokens(query, shape);
    !tokens.is_empty() && tokens.iter().all(|token| token.len() < 3)
}

pub fn humanize_degraded_reasons(reasons: &[String]) -> Vec<String> {
    reasons.iter().map(|code| humanize_one(code)).collect()
}

fn humanize_one(code: &str) -> String {
    if code == "home_root" {
        return "Project root is set to your home directory; large file-system indexes are disabled to avoid scanning the whole home tree.".into();
    }
    if code == "watcher_unavailable" {
        return "file watcher unavailable; continuing without live external-change invalidation"
            .to_string();
    }
    format!("(Degraded: {})", code)
}

fn degraded_warning(ctx: &AppContext) -> String {
    let mut text = "Lexical search ran in degraded full-file-scan mode.".to_string();
    let reasons = ctx.degraded_reasons();
    if !reasons.is_empty() {
        text.push_str(" Reasons: ");
        text.push_str(&humanize_degraded_reasons(&reasons).join("; "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callgraph::walk_project_files;
    use crate::callgraph_store::CallGraphStore;
    use crate::config::{Config, SemanticBackend, SemanticBackendConfig};
    use crate::context::{
        callgraph_cold_build_spawn_count_for_test, reset_callgraph_cold_build_spawn_count_for_test,
        AppContext,
    };
    use crate::parser::TreeSitterProvider;
    use crate::semantic_index::SemanticIndex;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn semantic_request(query: &str, top_k: usize) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "semantic-search-test",
            "command": "semantic_search",
            "query": query,
            "top_k": top_k,
        }))
        .expect("build semantic search request")
    }

    fn semantic_request_with_hint(query: &str, top_k: usize, hint: &str) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "semantic-search-test",
            "command": "semantic_search",
            "query": query,
            "top_k": top_k,
            "hint": hint,
        }))
        .expect("build semantic search request")
    }

    fn semantic_page_request(query: &str, top_k: usize, offset: usize) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "semantic-search-page-test",
            "command": "semantic_search",
            "query": query,
            "top_k": top_k,
            "offset": offset,
            "hint": "literal",
        }))
        .expect("build paged semantic search request")
    }

    fn response_value(response: Response) -> serde_json::Value {
        serde_json::to_value(response).expect("serialize response")
    }

    fn test_context(project_root: &Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project_root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    fn install_warm_callgraph_store(ctx: &AppContext, project_root: &Path) {
        let root = std::fs::canonicalize(project_root).expect("canonical project root");
        let files = walk_project_files(&root).collect::<Vec<_>>();
        let store_dir = root.join(".callgraph-store-test");
        let store =
            CallGraphStore::open(store_dir.clone(), root.clone()).expect("open callgraph store");
        store.cold_build(&files).expect("build callgraph store");
        drop(store);
        let store = CallGraphStore::open_readonly(store_dir, root)
            .expect("open read-only callgraph store")
            .expect("ready callgraph store");
        *ctx.callgraph_store()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(store));
    }

    fn start_mock_embedding_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept embedding request");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            let mut content_length = 0usize;
            loop {
                let n = stream.read(&mut chunk).expect("read embedding request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                            if let Some(value) = line.strip_prefix("Content-Length:") {
                                content_length = value.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                }
                if let Some(end) = header_end {
                    if buf.len() >= end + content_length {
                        break;
                    }
                }
            }

            let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write embedding response");
        });

        (format!("http://{}", addr), handle)
    }

    #[test]
    fn embed_query_construction_error_leaves_slot_empty_for_retry() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                semantic: SemanticBackendConfig {
                    backend: SemanticBackend::OpenAiCompatible,
                    model: "test-embedding".to_string(),
                    base_url: None,
                    api_key_env: None,
                    timeout_ms: 5_000,
                    query_timeout_ms: 3_000,
                    max_batch_size: 64,
                    max_files: 20_000,
                    ..Default::default()
                },
                ..Config::default()
            },
        );

        let err = embed_query("anything", &ctx).expect_err("construction should fail");
        assert!(
            err.contains("base_url is required"),
            "expected missing base_url construction error, got: {err}"
        );
        assert!(
            ctx.semantic_embedding_model().lock().is_none(),
            "failed model construction must not poison the lazy slot"
        );

        let (base_url, handle) = start_mock_embedding_server();
        ctx.update_config(|config| {
            config.semantic.base_url = Some(base_url);
        });

        let vector = embed_query("anything", &ctx).expect("retry should construct and embed");
        assert_eq!(vector, vec![0.1, 0.2, 0.3]);
        assert!(
            ctx.semantic_embedding_model().lock().is_some(),
            "successful retry should install the constructed model"
        );
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn classify_embed_query_error_names_timeout_budget_and_knob() {
        // A query-embedding timeout carries the budget that fired. The
        // classified detail must name the mechanism, the budget value, and the
        // knob that raises it — and the footer reason must carry the budget so
        // the agent sees the cause in the status line, not just the body.
        let timeout_error = format!(
            "failed to embed query: {}openai compatible request failed: operation timed out",
            crate::semantic_index::query_embedding_timeout_marker(3_000)
        );
        let classified = classify_embed_query_error(&timeout_error);
        assert!(
            classified.detail.contains("timed out after 3000ms"),
            "timeout detail must name the budget: {}",
            classified.detail
        );
        assert!(
            classified.detail.contains("semantic.query_timeout_ms"),
            "timeout detail must name the knob: {}",
            classified.detail
        );
        assert!(
            classified.detail.contains("raise it for slow providers"),
            "timeout detail must name the remedy: {}",
            classified.detail
        );
        assert_eq!(classified.footer_reason, "query embed timeout (3000ms)");
    }

    #[test]
    fn classify_embed_query_error_non_timeout_keeps_current_shape() {
        // A non-timeout failure (HTTP 4xx, connection refused, dimension
        // mismatch) must NOT claim a timeout. It keeps the current message
        // shape and the plain "unavailable" footer reason.
        for non_timeout in [
            "failed to embed query: openai compatible request failed (HTTP 401): Unauthorized",
            "failed to embed query: openai compatible request failed: connection refused",
            "semantic embedding dimension mismatch: query backend returned 768, index expects 384",
        ] {
            let classified = classify_embed_query_error(non_timeout);
            assert!(
                !classified.detail.contains("timed out"),
                "non-timeout must not claim timeout: {}",
                classified.detail
            );
            assert!(
                !classified.detail.contains("query_timeout_ms"),
                "non-timeout must not name the knob: {}",
                classified.detail
            );
            assert_eq!(classified.footer_reason, "unavailable");
            assert!(
                classified
                    .detail
                    .starts_with("Semantic search unavailable: "),
                "non-timeout keeps current message shape: {}",
                classified.detail
            );
        }
    }

    #[test]
    fn external_readiness_reports_building_before_bounded_borrow() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());
        let source = ExternalReadinessSource::new(&ctx, project.path(), None);

        let observation = extensions::ReadinessSource::sample(&source);

        assert_eq!(observation.trigram.status, IndexStatus::Building);
        assert!(matches!(
            observation.semantic.status,
            SemanticIndexStatus::Building { ref stage, .. } if stage == "loading_artifacts"
        ));
    }

    #[test]
    fn short_nl_concept_routes_to_hybrid_when_lexical_ready() {
        // "parse imports" classifies as a two-word lowercase NL concept, but it
        // is a literal code phrase the trigram lane can hit. With lexical ready
        // it must route to Hybrid (run the lexical lane), not pure Semantic.
        let shape = query_shape::classify("parse imports");
        assert_eq!(shape.kind, QueryKind::NaturalLanguage);
        let mut warnings = Vec::new();
        let mode = choose_mode("parse imports", &shape, true, &mut warnings);
        assert_eq!(mode, SearchMode::Hybrid);
    }

    #[test]
    fn long_nl_phrase_runs_both_ready_lanes() {
        let q = "how does the bridge resolve the binary";
        let shape = query_shape::classify(q);
        assert_eq!(shape.kind, QueryKind::NaturalLanguage);
        let mut warnings = Vec::new();
        let mode = choose_mode(q, &shape, true, &mut warnings);
        assert_eq!(mode, SearchMode::Hybrid);
        assert!(warnings.is_empty());
    }

    #[test]
    fn long_nl_phrase_discloses_semantic_only_when_lexical_is_unavailable() {
        let q = "how does the bridge resolve the binary";
        let shape = query_shape::classify(q);
        let mut warnings = Vec::new();
        let mode = choose_mode(q, &shape, false, &mut warnings);
        assert_eq!(mode, SearchMode::Semantic);
        assert_eq!(
            warnings,
            ["Lexical trigram index is unavailable; using semantic search only."]
        );
    }

    #[test]
    fn short_nl_extracts_lexical_tokens() {
        // The short-NL Hybrid path needs tokens; extract_tokens returns none for
        // NL, so collect_lexical_files uses the short-NL extractor.
        let tokens = query_shape::extract_short_nl_lexical_tokens("parse imports");
        assert_eq!(tokens, vec!["parse".to_string(), "imports".to_string()]);
        // Sub-3-char words are dropped (trigram floor).
        let tokens2 = query_shape::extract_short_nl_lexical_tokens("go to");
        assert!(tokens2.is_empty());
    }

    #[test]
    fn long_nl_lexical_tokens_drop_stopwords_and_normalize_punctuation() {
        let shape = query_shape::classify("not wired into the built-in browser tool yet");
        assert_eq!(
            query_shape::extract_lexical_tokens(
                "not wired into the built-in browser tool yet",
                &shape,
            ),
            ["wired", "built", "browser", "tool", "yet"]
        );
    }

    #[test]
    fn exact_tiers_normalize_phrases_and_bound_token_windows() {
        let project = tempfile::tempdir().expect("create project dir");
        let phrase = project.path().join("phrase.rs");
        let window = project.path().join("window.rs");
        let scattered = project.path().join("scattered.rs");
        fs::write(&phrase, "// ALPHA   beta gamma\n").expect("write phrase fixture");
        fs::write(&window, "// gamma\n// alpha\n// beta\n").expect("write window fixture");
        fs::write(
            &scattered,
            "// gamma\n// filler\n// filler\n// alpha\n// beta\n",
        )
        .expect("write scattered fixture");
        let tokens = query_shape::extract_content_tokens("alpha beta gamma");

        assert_eq!(
            lexical_candidate_exactness(&phrase, "alpha beta gamma", &tokens),
            (true, 1, Some(1))
        );
        assert_eq!(
            lexical_candidate_exactness(&window, "alpha beta gamma", &tokens),
            (true, 0, Some(3))
        );
        assert_eq!(
            lexical_candidate_exactness(&scattered, "alpha beta gamma", &tokens),
            (false, 0, None)
        );
    }

    #[test]
    fn building_status_returns_index_backed_fallback_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn needle_symbol() -> bool { true }\n";
        std::fs::write(&source_file, source).expect("write source file");

        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&source_file, source.as_bytes());
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "embedding".to_string(),
            files: Some(1),
            entries_done: Some(0),
            entries_total: Some(1),
        };

        let response = response_value(handle_semantic_search(
            &semantic_request("needle_symbol", 5),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["status"], "building");
        assert_eq!(response["semantic_status"], "building");
        // While semantic builds, only index-backed lanes produce results. The
        // existing "lexical" label means no embedding lane ran; exact evidence
        // may still refine those index-backed results.
        assert_eq!(response["interpreted_as"], "lexical");
        assert!(response["note"]
            .as_str()
            .expect("note")
            .contains("lexical-only fallback"));
        let text = response["text"].as_str().expect("text");
        assert!(text.contains("lexical fallback"));
        assert!(text.contains("Semantic index is rebuilding"));
        assert!(!text.contains(BORROWED_SEMANTIC_LOADING_WITH_LEXICAL_RESULTS));
        let results = response["results"].as_array().expect("results array");
        assert!(
            results.iter().any(|result| {
                matches!(result["source"].as_str(), Some("exact" | "lexical"))
                    && result["file"]
                        .as_str()
                        .expect("file")
                        .ends_with("src/lib.rs")
            }),
            "expected index-backed fallback result, got {results:?}"
        );
    }

    #[test]
    fn borrowed_loading_with_lexical_results_describes_shared_lane_truthfully() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn borrowed_loading_needle() {}\n";
        std::fs::write(&source_file, source).expect("write source file");

        let ctx = test_context(project.path());
        ctx.set_cache_role(true, None);
        let mut index = SearchIndex::new();
        index.index_file(&source_file, source.as_bytes());
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };

        let response = response_value(handle_semantic_search(
            &semantic_request("borrowed_loading_needle", 5),
            &ctx,
        ));
        let text = response["text"].as_str().expect("response text");
        assert!(text.contains(
            "Semantic lane is loading the shared index; lexical results below are complete for exact/identifier matches."
        ));
        assert!(!text.contains("still building"));
        assert!(!text.contains("rebuilding"));
        assert!(response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
    }

    #[test]
    fn empty_lexical_fallback_names_missing_semantic_coverage() {
        let text = format_lexical_unavailable_text(
            "Semantic index is loading.",
            &[],
            Path::new("/fixture"),
            "loading",
        );

        assert!(text.contains("0 lexical matches"));
        assert!(text.contains("semantic lane is unavailable"));
        assert!(text.contains("prose-style queries may match only via semantic"));
        assert!(!text.contains("lexical-only fallback returned 0"));
    }

    #[test]
    fn empty_degraded_grep_fallback_names_missing_semantic_coverage() {
        let result = GrepResult {
            matches: Vec::new(),
            total_matches: 0,
            files_searched: 0,
            files_with_matches: 0,
            index_status: IndexStatus::Fallback,
            truncated: false,
            fully_degraded: true,
            engine_capped: false,
            walk_truncated: false,
            skipped_foreign_mounts: 0,
        };
        let text = format_grep_lexical_unavailable_text(
            "Semantic index is loading.",
            &result,
            Path::new("/fixture"),
            "loading",
        );

        assert!(text.contains("0 lexical matches"));
        assert!(text.contains("semantic lane is unavailable"));
        assert!(!text.contains("lexical-only fallback returned 0"));
    }

    #[test]
    fn first_search_waits_for_slow_borrowed_base_and_returns_results() {
        assert_eq!(
            FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET,
            Duration::from_millis(2_500)
        );
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn waited_for_borrowed_artifact() {}\n";
        std::fs::write(&source_file, source).expect("write source file");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };

        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.install_search_index_rx(rx, ctx.configure_generation());
        let publish_file = source_file.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let mut index = SearchIndex::new();
            index.index_file(&publish_file, source.as_bytes());
            index.ready = true;
            tx.send(index).expect("publish search index");
        });

        // Decision-evidence form: non-empty index-backed results prove the query
        // waited for publication rather than taking the partial bounded walk.
        // Elapsed-time bounds were removed because a loaded runner can deschedule
        // either the publisher thread or this thread past any tight wall-clock budget;
        // the generous budget below is a hang catch, not a timing assertion.
        let response =
            with_first_search_index_load_wait_budget_for_test(Duration::from_secs(30), || {
                response_value(handle_semantic_search(
                    &semantic_request("waited_for_borrowed_artifact", 5),
                    &ctx,
                ))
            });

        assert_eq!(response["interpreted_as"], "lexical");
        assert!(response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
        assert!(!response["text"]
            .as_str()
            .expect("response text")
            .contains("nothing was searched yet"));
    }

    #[test]
    fn building_trigram_index_identifier_uses_bounded_walk() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("needle.ts");
        std::fs::write(
            &source_file,
            "export const fresh_root_needle = 'fresh_root_needle';\n",
        )
        .expect("write source file");

        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (_tx, rx) = crossbeam_channel::unbounded::<SearchIndex>();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        let raw_response =
            with_first_search_index_load_wait_budget_for_test(Duration::from_millis(40), || {
                handle_semantic_search(&semantic_request("fresh_root_needle", 5), &ctx)
            });
        let rendered = crate::subc_format::format_response("search", &raw_response, false);
        let response = response_value(raw_response);

        assert_eq!(response["success"], true);
        assert_eq!(response["status"], "partial");
        assert_eq!(response["complete"], false);
        assert_eq!(response["semantic_status"], "building");
        assert_eq!(response["interpreted_as"], "literal");
        let results = response["results"].as_array().expect("results array");
        assert!(results.iter().any(|result| {
            result["file"]
                .as_str()
                .is_some_and(|file| file.ends_with("needle.ts"))
        }));
        let text = response["text"].as_str().expect("response text");
        assert!(text.contains(TRIGRAM_BUILDING_BOUNDED_WALK_DISCLOSURE));
        // The handler never renders the trailer itself; the shared formatter
        // appends it from the wire envelope exactly once.
        assert!(!text.contains("(walk)"));
        assert!(!text.contains("(exhausted)"));
        assert_eq!(response["results_list_envelope"]["reason"], "walk");
        assert_eq!(rendered.matches("(walk)").count(), 1, "{rendered}");
        assert!(!rendered.contains("(exhausted)"));
        assert_eq!(
            response["results_list_envelope"]["total"]["kind"],
            "at_least"
        );
    }

    #[test]
    fn first_search_wait_budget_expires_with_honest_loading_reply() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (_tx, rx) = crossbeam_channel::unbounded::<SearchIndex>();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        let wait_budget = Duration::from_millis(40);
        let started = Instant::now();
        let raw_response = with_first_search_index_load_wait_budget_for_test(wait_budget, || {
            handle_semantic_search(&semantic_request("still_loading", 5), &ctx)
        });
        let rendered = crate::subc_format::format_response("search", &raw_response, false);
        let response = response_value(raw_response);

        assert!(started.elapsed() >= wait_budget);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(response["status"], "partial");
        let text = response["text"].as_str().expect("response text");
        assert!(text.contains(TRIGRAM_BUILDING_BOUNDED_WALK_DISCLOSURE));
        assert!(text.contains("Found 0 match"));
        // The trailer is the shared formatter's, appended from the wire envelope.
        assert_eq!(rendered.matches("(walk)").count(), 1, "{rendered}");
        assert!(!rendered.contains("(exhausted)"));
    }

    #[test]
    fn first_search_and_in_progress_load_complete_without_deadlock() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn no_deadlock_borrowed_artifact() {}\n";
        std::fs::write(&source_file, source).expect("write source file");
        let ctx = Arc::new(test_context(project.path()));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        ctx.install_search_index_rx(rx, ctx.configure_generation());

        let publish_file = source_file.clone();
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            let mut index = SearchIndex::new();
            index.index_file(&publish_file, source.as_bytes());
            index.ready = true;
            tx.send(index).expect("publish search index");
        });
        let search_ctx = Arc::clone(&ctx);
        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        let search = std::thread::spawn(move || {
            let response = with_first_search_index_load_wait_budget_for_test(
                Duration::from_millis(200),
                || {
                    response_value(handle_semantic_search(
                        &semantic_request("no_deadlock_borrowed_artifact", 5),
                        &search_ctx,
                    ))
                },
            );
            completed_tx
                .send(response)
                .expect("publish search response");
        });

        let response = completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("search and artifact publication must not deadlock");
        assert!(response["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
        publisher.join().expect("publisher joins");
        search.join().expect("search joins");
    }

    #[test]
    fn first_search_wait_observes_executor_cancellation() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = Arc::new(test_context(project.path()));
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        };
        let (_tx, rx) = crossbeam_channel::unbounded::<SearchIndex>();
        ctx.install_search_index_rx(rx, ctx.configure_generation());
        let cancellation = crate::executor::JobCancellation::new();
        let worker_cancellation = cancellation.clone();
        let worker_ctx = Arc::clone(&ctx);
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (completed_tx, completed_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            let _installed = crate::executor::install_job_cancellation(worker_cancellation);
            started_tx.send(()).expect("signal wait start");
            let response =
                with_first_search_index_load_wait_budget_for_test(Duration::from_secs(2), || {
                    response_value(handle_semantic_search(
                        &semantic_request("cancelled_wait", 5),
                        &worker_ctx,
                    ))
                });
            completed_tx
                .send(response)
                .expect("publish cancelled response");
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("search wait starts");
        std::thread::sleep(Duration::from_millis(20));
        cancellation.request_cancel();
        let response = completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled wait completes promptly");
        assert_eq!(response["code"], "request_cancelled");
        worker.join().expect("cancelled search joins");
    }

    #[test]
    fn read_only_failed_snapshot_retries_on_semantic_query() {
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let ctx = test_context(project.path());
        ctx.update_config(|config| {
            config.semantic_search = true;
            config.storage_dir = Some(storage.path().to_path_buf());
        });
        ctx.set_canonical_cache_root(project.path().to_path_buf());
        ctx.set_cache_writer_capabilities(false, true);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            SemanticIndexStatus::Failed("shared snapshot absent".to_string());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("retry snapshot", 5, "semantic"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["status"], "ready");
        assert_eq!(response["semantic_status"], "building");
        assert!(response["text"]
            .as_str()
            .expect("semantic fallback text")
            .contains("semantic lane is unavailable"));
        assert!(ctx.semantic_index_rx().lock().is_some());
        ctx.mark_subc_unbound();
        ctx.cancel_unbound_artifact_work();
    }

    #[test]
    fn regex_query_runs_without_semantic_index() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn exported() {}\n").expect("write source file");
        let ctx = test_context(project.path());
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint(".*exported", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "regex");
        assert_eq!(response["query_kind"], "Regex");
        assert_eq!(response["semantic_status"], "disabled");
        assert_eq!(response["results"][0]["kind"], "GrepLine");
    }

    #[test]
    fn auto_regexlike_uncompilable_query_falls_back_to_literal() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        // The fallback recompiles as an escaped literal, so the exact needle
        // must be present for this test to produce a match.
        std::fs::write(
            &source_file,
            "// assert_ne!(.*route_channel\nassert_ne!(route_channel, 0);\n",
        )
        .expect("write source file");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("assert_ne!(.*route_channel", 5, "auto"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        let results = response["results"].as_array().expect("results array");
        assert!(
            !results.is_empty(),
            "expected literal fallback result, got {results:?}"
        );
        assert_eq!(response["results"][0]["source"], "literal");
        assert_eq!(
            response["results"][0]["match_text"],
            "assert_ne!(.*route_channel"
        );
        let warnings = response["warnings"].as_array().expect("warnings array");
        let fallback_warning = warnings
            .iter()
            .filter_map(|warning| warning.as_str())
            .find(|warning| warning.contains("searched literally instead"))
            .expect("fallback warning");
        assert!(fallback_warning.contains("unclosed group"));
        assert!(fallback_warning.contains("Use grep when explicit regex lane control is required."));
    }

    #[test]
    fn legacy_regex_hint_is_ignored_for_uncompilable_query() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("assert_ne!(.*route_channel", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        assert!(response["text"]
            .as_str()
            .expect("literal fallback text")
            .contains("Found 0"));
    }

    #[test]
    fn valid_auto_regex_query_stays_regex_without_fallback_warning() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "let route_alpha_channel = 1;\n").expect("write source file");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("route_.*channel", 5, "auto"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "regex");
        assert_eq!(response["results"][0]["source"], "regex");
        let warnings = response["warnings"].as_array().expect("warnings array");
        assert!(!warnings.iter().any(|warning| {
            warning
                .as_str()
                .expect("warning")
                .contains("searched literally instead")
        }));
    }

    #[test]
    fn auto_short_token_warns_and_runs_grep_line_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "id = 1\n").expect("write source file");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("id", 5, "literal"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        assert!(response["warnings"][0]
            .as_str()
            .expect("warning")
            .contains("shorter than 3"));
    }

    #[test]
    fn unsupported_regex_auto_falls_back_to_literal() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("(?=foo)", 5, "regex"),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "literal");
        assert!(response["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("searched literally instead"))));
    }

    #[test]
    fn regex_zero_results_escalate_once_to_hybrid_terms() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/reminder.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(
            &source_file,
            "const missing_route = true;\npub fn exported() {}\n",
        )
        .expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request("^missing_route$", 5),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["result_count"], 1);
        assert_eq!(response["zero_result_escalation"], true);
        assert!(response["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|result| result["source"] == "lexical"));
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("[interpreted_as: regex; no exact match — ranked by terms instead]"));

        let first_project = tempfile::tempdir().expect("create first-lane project");
        let first_source = first_project.path().join("src/lib.rs");
        std::fs::create_dir_all(first_source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&first_source, "pub fn exported() {}\n").expect("write source file");
        let first_ctx = test_context(first_project.path());
        *first_ctx
            .semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
        let first_lane_response = response_value(handle_semantic_search(
            &semantic_request_with_hint(".*exported", 5, "regex"),
            &first_ctx,
        ));
        assert!(
            first_lane_response["result_count"]
                .as_u64()
                .is_some_and(|count| count > 0),
            "response: {first_lane_response:?}"
        );
        assert_eq!(
            first_lane_response
                .get("zero_result_escalation")
                .and_then(Value::as_bool),
            None,
            "response: {first_lane_response:?}"
        );
        assert!(!first_lane_response["text"]
            .as_str()
            .expect("text")
            .contains("no exact match"));
    }

    #[test]
    fn three_token_quoted_span_routes_as_code_literal_without_embedding() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/reminder.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "const template = \"outside <touser>\";\n")
            .expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(project.path().to_path_buf(), 3));

        let mut request = semantic_request("\"outside <touser>\" reminder text", 5);
        request.id = "b2-three-token-quoted-span".to_string();
        let response = response_value(handle_semantic_search(&request, &ctx));
        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "engine");
        assert_eq!(
            response["structuredContent"]["plan"]["shape"],
            "code_literal"
        );
        assert_eq!(
            response["structuredContent"]["plan"]["lanes_run"],
            serde_json::json!(["exact", "lexical"])
        );
        assert_eq!(
            response["structuredContent"]["search"]["embedding_calls"],
            0
        );
        assert_eq!(
            response["structuredContent"]["search"]["embedding_cache_hits"],
            0
        );
        assert_eq!(
            response["structuredContent"]["search"]["live_embed_calls"],
            0
        );
        assert!(response.get("zero_result_escalation").is_none());
    }

    #[test]
    fn four_token_quoted_span_remains_natural_language_and_runs_hybrid() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/reminder.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "const template = \"outside <touser>\";\n")
            .expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(project.path().to_path_buf(), 3));
        let (base_url, handle) = start_mock_embedding_server();
        ctx.update_config(|config| {
            config.semantic.backend = SemanticBackend::OpenAiCompatible;
            config.semantic.base_url = Some(base_url);
            config.semantic.model = "test-embedding".to_string();
        });

        let mut request = semantic_request("\"outside <touser>\" reminder text here", 5);
        request.id = "b2-four-token-quoted-span".to_string();
        let response = response_value(handle_semantic_search(&request, &ctx));
        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "hybrid");
        assert_eq!(
            response["structuredContent"]["plan"]["shape"],
            "natural_language"
        );
        assert!(response.get("zero_result_escalation").is_none());
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn garbage_regex_zero_after_escalation_is_honest_zero() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "const present_route = true;\n").expect("write source file");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(
            &source_file,
            std::fs::read(&source_file).expect("read source").as_slice(),
        );
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request("^qzxjvpl_888$", 5),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["result_count"], 0);
        assert_eq!(response["zero_result_escalation"], true);
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("[interpreted_as: regex; no exact match — ranked by terms instead]"));
    }

    #[test]
    fn humanize_degraded_reason_messages() {
        let reasons = vec![
            "home_root".to_string(),
            "watcher_unavailable".to_string(),
            "custom".to_string(),
        ];
        let human = humanize_degraded_reasons(&reasons);
        assert!(human[0].contains("home directory"));
        assert_eq!(
            human[1],
            "file watcher unavailable; continuing without live external-change invalidation"
        );
        assert_eq!(human[2], "(Degraded: custom)");
        assert!(human.join("; ").contains("; "));
    }

    fn rerank_shape(kind: QueryKind) -> QueryShape {
        QueryShape {
            kind,
            weights: query_shape::ShapeWeights {
                semantic: 0.0,
                lexical: 0.0,
                should_use_lexical: false,
            },
        }
    }

    fn semantic_candidate(
        file: &str,
        name: &str,
        qualified_name: Option<&str>,
        kind: SymbolKind,
        score: f32,
    ) -> SemanticResult {
        SemanticResult {
            file: PathBuf::from(file),
            name: name.to_string(),
            qualified_name: qualified_name.map(str::to_string),
            kind,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score,
            rank_score: score,
            cap_protected: false,
            source: "semantic",
        }
    }

    fn candidate_rank<'a>(results: &'a [SemanticResult], name: &str) -> &'a SemanticResult {
        results
            .iter()
            .find(|result| result.name == name)
            .expect("candidate present")
    }

    #[test]
    fn type_concept_identifier_detector_fires_only_for_titlecase_concepts() {
        for query in [
            "Engine implementations",
            "Engine handlers",
            "Allocation strategies",
            "EngineFactory implementations",
        ] {
            let shape = query_shape::classify(query);
            assert_eq!(shape.kind, QueryKind::Identifier, "{query}");
            assert!(
                query_shape::is_type_concept_identifier_query(query, &shape),
                "{query} should get definition priors"
            );
        }

        for query in [
            "engineFactory",
            "useState hook",
            "parseConfig option",
            "Engine",
        ] {
            let shape = query_shape::classify(query);
            assert_eq!(shape.kind, QueryKind::Identifier, "{query}");
            assert!(
                !query_shape::is_type_concept_identifier_query(query, &shape),
                "{query} should keep Identifier priors inert"
            );
        }

        for query in ["get user", "parse config"] {
            let shape = query_shape::classify(query);
            assert_eq!(shape.kind, QueryKind::NaturalLanguage, "{query}");
            assert!(!query_shape::is_type_concept_identifier_query(
                query, &shape
            ));
        }
    }

    #[test]
    fn type_concept_identifier_diversity_cap_limits_repeated_clusters_only() {
        let shape = rerank_shape(QueryKind::Identifier);
        let mut repeated_candidates = vec![
            semantic_candidate(
                "/project/src/a.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.90,
            ),
            semantic_candidate(
                "/project/src/b.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.89,
            ),
            semantic_candidate(
                "/project/src/c.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.88,
            ),
        ];
        rerank_semantic_candidates(&mut repeated_candidates, &shape, "Engine implementations");
        assert_eq!(repeated_candidates.len(), 2);
        assert!(repeated_candidates
            .iter()
            .all(|result| result.name == "Engine"));

        let mut distinct_candidates = vec![
            semantic_candidate(
                "/project/src/renderer.ts",
                "Renderer",
                Some("Renderer"),
                SymbolKind::Class,
                0.80,
            ),
            semantic_candidate(
                "/project/src/parser.ts",
                "Parser",
                Some("Parser"),
                SymbolKind::Class,
                0.79,
            ),
            semantic_candidate(
                "/project/src/planner.ts",
                "Planner",
                Some("Planner"),
                SymbolKind::Class,
                0.78,
            ),
        ];
        rerank_semantic_candidates(&mut distinct_candidates, &shape, "Engine implementations");
        assert_eq!(distinct_candidates.len(), 3);
        assert!(distinct_candidates
            .iter()
            .all(|result| result.rank_score > result.score));
    }

    #[test]
    fn type_concept_identifier_exact_name_boost_composes_with_kind_prior() {
        let shape = rerank_shape(QueryKind::Identifier);
        let mut candidates = vec![
            semantic_candidate(
                "/project/src/engine.ts",
                "Engine",
                Some("Engine"),
                SymbolKind::Class,
                0.70,
            ),
            semantic_candidate(
                "/project/src/renderer.ts",
                "Renderer",
                Some("Renderer"),
                SymbolKind::Class,
                0.75,
            ),
        ];

        rerank_semantic_candidates(&mut candidates, &shape, "Engine implementations");
        let named = candidate_rank(&candidates, "Engine");
        let sibling = candidate_rank(&candidates, "Renderer");

        assert!(named.rank_score > sibling.rank_score);
        assert!((named.rank_score - (0.70 * 1.08 * 1.20)).abs() < 0.0001);
        assert!((sibling.rank_score - (0.75 * 1.08)).abs() < 0.0001);
    }

    #[test]
    fn natural_language_diversity_cap_limits_repeated_name_kind_clusters() {
        let nl_shape = rerank_shape(QueryKind::NaturalLanguage);
        let mixed_shape = rerank_shape(QueryKind::Mixed);
        let candidates = vec![
            semantic_candidate(
                "/project/src/a.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.90,
            ),
            semantic_candidate(
                "/project/src/b.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.89,
            ),
            semantic_candidate(
                "/project/src/c.ts",
                "engineFactory",
                None,
                SymbolKind::Variable,
                0.88,
            ),
        ];

        let mut nl_candidates = candidates.clone();
        rerank_semantic_candidates(
            &mut nl_candidates,
            &nl_shape,
            "engine factory implementations",
        );
        assert_eq!(nl_candidates.len(), 2);

        let mut mixed_candidates = candidates;
        rerank_semantic_candidates(
            &mut mixed_candidates,
            &mixed_shape,
            "engineFactory implementations",
        );
        assert_eq!(mixed_candidates.len(), 3);
        assert!(mixed_candidates
            .iter()
            .all(|result| (result.rank_score - result.score).abs() < f32::EPSILON));
    }

    #[test]
    fn exact_name_boost_does_not_cap_protect_near_zero_common_names() {
        let shape = rerank_shape(QueryKind::Identifier);
        let mut candidates = vec![semantic_candidate(
            "/project/src/list.ts",
            "List",
            Some("List"),
            SymbolKind::Class,
            0.01,
        )];

        rerank_semantic_candidates(&mut candidates, &shape, "List");

        assert!((candidates[0].rank_score - 0.012).abs() < 0.0001);
        assert!(!candidates[0].cap_protected);
    }

    #[test]
    fn churned_borrowed_tree_query_completes_with_budget_degradation() {
        let session = tempfile::tempdir().expect("session root");
        let external = tempfile::tempdir().expect("external root");
        let external_root =
            std::fs::canonicalize(external.path()).expect("canonical external root");
        let git_status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&external_root)
            .status()
            .expect("initialize external git fixture");
        assert!(git_status.success());
        let storage = tempfile::tempdir().expect("storage");
        for file_index in 0..64 {
            let file = external_root.join(format!(
                "packages/pkg_{}/src/module_{file_index}.rs",
                file_index % 8
            ));
            std::fs::create_dir_all(file.parent().expect("fixture parent"))
                .expect("create nested fixture directory");
            std::fs::write(
                file,
                format!("pub fn borrowed_tree_needle_{file_index}() {{}}\n"),
            )
            .expect("write borrowed fixture");
        }
        let cache_dir =
            crate::search_index::resolve_cache_dir(&external_root, Some(storage.path()));
        let mut index = SearchIndex::build(&external_root);
        index.write_to_disk(&cache_dir, None);

        let ctx = AppContext::new(
            crate::context::default_language_provider_factory(),
            crate::config::Config {
                project_root: Some(session.path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        let mut req = semantic_request("borrowed_tree_needle", 10);
        req.params["path"] = serde_json::json!(external_root);
        let first = crate::readonly_artifacts::with_borrowed_search_load_limits_for_test(
            10_000,
            Duration::from_secs(5),
            || response_value(handle_semantic_search(&req, &ctx)),
        );
        assert_eq!(first["success"], true, "initial borrow failed: {first:?}");

        for file_index in 0..40 {
            let file = external_root.join(format!(
                "packages/pkg_{}/src/module_{file_index}.rs",
                file_index % 8
            ));
            std::fs::write(
                file,
                format!("pub fn churned_borrowed_tree_{file_index}() {{}}\n"),
            )
            .expect("churn borrowed fixture");
        }
        let mut rebuilt = SearchIndex::build(&external_root);
        rebuilt.write_to_disk(&cache_dir, None);

        let started = Instant::now();
        let degraded = crate::readonly_artifacts::with_borrowed_search_load_limits_for_test(
            10,
            Duration::from_secs(5),
            || response_value(handle_semantic_search(&req, &ctx)),
        );

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "churned generation must stop at the borrowed-load budget"
        );
        assert_eq!(degraded["success"], true);
        assert_eq!(degraded["complete"], false);
        assert_eq!(degraded["fully_degraded"], true);
        assert_eq!(
            degraded["borrowed_index_degraded_reason"],
            "borrowed_search_index_load_budget"
        );
        assert!(degraded["text"]
            .as_str()
            .expect("degraded response text")
            .ends_with(BORROWED_SEARCH_LOAD_FOOTER));
    }

    #[test]
    fn borrowed_load_budget_degradation_has_locked_disclosure() {
        let session = tempfile::tempdir().expect("session root");
        let external = tempfile::tempdir().expect("external root");
        std::fs::write(
            external.path().join("fixture.rs"),
            "pub fn budget_disclosure_needle() {}\n",
        )
        .expect("write external fixture");
        let ctx = test_context(session.path());
        let req = semantic_request("budget_disclosure_needle", 10);
        let params = SemanticSearchParams {
            query: "budget_disclosure_needle".to_string(),
            top_k: 10,
            offset: 0,
            include_tests: false,
        };
        let shape = query_shape::classify(&params.query);

        let response = response_value(handle_external_borrowed_degraded_fallback(
            &req,
            &ctx,
            &params,
            10,
            &shape,
            external.path(),
            crate::readonly_artifacts::BORROWED_SEARCH_LOAD_DEGRADATION,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["complete"], false);
        assert_eq!(response["fully_degraded"], true);
        assert_eq!(
            response["borrowed_index_degraded_reason"],
            "borrowed_search_index_load_budget"
        );
        assert!(response["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning.as_str() == Some(BORROWED_SEARCH_LOAD_WARNING)));
        assert!(response["text"]
            .as_str()
            .expect("text")
            .ends_with(BORROWED_SEARCH_LOAD_FOOTER));
    }

    #[test]
    fn borrowed_drift_remains_available_in_logs() {
        let message = borrowed_drift_log_message("semantic", Path::new("/borrowed"), 3);
        assert!(message.contains("borrowed semantic index"));
        assert!(message.contains("3 drifted file(s)"));
    }

    #[test]
    fn empty_semantic_index_skips_query_dimension_check() {
        let project = tempfile::tempdir().expect("create project dir");
        let (base_url, handle) = start_mock_embedding_server();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                semantic: SemanticBackendConfig {
                    backend: SemanticBackend::OpenAiCompatible,
                    model: "test-embedding".to_string(),
                    base_url: Some(base_url),
                    api_key_env: None,
                    timeout_ms: 5_000,
                    query_timeout_ms: 3_000,
                    max_batch_size: 64,
                    max_files: 20_000,
                    ..Default::default()
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::ready();
        *ctx.semantic_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(SemanticIndex::new(project.path().to_path_buf(), 384));

        let response = response_value(handle_semantic_search(
            &semantic_request("anything", 5),
            &ctx,
        ));

        assert_eq!(
            response["success"], true,
            "response should not fail: {response:?}"
        );
        assert_eq!(response["status"], "ready");
        assert_eq!(response["semantic_status"], "ready");
        assert!(response["results"].as_array().expect("results").is_empty());
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn file_summary_text_uses_summary_location_instead_of_line_range() {
        let project_root = Path::new("/project");
        let results = vec![HybridResult {
            file: PathBuf::from("/project/src/index.ts"),
            name: "index".to_string(),
            kind: SymbolKind::FileSummary,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score: 0.75,
            source: "semantic",
            semantic_score: Some(0.75),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        }];

        let text = format_semantic_text(&results, project_root, false, false, None);

        // File-summary rows show "[file summary]" with no line range, and no
        // longer leak the internal score/source.
        assert!(text.contains("index [file summary]"));
        assert!(!text.contains("lines 1-1"));
        assert!(!text.contains("score"));
        assert!(!text.contains("source semantic"));
    }

    /// A symbol hit whose `file` points at a real on-disk file with `body_lines`
    /// lines starting at line 0, so enrich_snippets_from_source can read it. The
    /// stored `snippet` is left empty on purpose — enrichment fills it from disk.
    fn write_symbol_hit(
        dir: &Path,
        file_name: &str,
        name: &str,
        body_lines: usize,
    ) -> HybridResult {
        let path = dir.join(file_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create symbol parent");
        }
        let body = (0..body_lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).expect("write symbol file");
        snippet_hit(
            path,
            name,
            SymbolKind::Function,
            0,
            body_lines.saturating_sub(1) as u32,
            0.5,
        )
    }

    fn snippet_hit(
        file: PathBuf,
        name: &str,
        kind: SymbolKind,
        start_line: u32,
        end_line: u32,
        score: f32,
    ) -> HybridResult {
        HybridResult {
            file,
            name: name.to_string(),
            kind,
            start_line,
            end_line,
            exported: false,
            snippet: String::new(),
            score,
            source: "semantic",
            semantic_score: Some(score),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        }
    }

    #[test]
    fn bounded_snippet_enrichment_matches_full_read_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mid.rs");
        std::fs::write(
            &path,
            "preamble one\npreamble two\n\n/// Explains target.\n#[inline]\nfn target() {\n    work();\n}\nmarker after required range\n",
        )
        .expect("write fixture");
        let hit = snippet_hit(
            path,
            "target",
            SymbolKind::Function,
            5,
            7,
            HIGH_CONFIDENCE_COSINE_FLOOR,
        );
        let mut bounded = vec![hit.clone()];
        let mut reference = vec![hit];

        let bounded_incomplete = enrich_snippets_from_source(&mut bounded, dir.path());
        let reference_incomplete =
            enrich_snippets_from_source_reference(&mut reference, dir.path(), None);

        assert_eq!(bounded[0].snippet, reference[0].snippet);
        assert_eq!(bounded_incomplete, reference_incomplete);
        assert!(bounded[0].snippet.contains("Explains target"));
        assert!(bounded[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE));
    }

    #[test]
    fn bounded_snippet_reader_stops_before_later_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bounded.rs");
        std::fs::write(&path, "line0\nline1\nline2\nmarker-must-not-be-read\n")
            .expect("write fixture");
        let plan = SnippetReadPlan {
            fixed_last_line: Some(2),
            summary_nonempty_lines: 0,
        };

        let lines = read_bounded_snippet_lines(&path, plan).expect("bounded read");

        assert_eq!(lines, vec!["line0", "line1", "line2"]);
    }

    #[test]
    fn bounded_snippet_reader_ignores_invalid_utf8_only_beyond_required_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("invalid-tail.rs");
        std::fs::write(&path, b"line0\nline1\n\xff\n").expect("write fixture");

        let mut valid_prefix = vec![snippet_hit(
            path.clone(),
            "prefix",
            SymbolKind::Function,
            0,
            1,
            0.5,
        )];
        let valid_incomplete = enrich_snippets_from_source(&mut valid_prefix, dir.path());
        assert_eq!(valid_prefix[0].snippet, "line0\nline1");
        assert!(!valid_incomplete);

        let mut invalid_range = vec![snippet_hit(
            path,
            "invalid",
            SymbolKind::Function,
            0,
            2,
            0.5,
        )];
        let invalid_incomplete = enrich_snippets_from_source(&mut invalid_range, dir.path());
        assert!(invalid_range[0].snippet.is_empty());
        assert!(!invalid_incomplete);
    }

    #[test]
    fn file_summary_and_symbol_share_one_bounded_file_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.rs");
        std::fs::write(
            &path,
            "\nsummary one\n\nfn target() {\n}\nsummary two\nsummary three\nmarker after requirements\n",
        )
        .expect("write fixture");
        let symbol = snippet_hit(path.clone(), "target", SymbolKind::Function, 3, 4, 0.5);
        let mut summary = snippet_hit(path, "shared.rs", SymbolKind::FileSummary, 0, 0, 0.5);
        summary.snippet = "persisted summary".to_string();
        let original = vec![symbol, summary];
        let (plans, _) = snippet_read_plans(&original, dir.path(), None);
        assert_eq!(plans.len(), 1, "same-file hits must share one read plan");

        let mut bounded = original.clone();
        let mut reference = original;
        let bounded_incomplete = enrich_snippets_from_source(&mut bounded, dir.path());
        let reference_incomplete =
            enrich_snippets_from_source_reference(&mut reference, dir.path(), None);

        assert_eq!(bounded[0].snippet, reference[0].snippet);
        assert_eq!(bounded[1].snippet, reference[1].snippet);
        assert_eq!(bounded_incomplete, reference_incomplete);
    }

    #[test]
    fn rows_omit_score_and_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "a.rs", "foo", 2)];
        let incomplete = enrich_snippets_from_source(&mut results, dir.path());
        let text = format_semantic_text(&results, dir.path(), false, incomplete, None);
        assert!(text.contains("foo [function] lines 1-2"));
        assert!(!text.contains("score"));
        assert!(!text.contains("source"));
    }

    #[test]
    fn snippets_are_rank_tiered_top_three_only_from_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Five hits, each a 30-line body, in distinct files so grouping does not
        // merge them. Rank order = vector order (already sorted). Budgets:
        // rank 0 = 20 lines (+10 more lines), ranks 1-2 = 5 lines (+25 more
        // lines), rank 3+ = header only.
        let mut results: Vec<HybridResult> = (0..5)
            .map(|i| write_symbol_hit(dir.path(), &format!("f{i}.rs"), &format!("fn{i}"), 30))
            .collect();
        let incomplete = enrich_snippets_from_source(&mut results, dir.path());
        assert!(incomplete);
        let text = format_semantic_text(&results, dir.path(), false, incomplete, None);

        assert!(text.contains("fn0 [function]"));
        // "lines" wording is load-bearing (vs "+N more" reading as results).
        assert!(text.contains("+10 more lines"));
        assert!(text.contains("+25 more lines"));
        // Rank 0 genuinely shows MORE than ranks 1-2 (gradient not inverted).
        let body_lines =
            |r: &HybridResult| r.snippet.lines().filter(|l| l.starts_with("line")).count();
        assert_eq!(body_lines(&results[0]), 20);
        assert_eq!(body_lines(&results[1]), 5);
        // Ranks 3,4 → header only, no body lines.
        assert!(
            results[3].snippet.is_empty(),
            "rank 4+ must have no snippet"
        );
        assert!(
            results[4].snippet.is_empty(),
            "rank 4+ must have no snippet"
        );
        // Zoom hint present because snippets were withheld.
        assert!(text.contains("aft_zoom <file> <symbol>"));
    }

    #[test]
    fn high_confidence_rank0_expands_full_symbol_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "full.rs", "full", 30)];
        results[0].semantic_score = Some(HIGH_CONFIDENCE_COSINE_FLOOR);
        results[0].score = HIGH_CONFIDENCE_COSINE_FLOOR;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(
            !incomplete,
            "full rank-0 symbol should not need a zoom hint"
        );
        assert!(results[0].snippet.contains("line29"));
        assert!(results[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE));
        assert!(!results[0].snippet.contains("+10 more lines"));
    }

    #[test]
    fn subfloor_rank0_keeps_preview_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "preview.rs", "preview", 30)];
        results[0].semantic_score = Some(HIGH_CONFIDENCE_COSINE_FLOOR - 0.01);
        results[0].score = HIGH_CONFIDENCE_COSINE_FLOOR - 0.01;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(incomplete);
        assert!(results[0].snippet.contains("line19"));
        assert!(!results[0].snippet.contains("line29"));
        assert!(results[0].snippet.contains("+10 more lines"));
    }

    #[test]
    fn test_support_rank0_never_full_expands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(
            dir.path(),
            "fixtures/full.rs",
            "fixture",
            30,
        )];
        results[0].semantic_score = Some(0.99);
        results[0].score = 0.99;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(incomplete);
        assert!(!results[0].snippet.contains("line29"));
        assert!(results[0].snippet.contains("+10 more lines"));
    }

    #[test]
    fn rank0_large_container_renders_member_menu_without_full_notice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("large.ts");
        let mut content = String::from(
            "class BigContainer {\n  methodOne(): number {\n    const visibleMethodBodyLine = 1;\n",
        );
        for i in 0..155 {
            content.push_str(&format!("    const filler{i} = {i};\n"));
        }
        content.push_str(
            "    return visibleMethodBodyLine;\n  }\n\n  methodTwo(): void {\n    console.log(\"second\");\n  }\n}\n",
        );
        std::fs::write(&path, content).expect("write large class");
        let ctx = test_context(dir.path());
        let symbols = ctx.provider().list_symbols(&path).expect("list symbols");
        let target = symbols
            .iter()
            .find(|symbol| symbol.name == "BigContainer")
            .expect("BigContainer symbol");
        let mut results = vec![HybridResult {
            file: path.clone(),
            name: "BigContainer".to_string(),
            kind: SymbolKind::Class,
            start_line: target.range.start_line,
            end_line: target.range.end_line,
            exported: false,
            snippet: String::new(),
            score: 0.99,
            source: "semantic",
            semantic_score: Some(0.99),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        }];

        let incomplete =
            enrich_snippets_from_source_with_context(&mut results, dir.path(), Some(&ctx));
        let snippet = &results[0].snippet;

        assert!(incomplete, "member menu is not a complete body");
        assert!(
            snippet.contains("member-signature menu; zoom a member for its body"),
            "large container should render a member menu: {snippet}"
        );
        assert!(
            snippet.contains("BigContainer.methodOne(): number"),
            "menu should include qualified method signatures: {snippet}"
        );
        assert!(
            !snippet.contains("visibleMethodBodyLine"),
            "menu must not include the class body: {snippet}"
        );
        assert!(
            !snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "member menu must not claim the full symbol was shown: {snippet}"
        );

        let disabled_ctx = test_context(dir.path());
        disabled_ctx.update_config(|config| {
            config.disabled_tools.push("aft_zoom".to_string());
        });
        let mut disabled_results = vec![results[0].clone()];
        enrich_snippets_from_source_with_context(
            &mut disabled_results,
            dir.path(),
            Some(&disabled_ctx),
        );
        assert!(disabled_results[0]
            .snippet
            .contains("member-signature menu; read a member for its body"));
        assert!(!disabled_results[0].snippet.contains("aft_zoom"));
    }

    #[test]
    fn oversized_rank0_full_expansion_renders_budgeted_head_slice() {
        // Use a 300-line symbol so the top result is truncated to a head slice,
        // setting incomplete=true instead of using the small default preview.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![write_symbol_hit(dir.path(), "huge.rs", "huge", 300)];
        results[0].semantic_score = Some(0.99);
        results[0].score = 0.99;

        let incomplete = enrich_snippets_from_source(&mut results, dir.path());

        assert!(incomplete);
        assert!(results[0].snippet.contains("line249"));
        assert!(!results[0].snippet.contains("line299"));
        assert!(results[0]
            .snippet
            .contains("… +50 more lines — zoom huge for the full body"));
        assert!(
            !results[0].snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "capped fallback is incomplete — must NOT claim no-re-read"
        );
    }

    #[test]
    fn rank0_expansion_includes_leading_doc_and_excludes_trailing_neighbor() {
        // A symbol preceded by a doc comment and a decorator, with a NEXT symbol
        // immediately after. Rank-0 expansion must show the doc + the symbol, and
        // must NOT bleed the following symbol in.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doc.ts");
        let content = "import x from 'y';\n\
                       \n\
                       /** Does the thing. */\n\
                       @decorator\n\
                       export function target() {\n\
                       \x20\x20return 1;\n\
                       }\n\
                       \n\
                       export function nextSymbol() {\n\
                       \x20\x20return 2;\n\
                       }\n";
        std::fs::write(&path, content).expect("write");
        // target() body spans the `export function target` line (index 4) through
        // its closing brace (index 6), 0-based inclusive.
        let mut results = vec![HybridResult {
            file: path,
            name: "target".to_string(),
            kind: SymbolKind::Function,
            start_line: 4,
            end_line: 6,
            exported: true,
            snippet: String::new(),
            score: 0.99,
            source: "semantic",
            semantic_score: Some(0.99),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        }];

        enrich_snippets_from_source(&mut results, dir.path());
        let snippet = &results[0].snippet;

        assert!(
            snippet.contains("Does the thing."),
            "leading doc comment must be included: {snippet}"
        );
        assert!(
            snippet.contains("@decorator"),
            "leading decorator must be included: {snippet}"
        );
        assert!(
            snippet.contains("export function target()"),
            "symbol signature must be present: {snippet}"
        );
        assert!(
            !snippet.contains("nextSymbol"),
            "trailing neighbor must NOT bleed in: {snippet}"
        );
        assert!(
            snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "full expansion must carry the no-re-read notice: {snippet}"
        );
    }

    #[test]
    fn rank0_expansion_does_not_include_c_preprocessor_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("target.c");
        let content = "#include <x.h>
                       int target(void) {
                         return 0;
                       }
";
        std::fs::write(&path, content).expect("write");
        let mut results = vec![HybridResult {
            file: path,
            name: "target".to_string(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 3,
            exported: false,
            snippet: String::new(),
            score: HIGH_CONFIDENCE_COSINE_FLOOR,
            source: "semantic",
            semantic_score: Some(HIGH_CONFIDENCE_COSINE_FLOOR),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        }];

        enrich_snippets_from_source(&mut results, dir.path());
        let snippet = &results[0].snippet;

        assert!(
            snippet.contains("int target(void)"),
            "symbol signature must be present: {snippet}"
        );
        assert!(
            !snippet.contains("#include <x.h>"),
            "C preprocessor directives must not be treated as symbol docs: {snippet}"
        );
        assert!(
            snippet.contains(RANK0_FULL_SYMBOL_NOTICE),
            "full expansion must carry the no-re-read notice: {snippet}"
        );
    }

    #[test]
    fn weak_top_match_emits_low_confidence_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hit = write_symbol_hit(dir.path(), "a.rs", "foo", 2);
        // Top semantic cosine below the weak floor.
        hit.semantic_score = Some(0.22);
        hit.score = 0.22;
        let results = vec![hit];
        let text = format_semantic_text(&results, dir.path(), false, false, None);
        assert!(
            text.contains("Top match is weak"),
            "expected weak-match note, got: {text}"
        );
    }

    #[test]
    fn strong_top_match_has_no_low_confidence_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut hit = write_symbol_hit(dir.path(), "a.rs", "foo", 2);
        hit.semantic_score = Some(0.72);
        hit.score = 0.72;
        let results = vec![hit];
        let text = format_semantic_text(&results, dir.path(), false, false, None);
        assert!(!text.contains("Top match is weak"), "got: {text}");
        // And no unconditional "[index: ready]" tax on the happy path.
        assert!(!text.contains("[index: ready]"), "got: {text}");
    }

    #[test]
    fn no_zoom_hint_when_all_snippets_fit() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two small symbols (3 lines each), both within their rank budget.
        let mut results = vec![
            write_symbol_hit(dir.path(), "a.rs", "foo", 3),
            write_symbol_hit(dir.path(), "b.rs", "bar", 3),
        ];
        let incomplete = enrich_snippets_from_source(&mut results, dir.path());
        assert!(!incomplete);
        let text = format_semantic_text(&results, dir.path(), false, incomplete, None);
        assert!(!text.contains("+"), "no truncation marker expected: {text}");
        assert!(!text.contains("aft_zoom"), "no zoom hint expected: {text}");
    }

    #[test]
    fn enrich_handles_missing_file_gracefully() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut results = vec![HybridResult {
            file: dir.path().join("does-not-exist.rs"),
            name: "ghost".to_string(),
            kind: SymbolKind::Function,
            start_line: 0,
            end_line: 9,
            exported: false,
            snippet: String::new(),
            score: 0.5,
            source: "semantic",
            semantic_score: Some(0.5),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        }];
        // Must not panic; header renders, no snippet body.
        let _ = enrich_snippets_from_source(&mut results, dir.path());
        assert!(results[0].snippet.is_empty());
        let text = format_result_sections(&results, dir.path());
        assert!(text.contains("ghost [function]"));
    }

    #[test]
    fn groups_render_in_rank_order_not_alphabetical() {
        let dir = tempfile::tempdir().expect("tempdir");
        // zzz.rs holds the top hit, aaa.rs the second. Alphabetical grouping
        // (the old BTreeMap bug) would put aaa.rs first; rank order keeps zzz.
        let results = vec![
            write_symbol_hit(dir.path(), "zzz.rs", "top", 1),
            write_symbol_hit(dir.path(), "aaa.rs", "second", 1),
        ];
        let text = format_result_sections(&results, dir.path());
        let zzz_at = text.find("zzz.rs").expect("zzz present");
        let aaa_at = text.find("aaa.rs").expect("aaa present");
        assert!(zzz_at < aaa_at, "top-ranked file must render first: {text}");
    }

    #[test]
    fn warm_callgraph_adds_compact_blast_radius_suffixes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src_dir = dir.path().join("src");
        let fixture_dir = dir.path().join("fixtures");
        std::fs::create_dir_all(&src_dir).expect("create src");
        std::fs::create_dir_all(&fixture_dir).expect("create fixtures");
        let target_file = src_dir.join("target.ts");
        std::fs::write(
            &target_file,
            "export function covered() {\n  return 1;\n}\nexport function untested() {\n  return 2;\n}\n",
        )
        .expect("write target");
        std::fs::write(
            src_dir.join("app.ts"),
            "import { covered, untested } from './target';\nexport function callerOne() {\n  return covered() + untested();\n}\nexport function callerTwo() {\n  return covered();\n}\n",
        )
        .expect("write app");
        std::fs::write(
            fixture_dir.join("covered_fixture.ts"),
            "import { covered } from '../src/target';\nexport function fixtureCaller() {\n  return covered();\n}\n",
        )
        .expect("write fixture");
        let ctx = test_context(dir.path());
        install_warm_callgraph_store(&ctx, dir.path());

        let results = vec![
            HybridResult {
                file: target_file.clone(),
                name: "covered".to_string(),
                kind: SymbolKind::Function,
                start_line: 0,
                end_line: 2,
                exported: true,
                snippet: String::new(),
                score: 0.8,
                source: "semantic",
                semantic_score: Some(0.8),
                lexical_score: None,
                hybrid_boosted: false,
                exact: false,
                exact_phrase_count: 0,
                exact_window_lines: None,
                fusion_score: 0.0,
            },
            HybridResult {
                file: target_file,
                name: "untested".to_string(),
                kind: SymbolKind::Function,
                start_line: 3,
                end_line: 5,
                exported: true,
                snippet: String::new(),
                score: 0.7,
                source: "semantic",
                semantic_score: Some(0.7),
                lexical_score: None,
                hybrid_boosted: false,
                exact: false,
                exact_phrase_count: 0,
                exact_window_lines: None,
                fusion_score: 0.0,
            },
        ];

        let text = format_semantic_text(&results, dir.path(), false, false, Some(&ctx));
        let covered_line = text
            .lines()
            .find(|line| line.contains("covered [function]"))
            .expect("covered row");
        assert!(
            covered_line.contains("↩"),
            "covered row should show callers: {text}"
        );
        assert!(
            covered_line.contains("app.ts") || covered_line.contains("covered_fixture.ts"),
            "covered row should include caller basenames: {covered_line}"
        );

        let untested_line = text
            .lines()
            .find(|line| line.contains("untested [function]"))
            .expect("untested row");
        assert!(
            untested_line.contains("↩"),
            "untested row should show callers: {text}"
        );
        assert!(
            untested_line.contains("app.ts"),
            "untested caller basename missing: {untested_line}"
        );

        // The `⚠untested` marker was removed: it provided no actionable signal in
        // a discovery tool and relied on is_test_support_file (fixtures/mocks
        // only), so it false-flagged genuinely tested code. Blast radius keeps
        // only the accurate `↩callers` + basenames.
        assert!(
            !text.contains("⚠untested"),
            "untested marker must no longer appear in search output: {text}"
        );
    }

    #[test]
    fn absent_warm_callgraph_emits_no_blast_radius_and_starts_no_build() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = test_context(dir.path());
        reset_callgraph_cold_build_spawn_count_for_test();
        let results = vec![write_symbol_hit(dir.path(), "target.rs", "target", 1)];

        let text = format_semantic_text(&results, dir.path(), false, false, Some(&ctx));

        assert!(
            !text.contains("↩"),
            "cold store should not annotate rows: {text}"
        );
        assert_eq!(callgraph_cold_build_spawn_count_for_test(), 0);
        assert!(ctx.callgraph_store_rx().lock().is_none());
    }

    #[test]
    fn more_available_appends_raise_topk_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let results = vec![write_symbol_hit(dir.path(), "a.rs", "foo", 1)];
        let text = format_semantic_text(&results, dir.path(), true, false, None);
        assert!(text.contains("More results available; raise topK to see more."));
    }

    #[test]
    fn file_summary_json_uses_summary_location_instead_of_line_numbers() {
        let result = HybridResult {
            file: PathBuf::from("/project/src/index.ts"),
            name: "index".to_string(),
            kind: SymbolKind::FileSummary,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score: 0.75,
            source: "semantic",
            semantic_score: Some(0.75),
            lexical_score: None,
            hybrid_boosted: false,
            exact: false,
            exact_phrase_count: 0,
            exact_window_lines: None,
            fusion_score: 0.0,
        };

        let json = result_to_json(&result);

        assert_eq!(json["kind"], "file_summary");
        assert_eq!(json["location"], "[file summary]");
        assert!(json["start_line"].is_null());
        assert!(json["end_line"].is_null());
        assert_eq!(json["source"], "semantic");
        assert_eq!(json["semantic_score"], 0.75);
        assert!(json["lexical_score"].is_null());
    }

    #[test]
    fn ready_uncontended_search_keeps_indexed_literal_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn parity_marker() {}\n").expect("write source");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&source_file, b"pub fn parity_marker() {}\n");
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let response = response_value(handle_semantic_search(
            &semantic_request_with_hint("parity_marker", 5, "literal"),
            &ctx,
        ));
        assert_eq!(response["success"], true);
        assert_eq!(response["interpreted_as"], "lexical");
        assert_eq!(
            response["results"][0]["source"], "exact",
            "the live exact lane now owns verbatim identifier matches"
        );
        assert!(response["results"][0]["file"]
            .as_str()
            .expect("result file")
            .ends_with("src/lib.rs"));
    }

    #[test]
    fn repeated_paged_search_reads_exact_candidates_once_per_query_generation() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn repeated_page_marker() {}\n").expect("write source");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&source_file, b"pub fn repeated_page_marker() {}\n");
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let pages = [(100, 0), (100, 100), (100, 200), (100, 300)]
            .into_iter()
            .chain((0..10).map(|page| (10, page * 10)))
            .chain((0..4).map(|page| (25, page * 25)))
            .chain([(100, 0)]);
        for (top_k, offset) in pages {
            let response = response_value(handle_semantic_search(
                &semantic_page_request("repeated_page_marker", top_k, offset),
                &ctx,
            ));
            assert_eq!(response["success"], true);
        }

        assert_eq!(ctx.search_exact_memo().verifier_call_count(), 1);
    }

    #[test]
    fn exact_page_memo_keys_include_corpus_generation() {
        let project = tempfile::tempdir().expect("create project dir");
        let first = project.path().join("src/a.rs");
        let second = project.path().join("src/b.rs");
        std::fs::create_dir_all(first.parent().expect("source parent")).expect("create source dir");
        std::fs::write(&first, "pub fn generation_marker() {}\n").expect("write first source");
        std::fs::write(&second, "pub fn generation_marker() {}\n").expect("write second source");
        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&first, b"pub fn generation_marker() {}\n");
        index.index_file(&second, b"pub fn generation_marker() {}\n");
        index.ready = true;
        *ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        *ctx.semantic_index_status()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;

        let before = response_value(handle_semantic_search(
            &semantic_page_request("generation_marker", 1, 0),
            &ctx,
        ));
        assert!(before["results"][0]["file"]
            .as_str()
            .expect("first result path")
            .ends_with("src/a.rs"));

        std::fs::write(&first, "pub fn unrelated() {}\n").expect("edit first source");
        ctx.search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .expect("installed search index")
            .update_file(&first);
        ctx.note_search_index_rx_generation(1);

        let after = response_value(handle_semantic_search(
            &semantic_page_request("generation_marker", 1, 0),
            &ctx,
        ));
        assert!(after["results"][0]["file"]
            .as_str()
            .expect("updated result path")
            .ends_with("src/b.rs"));
        assert_eq!(ctx.search_exact_memo().verifier_call_count(), 2);
    }

    #[test]
    fn contended_search_index_degrades_with_disclosure() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source_file, "pub fn contention_marker() {}\n").expect("write source");
        let ctx = test_context(project.path());
        let writer_guard = ctx
            .search_index()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let started = std::time::Instant::now();
        let response = response_value(handle_semantic_search(
            &semantic_request("contention_marker", 5),
            &ctx,
        ));
        drop(writer_guard);

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "contended search exceeded bounded response time"
        );
        assert_eq!(response["success"], true);
        assert_eq!(response["lexical_only_fallback"], true);
        assert!(response["text"]
            .as_str()
            .expect("fallback text")
            .contains("artifact contention"));
    }

    #[test]
    fn view_semantic_reader_scores_vectors_from_manifest_blobs() {
        let project = tempfile::tempdir().expect("project");
        let storage = tempfile::tempdir().expect("storage");
        std::fs::write(project.path().join("lib.rs"), "pub fn needle() {}\n").unwrap();
        let mut store = crate::blob_store::BlobStore::open(
            storage.path(),
            "semantic-reader-family",
            crate::blob_store::BlobPlane::Semantic,
        )
        .unwrap();
        let key = crate::blob_store::SemanticKey::for_current(
            b"pub fn needle() {}\n",
            b"lib.rs",
            "fingerprint",
        )
        .full_key();
        let mut payload = vec![1];
        let push = |payload: &mut Vec<u8>, bytes: &[u8]| {
            payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            payload.extend_from_slice(bytes);
        };
        push(&mut payload, b"semantic-v1");
        push(&mut payload, b"semantic-v1");
        push(&mut payload, b"fingerprint");
        payload.extend_from_slice(&1_u32.to_le_bytes());
        push(&mut payload, b"needle");
        push(&mut payload, b"");
        payload.push(0);
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.push(1);
        push(&mut payload, b"pub fn needle() {}");
        push(&mut payload, b"needle function");
        let vector = [1.0_f32, 0.0_f32]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>();
        push(&mut payload, &vector);
        store.put(&key, &payload).unwrap();
        let manifest = crate::views::Manifest::new([(
            crate::views::RelPath::new(b"lib.rs".to_vec()).unwrap(),
            crate::views::ManifestEntry::Regular {
                mode: 0o100644,
                planes: crate::views::RegularPlanes {
                    semantic: Some(key.to_hex()),
                    callgraph: None,
                },
                resolution_input: false,
            },
        )])
        .unwrap();
        let view = crate::context::ViewRuntimeSnapshot {
            storage: storage.path().to_path_buf(),
            family: "semantic-reader-family".to_string(),
            scope: "semantic-reader-view".to_string(),
            view_dir: storage.path().join("views/semantic-reader-view"),
            generation: Some("1-head".to_string()),
            manifest: Some(manifest),
            pending_paths: Default::default(),
        };

        let results = view_semantic_search(&view, project.path(), &[1.0, 0.0], 5, true).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "needle");
        assert_eq!(results[0].score, 1.0);
    }

    #[test]
    fn contended_callgraph_receiver_does_not_block_search_formatting() {
        let project = tempfile::tempdir().expect("create project dir");
        let ctx = test_context(project.path());
        let receiver_guard = ctx.callgraph_store_rx().lock();
        let started = std::time::Instant::now();
        assert!(warm_callgraph_store(&ctx).is_none());
        assert!(
            started.elapsed() < INTERACTIVE_ARTIFACT_READ_BUDGET,
            "callgraph receiver contention exceeded artifact budget"
        );
        drop(receiver_guard);
    }
}
