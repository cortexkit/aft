//! `gather` reads one pinned `CallGraphStore` snapshot and at most 40 neighbor nodes.
//! `gather` adds test-only nodes only when requested. Source trees live for one walk.
//! A `GatherGap` names its seed and does not discard other seeds.

mod decision;
mod graph;
mod walk;

pub(super) use decision::{
    deterministic_decision, validate_decision, Decision, DecisionError, DecisionInput,
    DecisionModel,
};
pub(super) type GatherSettings = walk::GatherSettings;
pub(super) type GatherWalk = walk::GatherWalk;
pub(super) type GatherWalkRequest<'a> = walk::GatherWalkRequest<'a>;

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

pub(super) fn resolve_containing_symbol(
    file: &Path,
    line: u32,
    ctx: &crate::context::AppContext,
) -> Option<(String, u32)> {
    graph::resolve_containing_symbol(file, line, ctx)
}

pub(super) fn resolve_seeds(
    ctx: &crate::context::AppContext,
    root: &Path,
    hits: &[(PathBuf, u32)],
    include_tests: bool,
) -> Vec<GatherNode> {
    graph::resolve_seeds(ctx, root, hits, include_tests)
}

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
