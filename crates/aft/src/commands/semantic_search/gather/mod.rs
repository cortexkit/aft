//! `gather` reads one pinned `CallGraphStore` snapshot and at most 40 neighbor nodes.
//! `gather` adds test-only nodes only when requested. Source trees live for one walk.
//! A `GatherGap` names its seed and does not discard other seeds.

mod decision;
mod endpoint;
mod graph;
mod render;
mod walk;

pub(super) use decision::{
    deterministic_decision, validate_decision, Decision, DecisionError, DecisionInput,
    DecisionModel,
};
pub(super) type GatherSettings = walk::GatherSettings;
pub(super) type GatherWalk = walk::GatherWalk;
pub(super) type GatherWalkRequest<'a> = walk::GatherWalkRequest<'a>;

pub(super) fn attach(
    req: &crate::protocol::RawRequest,
    ctx: &crate::context::AppContext,
    response: &mut crate::protocol::Response,
    selected_root: &Path,
) {
    let config = ctx.config();
    let query = req
        .params
        .get("query")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !response.success
        || query.trim().is_empty()
        || !crate::config::gather_enabled(
            config
                .search
                .gather
                .as_ref()
                .and_then(|gather| gather.enabled),
        )
        || !super::rerank::is_prose_query(query)
    {
        return;
    }
    let budget = render::available_bytes(response).min(
        GatherSettings::resolve(&config.search)
            .map_or(12 * 1024, |settings| settings.max_output_bytes),
    );
    if budget == 0 {
        return;
    }
    if selected_root.canonicalize().ok()
        != crate::grep_executor::project_root(ctx).canonicalize().ok()
    {
        render::append_state(response, "root_mismatch", budget);
        return;
    }
    let context =
        crate::search_context::SearchContext::from_request_field(req.params.get("search_context"))
            .unwrap_or_default();
    let trace = context
        .as_ref()
        .map(crate::search_context::SearchContext::as_str);
    let has_trace = trace.is_some_and(|text| !text.trim().is_empty());
    let include_tests = req
        .params
        .get("include_tests")
        .or_else(|| req.params.get("includeTests"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let hits: Vec<_> = response
        .data
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hit| {
            let path = hit.get("file")?.as_str()?;
            let line = hit
                .get("start_line")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| hit.get("line").and_then(serde_json::Value::as_u64))?;
            Some((PathBuf::from(path), u32::try_from(line).ok()?))
        })
        .collect();
    let reranker = match super::rerank::installed_backend(ctx) {
        super::rerank::slot::Installed::Ready(selected) => Some(selected.backend),
        _ => None,
    };
    let result = with_graph_snapshot(ctx, selected_root, |snapshot| {
        let seeds =
            graph::resolve_seeds_for_request(ctx, selected_root, &hits, include_tests, Some(req));
        if seeds.is_empty() {
            return Err("no_seeds".into());
        }
        gather_walk(&GatherWalkRequest {
            ctx,
            snapshot,
            seeds: &seeds,
            search: &config.search,
            session_id: Some(req.session()),
            request: Some(req),
            trace,
            query,
            include_tests,
            model: None,
            reranker: reranker.as_deref(),
            cancelled: None,
        })
        .map_err(str::to_owned)
    });
    match result {
        Ok(walk) => render::append_walk(response, walk, budget, has_trace),
        Err(state) => render::append_state(response, &state, budget),
    }
}

pub(super) fn gather_walk(request: &GatherWalkRequest<'_>) -> Result<GatherWalk, &'static str> {
    walk::gather_walk(request)
}

pub(super) fn parse_decision(
    json: &str,
    frontier_ids: &[String],
) -> Result<Decision, DecisionError> {
    decision::parse_decision(json, frontier_ids)
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(super) const DEFAULT_BODY_LINE_BUDGET: usize = 80;
pub(super) const DEFAULT_BODY_BYTE_BUDGET: usize = 4096;
/// Limit graph work and the symbol evidence sent in one decision request.
pub(super) const MAX_FRONTIER: usize = 40;
pub(super) type GatherSourceCache = graph::GatherSourceCache;
pub(super) type GatherGraph = graph::GatherGraph;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GatherGap {
    pub seed_id: String,
    pub state: &'static str,
}

pub(super) fn with_graph_snapshot<T>(
    ctx: &crate::context::AppContext,
    root: &Path,
    read: impl FnOnce(&crate::callgraph_store::CallGraphStore) -> Result<T, String>,
) -> Result<T, String> {
    graph::with_graph_snapshot(ctx, root, read)
}

#[cfg(test)]
pub(super) fn resolve_containing_symbol(
    file: &Path,
    line: u32,
    ctx: &crate::context::AppContext,
) -> Option<(String, u32)> {
    graph::resolve_containing_symbol(file, line, ctx)
}

#[cfg(test)]
pub(super) fn resolve_seeds(
    ctx: &crate::context::AppContext,
    root: &Path,
    hits: &[(PathBuf, u32)],
    include_tests: bool,
) -> Vec<GatherNode> {
    graph::resolve_seeds(ctx, root, hits, include_tests)
}

#[cfg(test)]
pub(super) fn render_node(
    ctx: &crate::context::AppContext,
    root: &Path,
    path: &Path,
    line: u32,
    line_budget: usize,
    include_tests: bool,
) -> Option<GatherNode> {
    graph::render_node(ctx, root, path, line, line_budget, include_tests)
}

#[cfg(test)]
pub(super) fn collect_callgraph_neighbors(
    ctx: &crate::context::AppContext,
    snapshot: &crate::callgraph_store::CallGraphStore,
    seeds: &[GatherNode],
    include_tests: bool,
) -> Result<Vec<GatherNode>, String> {
    Ok(collect_callgraph_neighbors_with_cache(
        ctx,
        snapshot,
        seeds,
        include_tests,
        &mut GatherSourceCache::default(),
    )?
    .nodes)
}

pub(super) fn collect_callgraph_neighbors_with_cache(
    ctx: &crate::context::AppContext,
    snapshot: &crate::callgraph_store::CallGraphStore,
    seeds: &[GatherNode],
    include_tests: bool,
    sources: &mut GatherSourceCache,
) -> Result<GatherGraph, String> {
    graph::collect_callgraph_neighbors(ctx, snapshot, seeds, include_tests, sources)
}

#[derive(Debug, Clone)]
pub(super) struct GatherNode {
    pub id: String,
    pub path: PathBuf,
    pub line: u32,
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub docs: Vec<String>,
    pub body: String,
    pub edge: String,
    pub seed_ordinal: usize,
    pub hop_distance: usize,
    pub rerank_score: Option<f32>,
}

#[derive(Default)]
pub(crate) struct GatherRuntime {
    neighbors: Mutex<NeighborCache>,
    decisions: Mutex<decision::DecisionCache>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GraphIdentity {
    root: PathBuf,
    generation: String,
    revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Direction {
    Caller,
    Callee,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NeighborKey {
    identity: GraphIdentity,
    node_id: String,
    direction: Direction,
    include_tests: bool,
}

#[derive(Clone)]
struct NeighborLocation {
    path: PathBuf,
    line: u32,
}

#[derive(Default)]
struct NeighborCache {
    identity: Option<GraphIdentity>,
    entries: HashMap<NeighborKey, Vec<NeighborLocation>>,
}

#[cfg(test)]
mod tests;
