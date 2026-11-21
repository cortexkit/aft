use super::*;
use crate::callgraph_store::{
    Result as CallGraphResult, StoreCallersResult as RawCallersResult,
    StoreImpactResult as RawImpactResult, StoredEdge,
};
use std::cell::RefCell;

pub(super) struct CountingStore {
    pub(super) root: PathBuf,
    pub(super) sqlite_path: PathBuf,
    pub(super) nodes: HashMap<(String, String), StoreNode>,
    pub(super) callers: HashMap<(String, String), Vec<StoreCallSite>>,
    pub(super) outgoing: HashMap<(String, String), Vec<StoreCallSite>>,
    pub(super) caller_queries: RefCell<HashMap<(String, String), usize>>,
    pub(super) caller_frontier_query_count: RefCell<usize>,
    pub(super) caller_frontier_target_count: RefCell<usize>,
    pub(super) forward_query_count: RefCell<usize>,
    pub(super) frontier_query_count: RefCell<usize>,
    pub(super) caller_count_queries: RefCell<usize>,
    pub(super) caller_count_targets: RefCell<usize>,
    pub(super) caller_count_identities: RefCell<Vec<(String, String)>>,
    pub(super) node_lookups: RefCell<usize>,
    pub(super) stored_hashes: HashMap<String, String>,
}

impl CountingStore {
    pub(super) fn new() -> Self {
        Self {
            root: PathBuf::from("/repo"),
            sqlite_path: PathBuf::from("/repo/callgraph.sqlite"),
            nodes: HashMap::new(),
            callers: HashMap::new(),
            outgoing: HashMap::new(),
            caller_queries: RefCell::new(HashMap::new()),
            caller_frontier_query_count: RefCell::new(0),
            caller_frontier_target_count: RefCell::new(0),
            forward_query_count: RefCell::new(0),
            frontier_query_count: RefCell::new(0),
            caller_count_queries: RefCell::new(0),
            caller_count_targets: RefCell::new(0),
            caller_count_identities: RefCell::new(Vec::new()),
            node_lookups: RefCell::new(0),
            stored_hashes: HashMap::new(),
        }
    }

    pub(super) fn add_node(&mut self, node: StoreNode) {
        self.nodes
            .insert((node.file.clone(), node.symbol.clone()), node);
    }

    pub(super) fn add_caller(&mut self, target: &StoreNode, caller: &StoreNode) {
        self.add_caller_at(target, caller, caller.line);
    }

    pub(super) fn add_caller_at(&mut self, target: &StoreNode, caller: &StoreNode, line: u32) {
        self.callers
            .entry((target.file.clone(), target.symbol.clone()))
            .or_default()
            .push(StoreCallSite {
                kind: "call".to_string(),
                caller_is_test: Some(false),
                caller: caller.clone(),
                target_file: target.file.clone(),
                target_symbol: target.symbol.clone(),
                target: Some(target.clone()),
                line,
                byte_start: 0,
                byte_end: 1,
                resolved: true,
                provenance: TRACE_DATA_RESOLVER_PROVENANCE.to_string(),
            });
    }

    pub(super) fn add_outgoing(&mut self, caller: &StoreNode, target: &StoreNode) {
        self.outgoing
            .entry((caller.file.clone(), caller.symbol.clone()))
            .or_default()
            .push(StoreCallSite {
                kind: "call".to_string(),
                caller_is_test: Some(false),
                caller: caller.clone(),
                target_file: target.file.clone(),
                target_symbol: target.symbol.clone(),
                target: Some(target.clone()),
                line: target.line,
                byte_start: 0,
                byte_end: 1,
                resolved: true,
                provenance: TRACE_DATA_RESOLVER_PROVENANCE.to_string(),
            });
    }

    pub(super) fn total_forward_queries(&self) -> usize {
        *self.forward_query_count.borrow()
    }

    pub(super) fn reset_forward_queries(&self) {
        *self.forward_query_count.borrow_mut() = 0;
    }

    pub(super) fn total_frontier_queries(&self) -> usize {
        *self.frontier_query_count.borrow()
    }

    pub(super) fn total_caller_queries(&self) -> usize {
        self.caller_queries.borrow().values().sum()
    }

    pub(super) fn total_caller_frontier_queries(&self) -> usize {
        *self.caller_frontier_query_count.borrow()
    }

    pub(super) fn caller_frontier_target_count(&self) -> usize {
        *self.caller_frontier_target_count.borrow()
    }

    pub(super) fn total_caller_count_queries(&self) -> usize {
        *self.caller_count_queries.borrow()
    }

    pub(super) fn caller_count_target_count(&self) -> usize {
        *self.caller_count_targets.borrow()
    }

    pub(super) fn reset_query_counts(&self) {
        self.caller_queries.borrow_mut().clear();
        *self.caller_frontier_query_count.borrow_mut() = 0;
        *self.caller_frontier_target_count.borrow_mut() = 0;
        *self.caller_count_queries.borrow_mut() = 0;
        *self.caller_count_targets.borrow_mut() = 0;
        self.caller_count_identities.borrow_mut().clear();
    }
}

impl CallGraphRead for CountingStore {
    fn direct_callers_bounded(
        &self,
        targets: &[(String, String)],
        limit: usize,
    ) -> CallGraphResult<(HashMap<(String, String), Vec<StoreCallSite>>, bool, usize)> {
        *self.caller_frontier_query_count.borrow_mut() += 1;
        *self.caller_frontier_target_count.borrow_mut() += targets.len();
        let mut result: HashMap<_, Vec<_>> = targets
            .iter()
            .cloned()
            .map(|key| (key, Vec::new()))
            .collect();
        let mut examined = 0;
        for key in targets.iter().collect::<BTreeSet<_>>() {
            for site in self.callers.get(key).into_iter().flatten() {
                if examined == limit {
                    return Ok((result, true, examined));
                }
                examined += 1;
                result.get_mut(key).unwrap().push(site.clone());
            }
        }
        Ok((result, false, examined))
    }

    fn project_root(&self) -> &Path {
        &self.root
    }

    fn project_key(&self) -> &str {
        "test-project"
    }

    fn sqlite_path(&self) -> &Path {
        &self.sqlite_path
    }

    fn is_current(&self) -> bool {
        true
    }

    fn edge_snapshot(&self) -> CallGraphResult<BTreeSet<StoredEdge>> {
        unreachable!("not used by trace_to_result")
    }

    fn indexed_file_count(&self) -> CallGraphResult<usize> {
        Ok(self.nodes.len())
    }

    fn node_for(&self, file_rel: &Path, symbol: &str) -> CallGraphResult<StoreNode> {
        Ok(self
            .nodes_for(file_rel, symbol)?
            .into_iter()
            .next()
            .expect("fixture node"))
    }

    fn nodes_for(&self, file_rel: &Path, symbol: &str) -> CallGraphResult<Vec<StoreNode>> {
        *self.node_lookups.borrow_mut() += 1;
        let key = (
            file_rel.to_string_lossy().replace('\\', "/"),
            symbol.to_string(),
        );
        Ok(self.nodes.get(&key).cloned().into_iter().collect())
    }

    fn stored_content_hash(&self, file: &str) -> CallGraphResult<Option<String>> {
        Ok(self.stored_hashes.get(file).cloned())
    }

    fn nodes_matching(&self, symbol: &str) -> CallGraphResult<Vec<StoreNode>> {
        Ok(self
            .nodes
            .values()
            .filter(|node| node.symbol == symbol)
            .cloned()
            .collect())
    }

    fn direct_callers_of(
        &self,
        file_rel: &Path,
        symbol: &str,
    ) -> CallGraphResult<Vec<StoreCallSite>> {
        let key = (
            file_rel.to_string_lossy().replace('\\', "/"),
            symbol.to_string(),
        );
        *self
            .caller_queries
            .borrow_mut()
            .entry(key.clone())
            .or_default() += 1;
        Ok(self.callers.get(&key).cloned().unwrap_or_default())
    }

    fn direct_callers_for_symbols(
        &self,
        targets: &[(String, String)],
    ) -> CallGraphResult<HashMap<(String, String), Vec<StoreCallSite>>> {
        *self.caller_frontier_query_count.borrow_mut() += 1;
        *self.caller_frontier_target_count.borrow_mut() += targets.len();
        Ok(targets
            .iter()
            .cloned()
            .map(|target| {
                let callers = self.callers.get(&target).cloned().unwrap_or_default();
                (target, callers)
            })
            .collect())
    }

    fn direct_caller_counts_of(
        &self,
        targets: &[(String, String)],
    ) -> CallGraphResult<HashMap<(String, String), usize>> {
        *self.caller_count_queries.borrow_mut() += 1;
        *self.caller_count_targets.borrow_mut() = targets.len();
        self.caller_count_identities
            .borrow_mut()
            .extend_from_slice(targets);
        Ok(targets
            .iter()
            .cloned()
            .map(|target| {
                let count = self
                    .callers
                    .get(&target)
                    .cloned()
                    .map(dedup_call_site_count)
                    .unwrap_or_default();
                (target, count)
            })
            .collect())
    }

    fn callers_of(
        &self,
        _file_rel: &Path,
        _symbol: &str,
        _depth: usize,
    ) -> CallGraphResult<RawCallersResult> {
        unreachable!("not used by trace_to_result")
    }

    fn impact_of(
        &self,
        _file_rel: &Path,
        _symbol: &str,
        _depth: usize,
    ) -> CallGraphResult<RawImpactResult> {
        unreachable!("not used by trace_to_result")
    }

    fn outgoing_calls_of(&self, node: &StoreNode) -> CallGraphResult<Vec<StoreCallSite>> {
        *self.forward_query_count.borrow_mut() += 1;
        Ok(self
            .outgoing
            .get(&(node.file.clone(), node.symbol.clone()))
            .cloned()
            .unwrap_or_default())
    }

    fn outgoing_calls_for_symbols(
        &self,
        sources: &[(String, String)],
    ) -> CallGraphResult<HashMap<(String, String), Vec<StoreCallSite>>> {
        *self.frontier_query_count.borrow_mut() += 1;
        Ok(sources
            .iter()
            .cloned()
            .map(|source| {
                let calls = self.outgoing.get(&source).cloned().unwrap_or_default();
                (source, calls)
            })
            .collect())
    }

    fn resolved_self_calls_of(&self, _node: &StoreNode) -> CallGraphResult<Vec<StoreCallSite>> {
        Ok(Vec::new())
    }

    fn unresolved_calls_of(&self, _node: &StoreNode) -> CallGraphResult<Vec<StoreUnresolvedCall>> {
        *self.forward_query_count.borrow_mut() += 1;
        Ok(Vec::new())
    }

    fn call_tree(
        &self,
        _file_rel: &Path,
        _symbol: &str,
        _depth: usize,
    ) -> CallGraphResult<callgraph::CallTreeNode> {
        unreachable!("not used by trace_to_result")
    }

    fn trace_to(
        &self,
        _file_rel: &Path,
        _symbol: &str,
        _max_depth: usize,
    ) -> CallGraphResult<callgraph::TraceToResult> {
        unreachable!("not used by trace_to_result")
    }

    fn trace_to_symbol_candidates(
        &self,
        _to_symbol: &str,
    ) -> CallGraphResult<Vec<TraceToSymbolCandidate>> {
        unreachable!("not used by trace_to_result")
    }

    fn trace_to_symbol(
        &self,
        _file_rel: &Path,
        _symbol: &str,
        _to_symbol: &str,
        _to_file: Option<&Path>,
        _max_depth: usize,
    ) -> CallGraphResult<callgraph::TraceToSymbolResult> {
        unreachable!("not used by trace_to_result")
    }
}

pub(super) fn node(symbol: &str, is_entry_point: bool) -> StoreNode {
    StoreNode::for_test(&format!("{symbol}.ts"), symbol, is_entry_point)
}

pub const IMPACT_BUDGET_NOTE_FOR_TEST: &str = IMPACT_BUDGET_NOTE;

pub fn impact_budget_fixture() -> (impl CallGraphRead, StoreNode) {
    layered_store(4, 3)
}

pub fn impact_result_with_budget_for_test(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
    budget: usize,
) -> StoreAdapterResult<(StoreImpactResult, usize)> {
    impact_result_with_budget(store, file, symbol, depth, include_tests, budget)
}

pub(super) fn layered_store(width: usize, layers: usize) -> (CountingStore, StoreNode) {
    let mut store = CountingStore::new();
    let target = node("target", false);
    store.add_node(target.clone());
    let mut previous = vec![target.clone()];
    for layer in 1..=layers {
        let current = (0..width)
            .map(|index| node(&format!("layer_{layer}_{index}"), layer == layers))
            .collect::<Vec<_>>();
        for caller in &current {
            store.add_node(caller.clone());
        }
        for target_node in &previous {
            for caller in &current {
                store.add_caller(target_node, caller);
            }
        }
        previous = current;
    }
    (store, target)
}
