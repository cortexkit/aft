mod graph;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub(super) use graph::{
    collect_callgraph_neighbors, render_node, resolve_containing_symbol, resolve_seeds,
    with_graph_snapshot, DEFAULT_BODY_BYTE_BUDGET, DEFAULT_BODY_LINE_BUDGET,
};

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
